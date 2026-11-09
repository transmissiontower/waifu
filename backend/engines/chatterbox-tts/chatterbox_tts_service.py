#!/usr/bin/env python3
"""
Chatterbox-Turbo gRPC service implementing the Waifu `Tts` contract (backend/crates/contracts/proto/tts.proto).

* gRPC server is up instantly; the model loads + warms up in a background thread.
* Voice = voices/<name>/ref.wav  (clean single-speaker clip, MORE than 5 s, ideally 8-12 s, lively tone).
  No ref.wav -> the model's built-in voice.
* Long sentences are split into clauses and each clause is streamed as soon as it is generated.
* Latency: voices are prepared once and cached (and preloaded at start-up, so the first reply never pays for it),
  the first clause after an idle gap is kept SHORT so audio starts after a few words instead of a whole sentence,
  requests are served in FIFO order and abandoned (barge-in) requests stop before the next clause.
* Prints `@@loading <what>` / `@@ready` lines on stdout: the supervisor turns them into the loading screen.
Install (own venv, Python 3.11):  see docs/CUSTOM_SERVICE.md  ("Chatterbox")
"""
import argparse
import os
import sys

_ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))  # project root
_VENV = os.path.join(_ROOT, ".venv-chatterbox")
for _py in (os.path.join(_VENV, "bin", "python"), os.path.join(_VENV, "Scripts", "python.exe")):
    if os.path.isfile(_py) and os.path.abspath(sys.prefix) != _VENV and not os.environ.get("CB_REEXEC"):
        os.environ["CB_REEXEC"] = "1"
        os.execv(_py, [_py] + sys.argv)

try:
    import grpc  # noqa: F401
    import grpc_tools  # noqa: F401
except ImportError as _e:
    sys.exit(f"[chatterbox] missing dependency ({_e}) in interpreter {sys.executable}\n"
             f"[chatterbox] fix: {sys.executable} -m pip install grpcio grpcio-tools")

os.environ.setdefault("PYTORCH_ENABLE_MPS_FALLBACK", "1")
import threading
import time
from concurrent import futures

HERE = os.path.dirname(os.path.abspath(__file__))
PROTO_DIR = os.path.join(_ROOT, "backend", "crates", "contracts", "proto")
GEN = os.path.join(HERE, "_gen")


def ensure_stubs():
    if not os.path.exists(os.path.join(GEN, "tts_pb2_grpc.py")):
        os.makedirs(GEN, exist_ok=True)
        from grpc_tools import protoc

        rc = protoc.main(["protoc", f"-I{PROTO_DIR}", f"--python_out={GEN}", f"--grpc_python_out={GEN}",
                          os.path.join(PROTO_DIR, "common.proto"), os.path.join(PROTO_DIR, "tts.proto")])
        if rc != 0:
            sys.exit("protoc failed")
    sys.path.insert(0, GEN)


ensure_stubs()
sys.path.insert(0, HERE)
from tts_text import TicketLock, clean_text, plan_clauses  # noqa: E402
import grpc  # noqa: E402
import numpy as np  # noqa: E402
import common_pb2  # noqa: E402
import tts_pb2  # noqa: E402
import tts_pb2_grpc  # noqa: E402


def log(*a):
    print("[chatterbox]", *a, flush=True)


def mark(kind: str, detail: str = ""):
    """Status line for the supervisor / loading screen (`@@loading ...`, `@@ready`, `@@failed ...`)."""
    print(f"@@{kind} {detail}".rstrip(), flush=True)


