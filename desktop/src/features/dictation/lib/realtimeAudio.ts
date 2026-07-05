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
 * Per-item tracking for a single transcription turn. OpenAI's Realtime API
 * sends delta and completed events tagged with an `item_id`; completed events
 * for different turns can arrive out of order, so we reconcile by item id.
 */
interface ItemSegment {
  /** Accumulated delta text (replaced by finalized on completion). */
  pending: string;
  /** Finalized text (set once the completed event arrives). */
  finalized: string | null;
}

/**
 * State for tracking transcription segments keyed by item id.
 * Maintains insertion order so the full transcript is reconstructed
 * in the order items were first seen.
 */
export interface TranscriptSegmentState {
  /** Ordered item ids (insertion order = utterance order). */
  itemOrder: string[];
  /** Per-item segment data. */
  items: Map<string, ItemSegment>;
}

export function createTranscriptSegmentState(): TranscriptSegmentState {
  return { itemOrder: [], items: new Map() };
}

/** Get the current full transcript text from segment state. */
export function getTranscriptText(state: TranscriptSegmentState): string {
  let result = "";
  for (const id of state.itemOrder) {
    const seg = state.items.get(id);
    if (!seg) continue;
    const text = seg.finalized ?? seg.pending;
    if (!text) continue;
    if (result && !result.endsWith(" ") && !text.startsWith(" ")) {
      result += " ";
    }
    result += text;
  }
  return result;
}

/** Internal: get or create the segment for an item. */
function getOrCreateItem(
  state: TranscriptSegmentState,
  itemId: string,
): ItemSegment {
  let seg = state.items.get(itemId);
  if (!seg) {
    seg = { pending: "", finalized: null };
    state.items.set(itemId, seg);
    state.itemOrder.push(itemId);
  }
  return seg;
}

/**
 * Merge a transcript event into the segment state, keyed by `item_id`.
 *
 * - Delta events: append to the item's `pending` text.
 * - Completed events: store `finalized` text, replacing accumulated deltas.
 *
 * Returns the full merged text across all items in order.
 */
export function mergeTranscriptEvent(
  state: TranscriptSegmentState,
  event: TranscriptEvent,
): string {
  // Use item_id from the event; fall back to a synthetic key for events
  // that lack one (shouldn't happen in practice, but be defensive).
  const itemId = event.item_id ?? "__default__";

  if (event.type === TRANSCRIPT_DELTA_EVENT) {
    const seg = getOrCreateItem(state, itemId);
    const delta = event.delta ?? "";
    if (delta) {
      seg.pending += delta;
    }
  } else if (event.type === TRANSCRIPT_COMPLETED_EVENT) {
    const seg = getOrCreateItem(state, itemId);
    seg.finalized = event.transcript ?? "";
  }

  // Reconstruct full text from all items in order.
  return getTranscriptText(state);
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
