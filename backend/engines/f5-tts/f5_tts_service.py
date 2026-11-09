#!/usr/bin/env python3
"""
F5-TTS gRPC service implementing the Waifu `Tts` contract (backend/crates/contracts/proto/tts.proto).

Why it is fast:
  * gRPC server is up instantly (supervisor sees it healthy); the model loads + warms up in a
    background thread, requests simply wait for it.
  * model, vocoder and every voice's reference audio are loaded ONCE and kept on the GPU.
  * audio is streamed: long sentences are split into short clauses and each clause is sent as
    soon as it is generated (first audio = time to generate ONE short clause, not the whole text).
  * low NFE step count (default 16 instead of 32) - ~2x faster, minor quality cost.

Voices: voices/<name>/ref.wav (3-12 s clean speech) + voices/<name>/ref.txt (its exact transcript).
If the folder has no ref.wav (e.g. the old Piper `amy`), the sample reference bundled with f5-tts is used.
Install:  pip install -r backend/engines/f5-tts/requirements.txt   (+ a CUDA build of torch for real speed)
"""
import argparse
import os
import sys

# Auto-use the project's venv (<root>/.venv) no matter which `python` the supervisor launched.
_ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))  # project root
for _py in (os.path.join(_ROOT, ".venv", "bin", "python"), os.path.join(_ROOT, ".venv", "Scripts", "python.exe")):
    if os.path.isfile(_py) and os.path.abspath(sys.prefix) != os.path.join(_ROOT, ".venv") and not os.environ.get("F5_REEXEC"):
        os.environ["F5_REEXEC"] = "1"
        os.execv(_py, [_py] + sys.argv)

try:
    import grpc  # noqa: F401
    import grpc_tools  # noqa: F401
except ImportError as _e:
    sys.exit(f"[f5-tts] missing dependency ({_e}) in interpreter {sys.executable}\n"
             f"[f5-tts] fix: {sys.executable} -m pip install torch torchaudio -r backend/engines/f5-tts/requirements.txt")

os.environ.setdefault("PYTORCH_ENABLE_MPS_FALLBACK", "1")  # unsupported ops fall back to CPU on Apple GPUs
import re
import threading
import time
from concurrent import futures

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = _ROOT
PROTO_DIR = os.path.join(ROOT, "backend", "crates", "contracts", "proto")
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
import grpc  # noqa: E402
import numpy as np  # noqa: E402
import common_pb2  # noqa: E402
import tts_pb2  # noqa: E402
import tts_pb2_grpc  # noqa: E402


def log(*a):
    print("[f5-tts]", *a, flush=True)


# ───────────────────────────── text helpers ─────────────────────────────
_NOISE = re.compile(r"[*_`#~]|[\U0001F000-\U0001FFFF\u2600-\u27BF]")


def clean_text(t: str) -> str:
    return " ".join(_NOISE.sub("", t).split())


def split_clauses(text: str, max_chars: int):
    """Short pieces => first audio arrives sooner. Splits on , ; : — after a minimum length."""
    if len(text) <= max_chars:
        return [text]
    parts, cur = [], ""
    for tok in re.split(r"(?<=[,;:—])\s+", text):
        if cur and len(cur) + len(tok) + 1 > max_chars:
            parts.append(cur)
            cur = tok
        else:
            cur = f"{cur} {tok}".strip()
    if cur:
        parts.append(cur)
    # merge a too-short tail so F5 doesn't get 1-2 word inputs (unstable)
    if len(parts) > 1 and len(parts[-1]) < 12:
        parts[-2] = f"{parts[-2]} {parts.pop()}"
    return parts