class Engine:
    def __init__(self, a):
        self.a = a
        self.ready = threading.Event()
        self.lock = TicketLock()  # FIFO: sentences stay in order, nobody starves
        self.error = None
        self.cur_voice = None
        self.builtin = None
        self.conds = {}  # voice name -> prepared conditionals (prepare_conditionals is slow: do it once per voice)
        self.last_end = 0.0  # monotonic time the last synthesis finished (idle detection for the short first clause)

    def load(self):
        try:
            t0 = time.time()
            mark("loading", "loading the voice model (Chatterbox Turbo)")
            import torch

            self.torch = torch
            if self.a.device != "auto":
                device = self.a.device
            elif torch.cuda.is_available():
                device = "cuda"
            elif getattr(torch.backends, "mps", None) and torch.backends.mps.is_available():
                device = "mps"
            else:
                device = "cpu"
            # checkpoints were saved on CUDA: map them to the device we actually have
            _orig = torch.load

            def _patched(*args, **kwargs):
                kwargs.setdefault("map_location", torch.device(device))
                return _orig(*args, **kwargs)

            torch.load = _patched
            from chatterbox.tts_turbo import ChatterboxTurboTTS

            self.model = ChatterboxTurboTTS.from_pretrained(device=device)
            self.sr = self.model.sr
            self.builtin = getattr(self.model, "conds", None)
            self.cur_voice = "" if self.builtin is not None else None
            log(f"Chatterbox Turbo loaded on {device} in {time.time() - t0:.1f}s")
            self.preload_voices()
            mark("loading", "warming up the voice engine")
            for _ in range(2):
                for _c in self._generate(self.a.default_voice, "Warming up the voice engine.", 1.0):
                    pass
            self.last_end = 0.0
            log(f"ready ({time.time() - t0:.1f}s total)")
            mark("ready")
        except Exception as e:  # noqa: BLE001
            self.error = f"{type(e).__name__}: {e}"
            log("FAILED to start:", self.error)
            import traceback; traceback.print_exc()
            mark("failed", self.error)
        finally:
            self.ready.set()

    def preload_voices(self, limit: int = 8):
        """Prepare every voice that has a ref.wav now, so the first reply in that voice is not delayed by it."""
        names = [n for n in self.list_voices() if n != "default"][:limit]
        for n in names:
            t0 = time.time()
            mark("loading", f"preparing voice '{n}'")
            try:
                self._conds_for(n)
                log(f"voice '{n}' prepared in {time.time() - t0:.1f}s")
            except Exception as e:  # noqa: BLE001
                log(f"voice '{n}' could not be prepared: {e!r}")

    def _conds_for(self, key: str):
        """Conditionals for a voice, prepared once and cached (re-prepared if its ref.wav changes on disk)."""
        wav = os.path.join(self.a.voices_dir, key, "ref.wav")
        stamp = os.path.getmtime(wav) if os.path.isfile(wav) else None
        hit = self.conds.get(key)
        if hit is not None and hit[0] == stamp:
            return hit[1]
        if stamp is not None:
            self.model.prepare_conditionals(wav)
            c = self.model.conds
            log(f"voice '{key}' loaded from ref.wav")
        elif self.builtin is not None:
            c = self.builtin
            if key not in ("", "default"):
                log(f"voice '{key}' has no ref.wav -> built-in voice")
        else:
            raise RuntimeError(f"voice '{key}' has no ref.wav and the model has no built-in voice")
        self.conds[key] = (stamp, c)
        return c

    def _select_voice(self, name: str):
        key = name or self.a.default_voice
        if key == self.cur_voice:
            return
        self.model.conds = self._conds_for(key)
        self.cur_voice = key

    def _generate(self, voice: str, text: str, speed: float, alive=lambda: True, first_chars: int = 0):
        """Yield one float32 waveform per clause. `alive` is polled before each clause (barge-in), so a request the
        client has already abandoned does not keep the GPU busy."""
        self._select_voice(voice)
        for clause in plan_clauses(text, self.a.clause_chars, first_chars):
            if not alive():
                return
            with self.torch.inference_mode():
                wav = self.model.generate(clause)
            yield wav.squeeze().detach().cpu().numpy().astype(np.float32)

    def list_voices(self):
        out = ["default"]
        if os.path.isdir(self.a.voices_dir):
            for n in sorted(os.listdir(self.a.voices_dir)):
                if os.path.isfile(os.path.join(self.a.voices_dir, n, "ref.wav")):
                    out.append(n)
        return out


