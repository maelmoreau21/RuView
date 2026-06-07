/**
 * Live CSI client for Pose Fusion.
 *
 * Connects to the RuvSense sensing WebSocket and exposes live CSI tensors for
 * the fusion pipeline. It never generates synthetic CSI frames.
 */

export class LiveCsiClient {
  static VERSION = 'v5-live-only';

  constructor(opts = {}) {
    this.subcarriers = opts.subcarriers || 52;
    this.timeWindow = opts.timeWindow || 56;
    this.mode = 'offline';
    this.ws = null;

    this.amplitudeBuffer = [];
    this.phaseBuffer = [];
    this.frameCount = 0;
    this._noiseState = new Float32Array(this.subcarriers);
    this.rssiDbm = -100;
    this._rssiTarget = -100;
    this.personPresence = 0;
  }

  async connectLive(url) {
    return new Promise((resolve) => {
      try {
        this.ws = new WebSocket(url);
        this.ws.binaryType = 'arraybuffer';
        this.ws.onmessage = (evt) => this._handleLiveFrame(evt.data);
        this.ws.onopen = () => { this.mode = 'live'; resolve(true); };
        this.ws.onerror = () => resolve(false);
        this.ws.onclose = () => { this.mode = 'offline'; };
        setTimeout(() => { if (this.mode !== 'live') resolve(false); }, 3000);
      } catch {
        resolve(false);
      }
    });
  }

  disconnect() {
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
    this.mode = 'offline';
  }

  get isLive() {
    return this.mode === 'live';
  }

  updatePersonState() {
    this.personPresence = 0;
  }

  nextFrame() {
    if (this.mode !== 'live' || !this._liveAmplitude) {
      this.rssiDbm += (this._rssiTarget - this.rssiDbm) * 0.1;
      return null;
    }

    const amp = new Float32Array(this.subcarriers);
    const phase = new Float32Array(this.subcarriers);
    amp.set(this._liveAmplitude);
    phase.set(this._livePhase || new Float32Array(this.subcarriers));

    this.amplitudeBuffer.push(new Float32Array(amp));
    this.phaseBuffer.push(new Float32Array(phase));
    if (this.amplitudeBuffer.length > this.timeWindow) {
      this.amplitudeBuffer.shift();
      this.phaseBuffer.shift();
    }

    this.rssiDbm += (this._rssiTarget - this.rssiDbm) * 0.1;

    let signalPower = 0;
    let noisePower = 0;
    for (let i = 0; i < this.subcarriers; i++) {
      signalPower += amp[i] * amp[i];
      noisePower += this._noiseState[i] * this._noiseState[i];
    }
    const snr = noisePower > 0 ? 10 * Math.log10(signalPower / noisePower) : 30;

    this.frameCount++;
    return { amplitude: amp, phase, snr: Math.max(0, Math.min(40, snr)) };
  }

  buildPseudoImage(targetSize = 56) {
    const buf = this.amplitudeBuffer;
    const pBuf = this.phaseBuffer;
    const frames = buf.length;
    if (frames < 2) {
      return new Uint8Array(targetSize * targetSize * 3);
    }

    const rgb = new Uint8Array(targetSize * targetSize * 3);
    for (let y = 0; y < targetSize; y++) {
      const fi = Math.min(Math.floor(y / targetSize * frames), frames - 1);
      for (let x = 0; x < targetSize; x++) {
        const si = Math.min(Math.floor(x / targetSize * this.subcarriers), this.subcarriers - 1);
        const idx = (y * targetSize + x) * 3;
        const ampVal = buf[fi][si];
        const phaseVal = (pBuf[fi][si] % (2 * Math.PI) + 2 * Math.PI) % (2 * Math.PI);
        rgb[idx] = Math.min(255, Math.max(0, Math.floor(ampVal * 255)));
        rgb[idx + 1] = Math.floor(phaseVal / (2 * Math.PI) * 255);
        if (fi > 0) {
          const diff = Math.abs(buf[fi][si] - buf[fi - 1][si]);
          rgb[idx + 2] = Math.min(255, Math.floor(diff * 500));
        }
      }
    }
    return rgb;
  }

