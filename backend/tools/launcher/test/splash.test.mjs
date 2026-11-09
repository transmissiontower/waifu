// node --test tools/launcher/test/splash.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const html = fs.readFileSync(new URL('../src/splash.html', import.meta.url), 'utf8');
const script = html.match(/<script>([\s\S]*)<\/script>/)[1];
const ctx = vm.createContext({});
vm.runInContext(script, ctx); // no `document` here, so only the pure part is defined
const view = ctx.view;

const svc = (label, state, detail = '') => ({ role: label, label, state, detail });

test('before anything is known: indeterminate build step, no crash on empty input', () => {
  for (const st of [undefined, null, {}]) {
    const v = view(st);
    assert.equal(v.steps[0].state, 'active');
    assert.equal(v.indeterminate, true);
    assert.equal(v.ready, false);
    assert.match(v.title, /Waking up the app/);
  }
});

test('compiling shows the crate count and the crate in progress', () => {
  const v = view({ name: 'Mio', phase: 'build', compiled: 212, crate: 'whisper-rs-sys', tail: ['a'] });
  assert.equal(v.title, 'Waking up Mio…');
  assert.equal(v.steps[0].detail, '212 crates compiled · now whisper-rs-sys');
  assert.equal(v.steps[1].state, 'pending');
  assert.equal(v.services.length, 0);
});

test('first-time dependency download is reported before compiling starts', () => {
  assert.equal(view({ phase: 'build', downloaded: 40 }).steps[0].detail, '40 crates downloaded');
});

test('models phase marks the build as done and shows the current download', () => {
  const v = view({ phase: 'models', detail: 'downloading ggml-tiny.en.bin' });
  assert.equal(v.steps.map((s) => s.state).join(), 'done,active,pending,pending');
  assert.equal(v.steps[1].detail, 'downloading ggml-tiny.en.bin');
});

test('services phase lists every service with what it is loading, and progress is the ready fraction', () => {
  const v = view({
    phase: 'services',
    services: { overall: 'starting', url: 'http://127.0.0.1:8080', services: [
      svc('Voice', 'loading', 'loading the voice model (Chatterbox Turbo)'),
      svc('Speech recognition', 'ready'),
      svc('Language model', 'loading', 'warming up the language model'),
      svc('Memory', 'ready'),
    ] },
  });
  assert.equal(v.steps[3].detail, '2 of 4 ready');
  assert.equal(v.services.length, 4);
  assert.equal(v.services[0].text, 'loading…');
  assert.equal(v.services[0].detail, 'loading the voice model (Chatterbox Turbo)');
  assert.equal(v.indeterminate, false);
  assert.equal(v.percent, Math.round(((3 + 0.5) / 4) * 100));
  assert.equal(v.ready, false);
  assert.equal(v.url, 'http://127.0.0.1:8080');
});

test('supervisor started without scripts (no phase file) is treated as the services phase', () => {
  const v = view({ services: { overall: 'starting', services: [svc('Memory', 'starting')] } });
  assert.equal(v.steps[3].state, 'active');
  assert.equal(v.steps[0].state, 'done');
});

test('overall ready -> 100 %, everything done, redirect target exposed', () => {
  const v = view({ name: 'Mio', phase: 'services', services: { overall: 'ready', url: 'http://127.0.0.1:8080', services: [svc('Voice', 'ready'), svc('Memory', 'degraded', 'x')] } });
  assert.equal(v.ready, true);
  assert.equal(v.percent, 100);
  assert.ok(v.steps.every((s) => s.state === 'done'));
  assert.equal(v.title, 'Mio is ready');
  assert.equal(v.url, 'http://127.0.0.1:8080');
});

test('an error stops the progress on the failing step and carries the log tail', () => {
  const v = view({ phase: 'build', error: 'cargo build failed', compiled: 5, tail: ['error[E0432]: unresolved import'] });
  assert.equal(v.steps[0].state, 'error');
  assert.equal(v.error, 'cargo build failed');
  assert.equal(v.subtitle, 'Start-up stopped');
  assert.equal(v.indeterminate, false);
  assert.equal(v.tail.join(), 'error[E0432]: unresolved import');
});

test('failed/degraded services count as finished so a broken optional service cannot block start-up', () => {
  const v = view({ phase: 'services', services: { overall: 'starting', services: [svc('Voice', 'failed', 'oops'), svc('Memory', 'degraded')] } });
  assert.equal(v.steps[3].detail, '2 of 2 ready');
});

test('page script: renders into a (fake) DOM and navigates to the app when ready', async () => {
  const made = [];
  const mk = (tag) => {
    const e = { tag, children: [], style: {}, className: '', textContent: '', firstChild: { style: {} },
      append(...c) { this.children.push(...c); }, };
    made.push(e); return e;
  };
  const byId = {};
  const document = { title: '', getElementById: (id) => (byId[id] ??= mk('#' + id)), createElement: mk };
  const hits = []; let redirect = null;
  const status = { name: 'Mio', phase: 'services', services: { overall: 'ready', url: 'http://127.0.0.1:8080', services: [svc('Voice', 'ready')] } };
  const timers = [];
  const sandbox = {
    document, console,
    location: { replace: (u) => { redirect = u; } },
    fetch: async (u) => { hits.push(u); return { json: async () => status }; },
    setTimeout: (f, ms) => { timers.push([f, ms]); return 0; },
  };
  vm.runInNewContext(script, sandbox);
  await new Promise((r) => setImmediate(r));
  assert.equal(hits[0], '/status');
  assert.equal(document.title, 'Mio is ready');
  assert.equal(byId.bar.firstChild.style.width, '100%');
  assert.equal(byId.err.style.display, 'none');
  const redirectTimer = timers.find(([, ms]) => ms === 600);
  assert.ok(redirectTimer, 'schedules the hand-over to the app');
  redirectTimer[0]();
  await new Promise((r) => setImmediate(r));
  assert.ok(hits.includes('http://127.0.0.1:8080/healthz'), 'probes the gateway first');
  assert.equal(redirect, 'http://127.0.0.1:8080/');
});
