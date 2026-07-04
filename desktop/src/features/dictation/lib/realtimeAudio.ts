import {
  REALTIME_BUFFER_PROCESSOR_NAME,
  createWorkletBlobUrl,
} from "./realtimeBufferWorklet";

export const OPENAI_REALTIME_WEBRTC_URL =
  "https://api.openai.com/v1/realtime/calls";
export const TRANSCRIPT_DELTA_EVENT =
  "conversation.item.input_audio_transcription.delta";
export const TRANSCRIPT_COMPLETED_EVENT =
  "conversation.item.input_audio_transcription.completed";

const MAX_BUFFER_CHUNKS = 500; // ~10s at 20ms per chunk

export type TranscriptEvent = {
  type?: string;
  item_id?: string;
  content_index?: number;
  delta?: string;
  transcript?: string;
  message?: string;
  error?: { message?: string };
};

export function createPeerConnection(): RTCPeerConnection {
  return new RTCPeerConnection();
}

export async function connectPeerConnection(options: {
  peerConnection: RTCPeerConnection;
  clientSecret: string;
}): Promise<void> {
  const offer = await options.peerConnection.createOffer();
  await options.peerConnection.setLocalDescription(offer);

  const response = await fetch(OPENAI_REALTIME_WEBRTC_URL, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${options.clientSecret}`,
      "Content-Type": "application/sdp",
    },
    body: offer.sdp ?? "",
  });

  const body = await response.text();
  if (!response.ok) {
    throw new Error(
      `OpenAI realtime connection failed (${response.status}): ${body}`,
    );
  }

  await options.peerConnection.setRemoteDescription({
    type: "answer",
    sdp: body,
  });
}

/**
 * State for tracking the current transcription segment. Completed events
 * carry the final text for the same segment that prior deltas built up,
 * potentially with corrections/punctuation. We track the delta accumulation
 * so we can replace it with the finalized text on completion.
 */
export interface TranscriptSegmentState {
  /** Text committed from previous (completed) segments. */
  committed: string;
  /** Accumulated delta text for the in-progress segment. */
  pendingDelta: string;
}

export function createTranscriptSegmentState(): TranscriptSegmentState {
  return { committed: "", pendingDelta: "" };
}

/**
 * Merge a transcript event into the segment state.
 *
 * - Delta events: append to `pendingDelta`.
 * - Completed events: replace `pendingDelta` with the finalized transcript,
 *   then commit it (move to `committed` and reset `pendingDelta`).
 *
 * Returns the full merged text (committed + pending).
 */
export function mergeTranscriptEvent(
  state: TranscriptSegmentState,
  event: TranscriptEvent,
): string {
  if (event.type === TRANSCRIPT_DELTA_EVENT) {
    const delta = event.delta ?? "";
    if (delta) {
      state.pendingDelta += delta;
    }
  } else if (event.type === TRANSCRIPT_COMPLETED_EVENT) {
    const finalText = event.transcript ?? "";
    // Replace the accumulated deltas with the finalized text, then commit.
    const separator = state.committed && finalText ? "" : "";
    state.committed = state.committed + separator + finalText;
    state.pendingDelta = "";
  }

  return state.committed + state.pendingDelta;
}

// ── Audio buffer capture ──────────────────────────────────────────────────

export interface AudioBufferCapture {
  chunks: Int16Array[];
  close(): void;
}

export async function createAudioBufferCapture(
  stream: MediaStream,
): Promise<AudioBufferCapture> {
  const audioContext = new AudioContext();
  const blobUrl = createWorkletBlobUrl();
  try {
    await audioContext.audioWorklet.addModule(blobUrl);
  } finally {
    URL.revokeObjectURL(blobUrl);
  }

  const source = audioContext.createMediaStreamSource(stream);
  const worklet = new AudioWorkletNode(
    audioContext,
    REALTIME_BUFFER_PROCESSOR_NAME,
  );
  source.connect(worklet);
  worklet.connect(audioContext.destination);

  const chunks: Int16Array[] = [];
  worklet.port.onmessage = (event: MessageEvent<ArrayBuffer>) => {
    if (chunks.length < MAX_BUFFER_CHUNKS) {
      chunks.push(new Int16Array(event.data));
    }
  };

  return {
    chunks,
    close() {
      worklet.disconnect();
      source.disconnect();
      void audioContext.close();
    },
  };
}

// ── Flush buffered PCM into the data channel ──────────────────────────────

function int16ToBase64(pcm: Int16Array): string {
  const bytes = new Uint8Array(pcm.buffer, pcm.byteOffset, pcm.byteLength);
  let binary = "";
  for (let i = 0; i < bytes.length; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary);
}

export function flushAudioBuffer(
  dataChannel: RTCDataChannel,
  chunks: Int16Array[],
): void {
  for (const chunk of chunks) {
    dataChannel.send(
      JSON.stringify({
        type: "input_audio_buffer.append",
        audio: int16ToBase64(chunk),
      }),
    );
  }
  chunks.length = 0;
}
