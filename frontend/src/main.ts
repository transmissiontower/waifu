import { Avatar } from './avatar';
import { Mic, Player } from './audio';
import { DEFAULT_VAD } from './vad';
import { Net } from './net';

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const log = $('log'), titleEl = $('title'), dots = $('dots'), toasts = $('toasts');
const net = new Net();
const avatar = new Avatar($<HTMLCanvasElement>('stage'));
let player: Player | null = null;
let mic: Mic | null = null;
let currentAvatar = '';
let persona: Record<string, any> = {};
let botBubble: HTMLElement | null = null;
let pendingIdle = false;
const userId = localStorage.getItem('waifu.user') ?? (() => { const v = 'u' + Math.random().toString(36).slice(2, 8); localStorage.setItem('waifu.user', v); return v; })();

function bubble(kind: 'user' | 'bot', text = ''): HTMLElement {
  const d = document.createElement('div');
  d.className = 'msg ' + kind;
  d.textContent = text;
  log.appendChild(d);
  log.scrollTop = log.scrollHeight;
  return d;
}

function toast(level: string, msg: string) {
  const d = document.createElement('div');
  d.className = 'toast ' + level;
  d.textContent = msg;
  toasts.appendChild(d);
  setTimeout(() => d.remove(), level === 'error' ? 12000 : 7000);
}

async function loadAvatar(file: string) {
  if (!file || file === currentAvatar) return;
  const pick = localStorage.getItem('waifu.avatar');
  const name = pick || file;
  try {
    await avatar.load('/avatars/' + encodeURIComponent(name));
    currentAvatar = file;
    $('fallback').hidden = true;
  } catch (e) {
    console.error(e);
    $('fallback').hidden = avatar.loaded;
    toast('warn', `Could not load avatar "${name}" - chat still works.`);
  }
}

function applyPersona(p: Record<string, any>) {
  persona = p;
  titleEl.textContent = p.name ?? '';
  document.title = p.name ?? 'Waifu';
  void loadAvatar(p.avatar);
}

function setState(s: string) {
  // keep the "speaking" pose until the audio queue has actually drained
  if (s === 'idle' && player?.isPlaying()) { pendingIdle = true; return; }
  pendingIdle = false;
  avatar.setState(s);
}

net.onJson = (j) => {
  switch (j.type) {
    case 'persona': applyPersona(j); break;
    case 'transcript': bubble('user', j.text); botBubble = null; break;
    case 'assistant_text':
      if (!botBubble) botBubble = bubble('bot');
      botBubble.textContent += j.delta;
      log.scrollTop = log.scrollHeight;
      break;
    case 'emotion': avatar.setEmotion(j.name); break;
    case 'state': setState(j.state); break;
    case 'animation': avatar.play(j.name); break;
    case 'notice': toast(j.level, j.message); break;
    case 'turn_done': botBubble = null; break;
  }
};
net.onAudio = (pcm, sr) => { player?.enqueue(pcm, sr); };
net.onOpen = () => net.send({ type: 'hello', user_id: userId, greet: false });
net.connect();

// idle pose once playback has drained
setInterval(() => { if (pendingIdle && !player?.isPlaying()) setState('idle'); }, 150);

function sendText(text: string) {
  text = text.trim();
  if (!text) return;
  if (player?.isPlaying()) { player.stopAll(); net.send({ type: 'interrupt' }); }
  bubble('user', text);
  botBubble = null;
  net.send({ type: 'text', text });
}

$('composer').addEventListener('submit', (e) => {
  e.preventDefault();
  const i = $<HTMLInputElement>('text');
  sendText(i.value);
  i.value = '';
});

$('mute').addEventListener('click', () => {
  if (!player) return;
  player.setMuted(!player.muted);
  $('mute').textContent = player.muted ? '🔇' : '🔊';
});

async function toggleMic() {
  const btn = $('mic');
  if (mic?.active) { mic.stop(); mic = null; btn.classList.remove('on', 'hot'); return; }
  try {
    // tuning knob: localStorage 'waifu.vad_hangover_ms' = silence (ms) that ends an utterance (default 450)
    const hang = Number(localStorage.getItem('waifu.vad_hangover_ms'));
    mic = new Mic(hang >= 150 ? { ...DEFAULT_VAD, hangoverMs: hang } : DEFAULT_VAD);
    mic.getBoost = () => (player?.isPlaying() ? 2.5 : 1);
    await mic.start({
      onSpeechStart: () => {
        btn.classList.add('hot');
        if (player?.isPlaying() || avatar.state === 'speaking' || avatar.state === 'thinking') { player?.stopAll(); net.send({ type: 'interrupt' }); } // barge-in
      },
      onChunk: (pcm) => net.sendAudio(pcm),
      onSpeechEnd: () => { btn.classList.remove('hot'); net.send({ type: 'audio_end' }); },
    });
    btn.classList.add('on');
  } catch (e) {
    mic = null;
    $('mic').hidden = true;
    toast('warn', `Microphone unavailable (${(e as Error).message}) - text chat only.`);
  }
}
$('mic').addEventListener('click', () => void toggleMic());

$('start-btn').addEventListener('click', async () => {
  $('start').remove();
  player = new Player();
  avatar.getLevel = () => player?.level() ?? 0;
  await player.ctx.resume();
  net.send({ type: 'hello', user_id: userId, greet: true });
  if (!navigator.mediaDevices?.getUserMedia) { $('mic').hidden = true; toast('info', 'No microphone API here - text chat only.'); }
  else void toggleMic();
});

// avatar picker + status dots
async function refreshAvatars() {
  try {
    const r = await (await fetch('/api/avatars')).json();
    const sel = $<HTMLSelectElement>('avatar-select');
    sel.innerHTML = '';
    for (const a of r.avatars as string[]) { const o = document.createElement('option'); o.value = o.textContent = a; sel.appendChild(o); }
    sel.value = localStorage.getItem('waifu.avatar') ?? persona.avatar ?? '';
    sel.onchange = () => { localStorage.setItem('waifu.avatar', sel.value); currentAvatar = ''; void loadAvatar(persona.avatar || sel.value); };
  } catch { /* gateway down */ }
}
async function refreshStatus() {
  try {
    const r = await (await fetch('/api/status')).json();
    dots.innerHTML = '';
    for (const [k, v] of Object.entries<any>(r.services)) {
      const d = document.createElement('span');
      d.className = 'dot ' + v.status;
      d.title = `${k}: ${v.status}` + (v.degraded && Object.keys(v.degraded).length ? ' - ' + Object.values(v.degraded).join('; ') : '');
      dots.appendChild(d);
    }
  } catch { dots.innerHTML = '<span class="dot down" title="gateway unreachable"></span>'; }
}
void refreshAvatars();
void refreshStatus();
setInterval(refreshStatus, 5000);
fetch('/api/persona').then((r) => r.json()).then((p) => p.name && applyPersona(p)).catch(() => {});