  getHeatmapData() {
    const frames = this.amplitudeBuffer.length;
    const width = this.subcarriers;
    const height = Math.min(frames, this.timeWindow);
    const data = new Float32Array(width * height);
    for (let y = 0; y < height; y++) {
      const fi = frames - height + y;
      if (fi >= 0 && fi < frames) {
        for (let x = 0; x < width; x++) {
          data[y * width + x] = this.amplitudeBuffer[fi][x];
        }
      }
    }
    return { data, width, height };
  }

  _handleLiveFrame(data) {
    if (typeof data === 'string') {
      try {
        this._handleJsonFrame(JSON.parse(data));
      } catch {
        // ignore malformed JSON
      }
      return;
    }

    if (data instanceof Blob) {
      data.arrayBuffer().then((ab) => this._handleLiveFrame(ab)).catch(() => {});
      return;
    }

    if (!(data instanceof ArrayBuffer) || data.byteLength < 20) return;
    const view = new DataView(data);
    const magic = view.getUint32(0, true);
    if (magic !== 0xC5110001) return;

    const numSub = Math.min(view.getUint16(8, true), this.subcarriers);
    this._liveAmplitude = new Float32Array(this.subcarriers);
    this._livePhase = new Float32Array(this.subcarriers);

    const headerSize = 20;
    for (let i = 0; i < numSub && (headerSize + i * 4 + 3) < data.byteLength; i++) {
      const real = view.getInt16(headerSize + i * 4, true);
      const imag = view.getInt16(headerSize + i * 4 + 2, true);
      this._liveAmplitude[i] = Math.sqrt(real * real + imag * imag) / 2048;
      this._livePhase[i] = Math.atan2(imag, real);
    }
  }

  _handleJsonFrame(msg) {
    this._liveAmplitude = new Float32Array(this.subcarriers);
    this._livePhase = new Float32Array(this.subcarriers);

    const node = (msg.nodes && msg.nodes[0]) || msg;
    const ampArr = node.amplitude || msg.amplitude;
    if (ampArr && Array.isArray(ampArr)) {
      const n = Math.min(ampArr.length, this.subcarriers);
      let maxAmp = 0;
      for (let i = 0; i < n; i++) maxAmp = Math.max(maxAmp, Math.abs(ampArr[i]));
      const scale = maxAmp > 0 ? 1.0 / maxAmp : 1.0;
      for (let i = 0; i < n; i++) this._liveAmplitude[i] = Math.abs(ampArr[i]) * scale;
    }

    const phaseArr = node.phase || msg.phase;
    if (phaseArr && Array.isArray(phaseArr)) {
      const n = Math.min(phaseArr.length, this.subcarriers);
      for (let i = 0; i < n; i++) this._livePhase[i] = phaseArr[i];
    } else if (ampArr) {
      for (let i = 1; i < this.subcarriers; i++) {
        this._livePhase[i] = this._livePhase[i - 1] + (this._liveAmplitude[i] - this._liveAmplitude[i - 1]) * Math.PI;
      }
    }

    const iq = node.iq || msg.iq;
    if (iq && Array.isArray(iq)) {
      const n = Math.min(iq.length / 2, this.subcarriers);
      for (let i = 0; i < n; i++) {
        const real = iq[i * 2];
        const imag = iq[i * 2 + 1];
        this._liveAmplitude[i] = Math.sqrt(real * real + imag * imag) / 2048;
        this._livePhase[i] = Math.atan2(imag, real);
      }
    }

    if (typeof node.rssi_dbm === 'number') {
      this._rssiTarget = node.rssi_dbm;
    } else if (msg.features && typeof msg.features.mean_rssi === 'number') {
      this._rssiTarget = msg.features.mean_rssi;
    }

    const cls = msg.classification;
    this.personPresence = cls && cls.presence && typeof cls.confidence === 'number' ? cls.confidence : 0;
  }
}
