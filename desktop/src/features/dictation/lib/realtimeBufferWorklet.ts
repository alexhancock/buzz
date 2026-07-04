const TARGET_SAMPLE_RATE = 24000;
const FRAME_SAMPLES = 480; // 20ms at 24kHz

export const REALTIME_BUFFER_PROCESSOR_NAME = "realtime-buffer-processor";

export const REALTIME_BUFFER_WORKLET_SOURCE = /* js */ `
class RealtimeBufferProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this._ratio = sampleRate / ${TARGET_SAMPLE_RATE};
    this._offset = 0;
    this._buf = new Float32Array(${FRAME_SAMPLES});
    this._idx = 0;
  }

  process(inputs) {
    const input = inputs[0]?.[0];
    if (!input) return true;

    while (this._offset < input.length) {
      const i = Math.floor(this._offset);
      const frac = this._offset - i;
      const s0 = input[i];
      const s1 = i + 1 < input.length ? input[i + 1] : s0;
      this._buf[this._idx++] = s0 + frac * (s1 - s0);

      if (this._idx >= ${FRAME_SAMPLES}) {
        const pcm = new Int16Array(${FRAME_SAMPLES});
        for (let j = 0; j < ${FRAME_SAMPLES}; j++) {
          const s = Math.max(-1, Math.min(1, this._buf[j]));
          pcm[j] = s < 0 ? s * 0x8000 : s * 0x7fff;
        }
        this.port.postMessage(pcm.buffer, [pcm.buffer]);
        this._idx = 0;
      }
      this._offset += this._ratio;
    }
    this._offset -= input.length;
    return true;
  }
}

registerProcessor('${REALTIME_BUFFER_PROCESSOR_NAME}', RealtimeBufferProcessor);
`;

/** Create a blob URL that can be passed to `audioWorklet.addModule()`. */
export function createWorkletBlobUrl(): string {
  return URL.createObjectURL(
    new Blob([REALTIME_BUFFER_WORKLET_SOURCE], {
      type: "application/javascript",
    }),
  );
}