class Svc(tts_pb2_grpc.TtsServicer):
    def __init__(self, eng: Engine):
        self.e = eng

    def Synthesize(self, request, context):
        e = self.e
        if not e.ready.wait(timeout=180) or e.error:
            context.abort(grpc.StatusCode.UNAVAILABLE, f"Chatterbox not ready: {e.error or 'still loading'}")
        text = clean_text(request.text)
        if not text:
            yield common_pb2.AudioChunk(pcm16=b"", sample_rate=e.sr, channels=1, seq=0, last=True)
            return
        t0, seq, first = time.time(), 0, True
        per = e.sr // 8  # ~125 ms chunks
        # After an idle gap this is (probably) the start of a new reply: cut a short head so the first audio is
        # ready after ~a clause instead of a whole sentence. Later sentences of the same reply follow immediately.
        cold = (time.monotonic() - e.last_end) > e.a.idle_s
        first_chars = e.a.first_chars if cold else 0
        try:
            with e.lock:
                if not context.is_active():  # abandoned while queued
                    return
                for f in e._generate(request.voice, text, 1.0, alive=context.is_active, first_chars=first_chars):
                    for i in range(0, len(f), per):
                        if not context.is_active():  # barge-in
                            return
                        pcm = (np.clip(f[i:i + per], -1.0, 1.0) * 32767).astype("<i2").tobytes()
                        if first:
                            first = False
                            log(f"first audio {int((time.time() - t0) * 1000)} ms for {len(text)} chars"
                                f"{' (short first clause)' if first_chars else ''}")
                        yield common_pb2.AudioChunk(pcm16=pcm, sample_rate=e.sr, channels=1, seq=seq, last=False)
                        seq += 1
        except Exception as ex:  # noqa: BLE001
            log("synthesis error:", repr(ex))
            context.abort(grpc.StatusCode.UNAVAILABLE, f"Chatterbox synthesis failed: {ex}")
        finally:
            e.last_end = time.monotonic()
        yield common_pb2.AudioChunk(pcm16=b"", sample_rate=e.sr, channels=1, seq=seq, last=True)

    def ListVoices(self, request, context):
        return tts_pb2.VoiceList(voices=self.e.list_voices(), default_voice=self.e.a.default_voice, engine="chatterbox-turbo")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--addr", default=os.environ.get("WAIFU_ADDR", "127.0.0.1:60001"))
    ap.add_argument("--device", default=os.environ.get("CB_DEVICE") or os.environ.get("WAIFU_TTS_DEVICE") or "auto")
    ap.add_argument("--clause-chars", type=int, default=int(os.environ.get("CB_CLAUSE_CHARS", "90")))
    ap.add_argument("--first-chars", type=int, default=int(os.environ.get("CB_FIRST_CHARS", "40")),
                    help="size of the short first clause after an idle gap (0 = off)")
    ap.add_argument("--idle-s", type=float, default=float(os.environ.get("CB_IDLE_S", "1.5")),
                    help="silence of the engine after which the next request counts as the start of a reply")
    ap.add_argument("--voices-dir", default=os.environ.get("CB_VOICES_DIR") or os.environ.get("WAIFU_VOICES_DIR") or os.path.join(_ROOT, "assets", "voices"))
    ap.add_argument("--default-voice", default=os.environ.get("CB_DEFAULT_VOICE", "default"))
    a = ap.parse_args()
    mark("loading", "starting the voice engine")  # before the port opens, so the supervisor never misses it
    eng = Engine(a)
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=8))
    tts_pb2_grpc.add_TtsServicer_to_server(Svc(eng), server)
    server.add_insecure_port(a.addr)
    server.start()
    log(f"listening on {a.addr} (model loads in background)")
    threading.Thread(target=eng.load, daemon=True).start()
    try:
        server.wait_for_termination()
    except KeyboardInterrupt:
        server.stop(grace=2)


if __name__ == "__main__":
    main()
