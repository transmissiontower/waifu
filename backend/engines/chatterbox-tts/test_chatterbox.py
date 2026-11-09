"""Tests for the Chatterbox service's latency logic. No GPU, grpc or torch needed:
`python3 -m unittest discover -s backend/engines/chatterbox-tts -p 'test_*.py'`"""
import contextlib
import importlib
import os
import random
import sys
import tempfile
import threading
import time
import types
import unittest
from types import SimpleNamespace

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from tts_text import TicketLock, clean_text, plan_clauses, split_clauses, split_first  # noqa: E402


# ───────────────────────────── pure helpers ─────────────────────────────
class TextTests(unittest.TestCase):
    def test_clean_text_strips_markdown_and_emoji(self):
        self.assertEqual(clean_text("**Hi**  there_ `x` 😀  you"), "Hi there x you")

    def test_short_text_is_not_split(self):
        self.assertEqual(split_first("Oh, really? That is nice.", 40), ("Oh, really? That is nice.", ""))
        self.assertEqual(split_first("anything at all, even long text here", 0)[1], "")

    def test_head_is_cut_at_the_last_clause_boundary_in_the_window(self):
        text = "Well, I did not expect that from you today, but fine, whatever you say then."
        head, rest = split_first(text, 40)
        self.assertEqual(head, "Well, I did not expect that from you today,")
        self.assertEqual(rest, "but fine, whatever you say then.")

    def test_falls_back_to_a_word_boundary_without_punctuation(self):
        text = "this sentence has no commas whatsoever so it must be cut at a space somewhere reasonable"
        head, rest = split_first(text, 40)
        self.assertTrue(10 <= len(head) <= 44, head)
        self.assertEqual(f"{head} {rest}", text)
        self.assertFalse(head.endswith(" ") or rest.startswith(" "))

    def test_never_leaves_a_tiny_rest_or_tiny_head(self):
        head, rest = split_first("This one is just a little over the limit, ok?", 30)
        self.assertEqual(rest, "")  # "ok?" would be a pointless extra generation
        self.assertEqual(split_first("Oh, " + "x" * 60, 40)[0][:3], "Oh,")  # "Oh," alone is never the head
        h, r = split_first("Oh, " + "word " * 12, 40)
        self.assertGreaterEqual(len(h), 13)

    def test_plan_preserves_all_words_for_random_text(self):
        rnd = random.Random(7)
        words = ["hi", "well,", "okay.", "you're", "really", "tired;", "so", "much", "—", "a", "tiny", "dog:", "right?"]
        for _ in range(300):
            text = " ".join(rnd.choice(words) for _ in range(rnd.randint(1, 40)))
            for first in (0, 20, 40, 70):
                parts = plan_clauses(text, 90, first)
                self.assertEqual(" ".join(" ".join(parts).split()), " ".join(text.split()), (text, first))
                self.assertTrue(all(p.strip() for p in parts))

    def test_first_clause_is_much_shorter_than_the_whole_sentence(self):
        text = "Honestly, I think you are overcomplicating this whole thing, and you know it, don't you."
        self.assertEqual(len(plan_clauses(text, 90, 0)), 1)
        parts = plan_clauses(text, 90, 40)
        self.assertGreater(len(parts), 1)
        self.assertLess(len(parts[0]), len(text) * 0.6)

    def test_split_clauses_short_tail_is_merged_into_the_previous_clause(self):
        # regression: the old `parts[-2] = f"{parts[-2]} {parts.pop()}"` raised IndexError here (exactly 2 parts)
        # and dropped / duplicated text with 3+ parts
        two = "a" * 80 + ", right?"
        self.assertEqual(split_clauses(two, 60), [two])
        three = "a" * 50 + ", " + "b" * 58 + ", ok?"  # 3 parts, the last one short
        self.assertEqual(split_clauses(three, 60), ["a" * 50 + ",", "b" * 58 + ", ok?"])

    def test_split_clauses_unchanged_behaviour(self):
        t = "a" * 50 + ", " + "b" * 50 + ", " + "c" * 5
        self.assertEqual(split_clauses(t, 60), ["a" * 50 + ",", "b" * 50 + ", " + "c" * 5])