# ───────────────────────────── engine ─────────────────────────────
class Engine:
    def __init__(self, a):
        self.a = a
        self.ready = threading.Event()
        self.lock = threading.Lock()  # one inference at a time on the GPU
        self.voices = {}  # name -> (audio tensor, sr, ref_text)
        self.error = None

    def load(self):
        try:
            t0 = time.time()
            import torch
            import torchaudio  # noqa: F401
            from f5_tts.api import F5TTS
            from f5_tts.infer import utils_infer

            self.torch = torch
            self.utils = utils_infer
            if self.a.device != "auto":
                device = self.a.device
            elif torch.cuda.is_available():
                device = "cuda"
            elif getattr(torch.backends, "mps", None) and torch.backends.mps.is_available():
                device = "mps"  # Apple Silicon GPU
            else:
                device = "cpu"
            if device == "cpu":
                log("WARNING: no CUDA - F5-TTS on CPU is far from realtime. Install a CUDA build of torch.")
            if device == "cuda":
                torch.backends.cuda.matmul.allow_tf32 = True
                torch.backends.cudnn.allow_tf32 = True
            self.f5 = F5TTS(model=self.a.model, device=device)
            self.sr = self.f5.target_sample_rate
            log(f"model {self.a.model} loaded on {device} in {time.time() - t0:.1f}s")
            # warm-up: compiles kernels, fills caches -> first real reply is fast
            for _ in range(2):
                for _chunk in self._generate(self.a.default_voice, "Warming up the voice engine.", 1.0):
                    pass
            log(f"ready ({time.time() - t0:.1f}s total)")
        except Exception as e:  # noqa: BLE001
            self.error = f"{type(e).__name__}: {e}"
            log("FAILED to start:", self.error)
        finally:
            self.ready.set()

    # voice -> (audio, sr, text), cached
    def voice(self, name: str):
        key = name or self.a.default_voice
        if key in self.voices:
            return self.voices[key]
        import torchaudio
        from importlib.resources import files

        d = os.path.join(self.a.voices_dir, key)
        wav, txt = os.path.join(d, "ref.wav"), os.path.join(d, "ref.txt")
        if os.path.isfile(wav):
            ref_text = open(txt, encoding="utf-8").read().strip() if os.path.isfile(txt) else ""
            ref_path, ref_text = self.utils.preprocess_ref_audio_text(wav, ref_text, show_info=lambda *_: None)
        else:
            if key not in ("", "default"):
                log(f"voice '{key}' has no ref.wav -> using bundled sample reference")
            ref_path = str(files("f5_tts").joinpath("infer/examples/basic/basic_ref_en.wav"))
            ref_text = "Some call me nature, others call me mother nature."
        audio, sr = torchaudio.load(ref_path)
        self.voices[key] = (audio, sr, ref_text)
        return self.voices[key]

    def _generate(self, voice: str, text: str, speed: float):
        """Yields float32 numpy chunks (mono, self.sr) as soon as each clause is ready."""
        audio, sr, ref_text = self.voice(voice)
        batches = split_clauses(text, self.a.clause_chars)
        with self.torch.inference_mode():
            gen = self.utils.infer_batch_process(
                (audio, sr), ref_text, batches, self.f5.ema_model, self.f5.vocoder,
                mel_spec_type=self.f5.mel_spec_type, progress=__import__("tqdm"), target_rms=0.1,
                cross_fade_duration=0.0, nfe_step=self.a.nfe, cfg_strength=self.a.cfg,
                sway_sampling_coef=-1.0, speed=speed, device=self.f5.device, streaming=True,
                chunk_size=self.sr // 8,  # ~125 ms
            )
            for item in gen:
                yield np.asarray(item[0], dtype=np.float32)

    def list_voices(self):
        out = ["default"]
        if os.path.isdir(self.a.voices_dir):
            for n in sorted(os.listdir(self.a.voices_dir)):
                if os.path.isfile(os.path.join(self.a.voices_dir, n, "ref.wav")):
                    out.append(n)
        return out


# ───────────────────────────── gRPC ─────────────────────────────
class Svc(tts_pb2_grpc.TtsServicer):
    def __init__(self, eng: Engine):
        self.e = eng

    def Synthesize(self, request, context):
        e = self.e
        if not e.ready.wait(timeout=120) or e.error:
            context.abort(grpc.StatusCode.UNAVAILABLE, f"F5-TTS not ready: {e.error or 'still loading'}")
        text = clean_text(request.text)
        if not text:
            yield common_pb2.AudioChunk(pcm16=b"", sample_rate=e.sr, channels=1, seq=0, last=True)
            return
        speed = request.speed if request.speed > 0 else 1.0
        t0, seq, first = time.time(), 0, True
        try:
            with e.lock:
                for f in e._generate(request.voice, text, speed):
                    if not context.is_active():  # barge-in: stop generating immediately
                        return
                    pcm = (np.clip(f, -1.0, 1.0) * 32767).astype("<i2").tobytes()
                    if first:
                        first = False
                        log(f"first audio {int((time.time() - t0) * 1000)} ms for {len(text)} chars")
                    yield common_pb2.AudioChunk(pcm16=pcm, sample_rate=e.sr, channels=1, seq=seq, last=False)
                    seq += 1
        except Exception as ex:  # noqa: BLE001
            log("synthesis error:", repr(ex))
            context.abort(grpc.StatusCode.UNAVAILABLE, f"F5-TTS synthesis failed: {ex}")
        yield common_pb2.AudioChunk(pcm16=b"", sample_rate=e.sr, channels=1, seq=seq, last=True)

    def ListVoices(self, request, context):
        return tts_pb2.VoiceList(voices=self.e.list_voices(), default_voice=self.e.a.default_voice, engine="f5-tts")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--addr", default=os.environ.get("WAIFU_ADDR", "127.0.0.1:60001"))
    ap.add_argument("--model", default=os.environ.get("F5_MODEL", "F5TTS_v1_Base"))
    ap.add_argument("--device", default=os.environ.get("F5_DEVICE") or os.environ.get("WAIFU_TTS_DEVICE") or "auto")
    ap.add_argument("--nfe", type=int, default=int(os.environ.get("F5_NFE", "16")), help="diffusion steps: 8-12 fastest, 32 best")
    ap.add_argument("--cfg", type=float, default=float(os.environ.get("F5_CFG", "2.0")))
    ap.add_argument("--clause-chars", type=int, default=int(os.environ.get("F5_CLAUSE_CHARS", "70")))
    ap.add_argument("--voices-dir", default=os.environ.get("F5_VOICES_DIR") or os.environ.get("WAIFU_VOICES_DIR") or os.path.join(ROOT, "assets", "voices"))
    ap.add_argument("--default-voice", default=os.environ.get("F5_DEFAULT_VOICE", "default"))
    a = ap.parse_args()

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
