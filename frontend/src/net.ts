export type Json = Record<string, any>;

/** WebSocket client with automatic reconnect. Binary frames: [1][u32 sr][u32 seq][pcm16]. */
export class Net {
  private ws?: WebSocket;
  private queue: (string | ArrayBufferLike)[] = [];
  onJson: (j: Json) => void = () => {};
  onAudio: (pcm: Int16Array, sr: number) => void = () => {};
  onOpen: () => void = () => {};
  onClose: () => void = () => {};
  connected = false;

  connect() {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/ws`);
    ws.binaryType = 'arraybuffer';
    this.ws = ws;
    ws.onopen = () => { this.connected = true; this.onOpen(); };
    ws.onclose = () => { this.connected = false; this.onClose(); setTimeout(() => this.connect(), 1500); };
    ws.onerror = () => ws.close();
    ws.onmessage = (e) => {
      if (typeof e.data === 'string') { try { this.onJson(JSON.parse(e.data)); } catch { /* ignore */ } return; }
      const dv = new DataView(e.data as ArrayBuffer);
      if (dv.getUint8(0) === 1) {
        const sr = dv.getUint32(1, true);
        const bytes = (e.data as ArrayBuffer).slice(9);
        this.onAudio(new Int16Array(bytes, 0, bytes.byteLength >> 1), sr);
      }
    };
  }

  send(j: Json) { if (this.connected) this.ws!.send(JSON.stringify(j)); }

  sendAudio(pcm: Int16Array) {
    if (!this.connected) return;
    const b = new Uint8Array(1 + pcm.byteLength);
    b[0] = 2;
    b.set(new Uint8Array(pcm.buffer, pcm.byteOffset, pcm.byteLength), 1);
    this.ws!.send(b);
  }
}