class TicketLockTests(unittest.TestCase):
    def test_strict_fifo_order(self):
        lock, order, n = TicketLock(), [], 12
        lock.acquire()  # main holds it while the workers queue up one by one
        threads = []

        def worker(i):
            with lock:
                order.append(i)

        for i in range(n):
            t = threading.Thread(target=worker, args=(i,))
            t.start()
            threads.append(t)
            deadline = time.time() + 2
            while lock._next < i + 2 and time.time() < deadline:  # wait until thread i has taken its ticket
                time.sleep(0.001)
        lock.release()
        for t in threads:
            t.join(2)
        self.assertEqual(order, list(range(n)))

    def test_released_on_exception(self):
        lock = TicketLock()
        with self.assertRaises(ValueError):
            with lock:
                raise ValueError("boom")
        got = []
        t = threading.Thread(target=lambda: (lock.acquire(), got.append(1), lock.release()))
        t.start()
        t.join(1)
        self.assertEqual(got, [1])


# ─────────────────────── the service, with a fake model ───────────────────────
def load_service_module():
    """Import chatterbox_tts_service with grpc / protobuf stubs (they are not installed in CI)."""

    class AudioChunk:
        def __init__(self, **kw):
            self.__dict__.update(kw)

    stubs = {
        "grpc": types.SimpleNamespace(
            StatusCode=types.SimpleNamespace(UNAVAILABLE="UNAVAILABLE"),
            server=lambda *a, **k: None,
        ),
        "grpc_tools": types.ModuleType("grpc_tools"),
        "common_pb2": types.SimpleNamespace(AudioChunk=AudioChunk),
        "tts_pb2": types.SimpleNamespace(VoiceList=lambda **k: SimpleNamespace(**k)),
        "tts_pb2_grpc": types.SimpleNamespace(TtsServicer=object, add_TtsServicer_to_server=lambda *a: None),
    }
    saved = {k: sys.modules.get(k) for k in stubs}
    sys.modules.update(stubs)
    sys.modules.pop("chatterbox_tts_service", None)
    try:
        return importlib.import_module("chatterbox_tts_service")
    finally:
        for k, v in saved.items():
            if v is None:
                sys.modules.pop(k, None)
            else:
                sys.modules[k] = v


svc_mod = load_service_module()


class FakeWav:
    def __init__(self, n):
        self.a = np.full(n, 0.1, dtype=np.float32)

    def squeeze(self):
        return self

    def detach(self):
        return self

    def cpu(self):
        return self

    def numpy(self):
        return self.a


class FakeModel:
    sr = 24000

    def __init__(self):
        self.conds = "BUILTIN"
        self.prepared = []
        self.generated = []

    def prepare_conditionals(self, wav):
        self.prepared.append(wav)
        self.conds = f"COND:{wav}"

    def generate(self, clause):
        self.generated.append((self.conds, clause))
        return FakeWav(len(clause) * 100)


class FakeContext:
    def __init__(self, active_for=None):
        self.n, self.active_for, self.aborted = 0, active_for, None

    def is_active(self):
        self.n += 1
        return self.active_for is None or self.n <= self.active_for

    def abort(self, code, msg):
        self.aborted = (code, msg)
        raise RuntimeError(msg)


def make_engine(voices_dir, **over):
    a = SimpleNamespace(device="cpu", clause_chars=90, first_chars=40, idle_s=1.5, voices_dir=voices_dir,
                        default_voice="default")
    for k, v in over.items():
        setattr(a, k, v)
    e = svc_mod.Engine(a)
    e.model, e.sr, e.torch = FakeModel(), 24000, SimpleNamespace(inference_mode=contextlib.nullcontext)
    e.builtin, e.cur_voice = e.model.conds, ""
    e.ready.set()
    return e


def request(text, voice=""):
    return SimpleNamespace(text=text, voice=voice)


LONG = "Honestly, I think you are overcomplicating this whole thing, and you know it, don't you."


class EngineTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.voices = self.tmp.name
        os.makedirs(os.path.join(self.voices, "alice"))
        with open(os.path.join(self.voices, "alice", "ref.wav"), "wb") as f:
            f.write(b"RIFF")
        self.e = make_engine(self.voices)
        self.svc = svc_mod.Svc(self.e)

    def tearDown(self):
        self.tmp.cleanup()

    def run_synth(self, text, voice="", ctx=None):
        return list(self.svc.Synthesize(request(text, voice), ctx or FakeContext()))

    def test_voice_is_prepared_once_and_cached(self):
        self.run_synth("hello there my friend", "alice")
        self.run_synth("hello again my friend", "alice")
        self.run_synth("builtin voice please", "bob")  # no ref.wav -> built-in
        self.run_synth("back to alice please", "alice")
        self.assertEqual(len(self.e.model.prepared), 1)
        self.assertEqual(self.e.model.generated[2][0], "BUILTIN")
        self.assertTrue(self.e.model.generated[3][0].startswith("COND:"))

    def test_changed_ref_wav_is_picked_up_without_restart(self):
        self.run_synth("one", "alice")
        p = os.path.join(self.voices, "alice", "ref.wav")
        os.utime(p, (time.time() + 50, time.time() + 50))
        self.e.cur_voice = None
        self.run_synth("two", "alice")
        self.assertEqual(len(self.e.model.prepared), 2)

    def test_preload_prepares_every_voice_before_the_first_reply(self):
        self.e.preload_voices()
        self.assertEqual(len(self.e.model.prepared), 1)
        self.run_synth("first reply in alice's voice", "alice")
        self.assertEqual(len(self.e.model.prepared), 1, "first reply must not pay for prepare_conditionals")

    def test_first_sentence_after_idle_starts_with_a_short_clause(self):
        out = self.run_synth(LONG)
        first_clause = self.e.model.generated[0][1]
        self.assertLess(len(first_clause), len(LONG) * 0.6)
        self.assertEqual(" ".join(c for _, c in self.e.model.generated), LONG)
        self.assertTrue(out[-1].last and out[-1].pcm16 == b"")
        self.assertTrue(all(not c.last for c in out[:-1]))

    def test_follow_up_sentences_of_the_same_reply_are_not_chopped(self):
        self.run_synth(LONG)
        before = len(self.e.model.generated)
        self.run_synth(LONG)  # arrives right after the previous one finished -> same reply
        self.assertEqual(len(self.e.model.generated) - before, 1)

    def test_idle_gap_makes_the_next_request_short_again(self):
        self.run_synth(LONG)
        self.e.last_end = time.monotonic() - 10
        before = len(self.e.model.generated)
        self.run_synth(LONG)
        self.assertGreater(len(self.e.model.generated) - before, 1)

    def test_first_chars_zero_disables_the_split(self):
        e = make_engine(self.voices, first_chars=0)
        list(svc_mod.Svc(e).Synthesize(request(LONG), FakeContext()))
        self.assertEqual(len(e.model.generated), 1)

    def test_abandoned_request_stops_before_generating_more_clauses(self):
        ctx = FakeContext(active_for=3)  # barge-in shortly after the first clause started streaming
        out = self.run_synth(LONG + " " + LONG, ctx=ctx)
        self.assertLess(len(self.e.model.generated), 3)
        self.assertFalse(out and out[-1].last and len(self.e.model.generated) > 2)

    def test_request_cancelled_while_queued_never_touches_the_model(self):
        out = self.run_synth("this client already left", ctx=FakeContext(active_for=0))
        self.assertEqual(self.e.model.generated, [])
        self.assertEqual(out, [])

    def test_queued_requests_are_served_in_arrival_order(self):
        order = []
        orig = self.e.model.generate

        def slow(clause):
            time.sleep(0.01)
            order.append(clause)
            return orig(clause)

        self.e.model.generate = slow
        self.e.lock.acquire()  # block the engine so all requests queue
        threads = []
        for i in range(6):
            t = threading.Thread(target=self.run_synth, args=(f"sentence number {i} is here ok",))
            t.start()
            threads.append(t)
            while self.e.lock._next < i + 2:
                time.sleep(0.001)
        self.e.lock.release()
        for t in threads:
            t.join(5)
        self.assertEqual(order, [f"sentence number {i} is here ok" for i in range(6)])

    def test_synthesis_error_aborts_with_unavailable(self):
        self.e.model.generate = lambda clause: (_ for _ in ()).throw(ValueError("gpu on fire"))
        ctx = FakeContext()
        with self.assertRaises(RuntimeError):
            list(self.svc.Synthesize(request("hello there friend"), ctx))
        self.assertEqual(ctx.aborted[0], "UNAVAILABLE")
        self.assertGreater(self.e.last_end, 0)  # bookkeeping still happens


class MarkerTests(unittest.TestCase):
    def test_status_markers_are_single_lines(self):
        import io

        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            svc_mod.mark("loading", "warming up")
            svc_mod.mark("ready")
        self.assertEqual(buf.getvalue().splitlines(), ["@@loading warming up", "@@ready"])

    def test_load_failure_reports_failed_and_unblocks_waiters(self):
        import io

        e = make_engine(tempfile.gettempdir())
        e.ready.clear()
        e.a.device = "does-not-exist"
        buf = io.StringIO()
        sys.modules.setdefault("torch", None)  # forces ImportError inside load() when torch is absent
        with contextlib.redirect_stdout(buf):
            e.load()
        self.assertTrue(e.ready.is_set())
        if e.error:
            self.assertIn("@@failed", buf.getvalue())


if __name__ == "__main__":
    unittest.main(verbosity=2)
