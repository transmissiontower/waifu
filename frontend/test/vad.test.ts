import { test } from 'node:test';
import assert from 'node:assert/strict';
import { Vad, DEFAULT_VAD } from '../src/vad.ts';

const frames = (v: Vad, rms: number, n: number, boost = 1) => {
  const ev: [number, string][] = [];
  for (let i = 0; i < n; i++) { const e = v.push(rms, boost); if (e) ev.push([i, e]); }
  return ev;
};

test('speech start fires after ~96 ms, end after ~450 ms of silence', () => {
  const v = new Vad();
  frames(v, 0.004, 30);
  assert.deepEqual(frames(v, 0.1, 10), [[2, 'start']]); // 3 frames = 96 ms (old: 200 ms)
  const ev = frames(v, 0.004, 30);
  assert.deepEqual(ev, [[14, 'end']]);                   // 15 frames x 32 ms = 480 ms (old: 900 ms)
  assert.ok((ev[0][0] + 1) * DEFAULT_VAD.frameMs <= 480);
});

test('short pauses inside a sentence do not end the utterance', () => {
  const v = new Vad();
  frames(v, 0.1, 5);
  assert.deepEqual(frames(v, 0.004, 10), []);  // 320 ms pause
  assert.deepEqual(frames(v, 0.1, 5), []);
  assert.equal(v.speaking, true);
});

test('a blip shorter than the onset time is ignored', () => {
  const v = new Vad();
  assert.deepEqual(frames(v, 0.1, 2), []);
  assert.deepEqual(frames(v, 0.004, 5), []);
  assert.equal(v.speaking, false);
});

test('while she is speaking the onset is stricter and the threshold higher', () => {
  const v = new Vad();
  frames(v, 0.004, 20);
  assert.deepEqual(frames(v, 0.01, 20, 2.5), []);   // below 0.015*2.5
  assert.deepEqual(frames(v, 0.1, 4, 2.5), []);     // 128 ms < 160 ms
  assert.deepEqual(frames(v, 0.1, 1, 2.5), [[0, 'start']]);
});

test('noise floor adapts only in silence, at the same time constant as the old 100 ms detector', () => {
  const v = new Vad();
  frames(v, 0.012, 100); // 3.2 s of quiet background noise (below the trigger floor)
  const expected = 0.012 - (0.012 - 0.005) * Math.pow(0.95, 32); // old: x0.95 per 100 ms
  assert.ok(Math.abs(v.noiseFloor - expected) < 1e-4, `${v.noiseFloor} vs ${expected}`);
  const before = v.noiseFloor;
  frames(v, 0.2, 10); // speech must not drag the floor up
  assert.equal(v.noiseFloor, before);
});

test('can detect a second utterance after the first ended', () => {
  const v = new Vad();
  frames(v, 0.1, 5); frames(v, 0.004, 20);
  assert.equal(v.speaking, false);
  assert.deepEqual(frames(v, 0.1, 5), [[2, 'start']]);
});
