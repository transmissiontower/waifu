"""Pure helpers for the Chatterbox TTS service (no torch / grpc imports, so they are unit-testable).

* clean_text      - strip markdown / emoji noise
* split_clauses   - cut long text into clauses of at most N chars (at commas etc.)
* split_first     - cut a SHORT head off the text so the first audio can start after generating only a few words
* plan_clauses    - combine the two
* TicketLock      - a FIFO mutex (threading.Lock does not guarantee arrival order)
"""
import re
import threading

_NOISE = re.compile(r"[*_`#~]|[\U0001F000-\U0001FFFF\u2600-\u27BF]")
_CLAUSE_END = re.compile(r"[,;:—]\s")


def clean_text(t: str) -> str:
    return " ".join(_NOISE.sub("", t).split())


def split_clauses(text: str, max_chars: int):
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
    if len(parts) > 1 and len(parts[-1]) < 12:
        # NB: pop first. (`parts[-2] = f"{parts[-2]} {parts.pop()}"` assigns AFTER the pop, i.e. to the wrong
        # element - it dropped text, or raised IndexError for exactly two parts.)
        tail = parts.pop()
        parts[-1] = f"{parts[-1]} {tail}"
    return parts


def split_first(text: str, first_chars: int):
    """Return (head, rest). The head is a short opening chunk (~first_chars) so synthesis of the first audio
    does not have to wait for the whole sentence. rest == "" means: do not split.

    Preference: the clause boundary (, ; : -) closest to first_chars, else - only for a long run-on without
    punctuation - a word boundary. Never produces a tiny head (< first_chars/3) or a tiny rest (< 12 chars):
    a lone "Oh," or "you?" would cost a whole extra generation call and sound choppy."""
    text = text.strip()
    if first_chars <= 0:
        return text, ""
    limit = int(first_chars * 1.35)
    if len(text) <= limit:
        return text, ""
    min_head = max(10, first_chars // 3)
    cut = -1
    for m in _CLAUSE_END.finditer(text[: limit + 2]):
        pos = m.start() + 1  # just after the punctuation mark
        if min_head <= pos <= limit and (cut < 0 or abs(pos - first_chars) < abs(cut - first_chars)):
            cut = pos
    if cut < 0 and len(text) > first_chars * 2:
        cut = text.rfind(" ", min_head, int(first_chars * 1.1) + 1)
    if cut < 0:
        return text, ""
    head, rest = text[:cut].strip(), text[cut:].strip()
    if len(head) < min_head or len(rest) < 12:
        return text, ""
    return head, rest


def plan_clauses(text: str, max_chars: int, first_chars: int = 0):
    if first_chars > 0:
        head, rest = split_first(text, first_chars)
        if rest:
            return [head] + split_clauses(rest, max_chars)
    return split_clauses(text, max_chars)


class TicketLock:
    """FIFO mutex. Waiting threads are served strictly in arrival order, so when several Synthesize calls queue up
    (e.g. the next sentence arrives while the previous one is still generating) audio stays in sentence order and
    nobody starves."""

    def __init__(self):
        self._cv = threading.Condition()
        self._next = 0
        self._serving = 0

    def acquire(self):
        with self._cv:
            ticket = self._next
            self._next += 1
            while self._serving != ticket:
                self._cv.wait()

    def release(self):
        with self._cv:
            self._serving += 1
            self._cv.notify_all()

    def __enter__(self):
        self.acquire()
        return self

    def __exit__(self, *exc):
        self.release()
        return False
