/** Energy voice-activity detector, pure (no DOM / audio APIs) so it can be unit-tested.
 *  It is fed one RMS value per analysis frame and says when speech starts and ends. */
export interface VadConfig {
  /** analysis frame length */
  frameMs: number;
  /** continuous loud time before speech is declared (= barge-in latency) */
  onsetMs: number;
  /** same, but while she is speaking (boost > 1): stricter so her own voice / clicks don't interrupt her */
  onsetWhileSpeakingMs: number;
  /** continuous silence before the utterance is declared over (= the delay before she starts thinking) */
  hangoverMs: number;
  /** audio kept from before the detected start, so the first syllable is not clipped */
  preRollMs: number;
  /** absolute floor of the trigger threshold */
  minThreshold: number;
  /** trigger threshold = max(minThreshold, noiseFloor * noiseFactor) * boost */
  noiseFactor: number;
}

export const DEFAULT_VAD: VadConfig = {
  frameMs: 32,
  onsetMs: 96,
  onsetWhileSpeakingMs: 160,
  hangoverMs: 450, // was 900 ms (9 x 100 ms frames)
  preRollMs: 320,
  minThreshold: 0.015,
  noiseFactor: 3.5,
};

export type VadEvent = 'start' | 'end' | null;

export class Vad {
  private noise = 0.005;
  private loud = 0;
  private quiet = 0;
  private inSpeech = false;
  private readonly noiseAlpha: number;
  readonly cfg: VadConfig;

  constructor(cfg: VadConfig = DEFAULT_VAD) {
    this.cfg = cfg;
    // the old detector used 0.95/0.05 per 100 ms frame; keep the same time constant at any frame length
    this.noiseAlpha = 1 - Math.pow(0.95, cfg.frameMs / 100);
  }

  get speaking(): boolean { return this.inSpeech; }
  get noiseFloor(): number { return this.noise; }
  get hangoverFrames(): number { return Math.max(1, Math.ceil(this.cfg.hangoverMs / this.cfg.frameMs)); }
  onsetFrames(boost: number): number {
    return Math.max(1, Math.ceil((boost > 1 ? this.cfg.onsetWhileSpeakingMs : this.cfg.onsetMs) / this.cfg.frameMs));
  }
  get preRollFrames(): number { return Math.max(1, Math.ceil(this.cfg.preRollMs / this.cfg.frameMs)); }

  /** Feed one frame. Returns 'start' on the frame speech begins, 'end' on the frame it ends. */
  push(rms: number, boost = 1): VadEvent {
    const thr = Math.max(this.cfg.minThreshold, this.noise * this.cfg.noiseFactor) * boost;
    if (rms > thr) { this.loud++; this.quiet = 0; }
    else {
      this.quiet++; this.loud = 0;
      if (!this.inSpeech) this.noise = this.noise * (1 - this.noiseAlpha) + rms * this.noiseAlpha;
    }
    if (!this.inSpeech) {
      if (this.loud >= this.onsetFrames(boost)) { this.inSpeech = true; this.quiet = 0; return 'start'; }
    } else if (this.quiet >= this.hangoverFrames) {
      this.inSpeech = false; this.loud = 0;
      return 'end';
    }
    return null;
  }

  reset() { this.loud = this.quiet = 0; this.inSpeech = false; }
}
