import { DEFAULT_VAD, Vad, type VadConfig } from './vad';

/** Playback of streamed PCM16 chunks + level metering for lip-sync. */
export class Player {
  ctx: AudioContext;
  private gain: GainNode;
  private analyser: AnalyserNode;
  private buf: Float32Array<ArrayBuffer>;
  private next = 0;
  private sources = new Set<AudioBufferSourceNode>();
  muted = false;

  constructor() {
    this.ctx = new AudioContext();
    this.gain = this.ctx.createGain();
    this.analyser = this.ctx.createAnalyser();
    this.analyser.fftSize = 1024;
    this.buf = new Float32Array(this.analyser.fftSize);
    this.gain.connect(this.analyser);
    this.analyser.connect(this.ctx.destination);
  }

  setMuted(m: boolean) { this.muted = m; this.gain.gain.value = m ? 0 : 1; }

  enqueue(pcm: Int16Array, sampleRate: number) {
    if (this.ctx.state === 'suspended') void this.ctx.resume();
    const f = new Float32Array(pcm.length);
    for (let i = 0; i < pcm.length; i++) f[i] = pcm[i] / 32768;
    const ab = this.ctx.createBuffer(1, f.length, sampleRate);
    ab.copyToChannel(f, 0);
    const src = this.ctx.createBufferSource();
    src.buffer = ab;
    src.connect(this.gain);
    const t = Math.max(this.ctx.currentTime + 0.04, this.next);
    src.start(t);
    this.next = t + ab.duration;
    this.sources.add(src);
    src.onended = () => this.sources.delete(src);
  }

  isPlaying(): boolean { return this.sources.size > 0 && this.next > this.ctx.currentTime; }

  stopAll() {
    for (const s of this.sources) { try { s.stop(); } catch { /* already stopped */ } }
    this.sources.clear();
    this.next = 0;
  }

  /** 0..1 loudness of what is being heard right now (drives the mouth). */
  level(): number {
    if (!this.isPlaying()) return 0;
    this.analyser.getFloatTimeDomainData(this.buf);
    let s = 0;
    for (const v of this.buf) s += v * v;
    const rms = Math.sqrt(s / this.buf.length);
    return Math.min(1, Math.pow(rms * 7, 0.75));
  }
}

const WORKLET = `class Cap extends AudioWorkletProcessor { process(i){ const c=i[0][0]; if(c) this.port.postMessage(c.slice(0)); return true; } }
registerProcessor('cap', Cap);`;

export interface MicHandlers {
  onSpeechStart(): void;
  onChunk(pcm16: Int16Array): void;
  onSpeechEnd(): void;
}

const FRAME_SAMPLES = 512; // 32 ms @ 16 kHz (was 100 ms: slower to notice speech start / end)

/** Hands-free microphone with a simple adaptive energy VAD, downsampled to 16 kHz PCM16. */
export class Mic {
  private stream?: MediaStream;
  private ctx?: AudioContext;
  private node?: AudioWorkletNode;
  active = false;
  /** raise the trigger threshold while she is speaking so she doesn't hear herself */
  getBoost: () => number = () => 1;
  level = 0;

  constructor(private vadCfg: VadConfig = DEFAULT_VAD) {}

  async start(h: MicHandlers): Promise<void> {
    if (!navigator.mediaDevices?.getUserMedia) throw new Error('getUserMedia unavailable (needs https or localhost)');
    this.stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true } });
    this.ctx = new AudioContext();
    const url = URL.createObjectURL(new Blob([WORKLET], { type: 'application/javascript' }));
    await this.ctx.audioWorklet.addModule(url);
    const src = this.ctx.createMediaStreamSource(this.stream);
    this.node = new AudioWorkletNode(this.ctx, 'cap');
    src.connect(this.node);

    const ratio = this.ctx.sampleRate / 16000;
    const vad = new Vad(this.vadCfg);
    let pre: Int16Array[] = [];
    let acc: number[] = [];
    let pos = 0; // fractional read position, carried across worklet blocks
    this.active = true;
    this.node.port.onmessage = (e: MessageEvent<Float32Array>) => {
      if (!this.active) return;
      const f = e.data;
      for (; pos < f.length; pos += ratio) acc.push(f[Math.floor(pos)]);
      pos -= f.length;
      while (acc.length >= FRAME_SAMPLES) {
        const frame = acc.splice(0, FRAME_SAMPLES);
        let s = 0;
        for (const v of frame) s += v * v;
        const rms = Math.sqrt(s / frame.length); // energy of the whole frame, not of one 2.7 ms block
        this.level = rms;
        const pcm = new Int16Array(frame.length);
        frame.forEach((v, i) => (pcm[i] = Math.max(-1, Math.min(1, v)) * 32767));
        const wasSpeaking = vad.speaking;
        if (!wasSpeaking) { pre.push(pcm); if (pre.length > vad.preRollFrames) pre.shift(); }
        const ev = vad.push(rms, this.getBoost());
        if (ev === 'start') { h.onSpeechStart(); pre.forEach(h.onChunk); pre = []; }
        else if (wasSpeaking) { h.onChunk(pcm); if (ev === 'end') h.onSpeechEnd(); }
      }
    };
  }

  stop() {
    this.active = false;
    this.stream?.getTracks().forEach((t) => t.stop());
    void this.ctx?.close();
  }
}
