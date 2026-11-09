#!/usr/bin/env python3
"""
Template: a custom TTS service written in Python that implements the Waifu `Tts` contract.

Run standalone:   python tts_service.py --addr 127.0.0.1:60001
Or let the supervisor launch it - in config/services.yaml:

    tts:
      impl: external
      name: my-python-tts
      addr: 127.0.0.1:60001
      command: ["python3", "backend/engines/template-python/tts_service.py", "--addr", "127.0.0.1:60001"]

Replace `synthesize_pcm()` with a call to your engine (ElevenLabs, Coqui, edge-tts, a GPU server...).
Contract: backend/crates/contracts/proto/tts.proto  (stream of PCM16 mono AudioChunk).
The Python stubs are generated automatically on first start (needs: pip install grpcio grpcio-tools).
"""
import argparse
import math
import os
import struct
import sys
import time
from concurrent import futures

HERE = os.path.dirname(os.path.abspath(__file__))
PROTO_DIR = os.path.abspath(os.path.join(HERE, "..", "..", "crates", "contracts", "proto"))
GEN = os.path.join(HERE, "_gen")


def ensure_stubs():
    """Generate Python gRPC stubs from the shared .proto files (once)."""
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
import common_pb2  # noqa: E402
import tts_pb2  # noqa: E402
import tts_pb2_grpc  # noqa: E402

SAMPLE_RATE = 22050


def synthesize_pcm(text: str, speed: float) -> bytes:
    """DEMO ENGINE: a soft two-tone 'blip' per word. Replace with a real TTS engine.
    Must return little-endian PCM16 mono at SAMPLE_RATE."""
    out = bytearray()
    for i, _word in enumerate(text.split()):
        dur = 0.18 / (speed or 1.0)
        f = 300 + (i * 47) % 200
        n = int(SAMPLE_RATE * dur)
        for k in range(n):
            env = math.sin(math.pi * k / n)
            s = 0.3 * env * (math.sin(2 * math.pi * f * k / SAMPLE_RATE) + 0.5 * math.sin(2 * math.pi * 2 * f * k / SAMPLE_RATE))
            out += struct.pack("<h", int(s * 32767 / 1.5))
        out += b"\x00\x00" * int(SAMPLE_RATE * 0.03)
    return bytes(out)


class TtsServicer(tts_pb2_grpc.TtsServicer):
    def Synthesize(self, request, context):
        pcm = synthesize_pcm(request.text, request.speed)
        per = SAMPLE_RATE // 5 * 2  # 200 ms chunks, in bytes
        chunks = [pcm[i:i + per] for i in range(0, len(pcm), per)] or [b""]
        for seq, c in enumerate(chunks):
            if not context.is_active():  # client interrupted (barge-in)
                return
            yield common_pb2.AudioChunk(pcm16=c, sample_rate=SAMPLE_RATE, channels=1, seq=seq, last=seq == len(chunks) - 1)

    def ListVoices(self, request, context):
        return tts_pb2.VoiceList(voices=["demo"], default_voice="demo", engine="python-demo")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--addr", default=os.environ.get("WAIFU_ADDR", "127.0.0.1:60001"))
    a = ap.parse_args()
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=8))
    tts_pb2_grpc.add_TtsServicer_to_server(TtsServicer(), server)
    server.add_insecure_port(a.addr)
    server.start()
    print(f"python tts service listening on {a.addr}", flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        server.stop(grace=2)


if __name__ == "__main__":
    main()
