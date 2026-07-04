import { describe, it } from "node:test";
import assert from "node:assert/strict";

// Inline the logic to keep the test self-contained and avoid bundler issues.
const TRANSCRIPT_DELTA_EVENT =
  "conversation.item.input_audio_transcription.delta";
const TRANSCRIPT_COMPLETED_EVENT =
  "conversation.item.input_audio_transcription.completed";

function createTranscriptSegmentState() {
  return { itemOrder: [], items: new Map() };
}

function getOrCreateItem(state, itemId) {
  let seg = state.items.get(itemId);
  if (!seg) {
    seg = { pending: "", finalized: null };
    state.items.set(itemId, seg);
    state.itemOrder.push(itemId);
  }
  return seg;
}

function mergeTranscriptEvent(state, event) {
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

  let result = "";
  for (const id of state.itemOrder) {
    const seg = state.items.get(id);
    if (!seg) continue;
    result += seg.finalized ?? seg.pending;
  }
  return result;
}

describe("mergeTranscriptEvent", () => {
  it("accumulates delta events for a single item", () => {
    const state = createTranscriptSegmentState();
    const r1 = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_1",
      delta: "hello ",
    });
    assert.equal(r1, "hello ");

    const r2 = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_1",
      delta: "world",
    });
    assert.equal(r2, "hello world");
  });

  it("replaces deltas with finalized text on completed event", () => {
    const state = createTranscriptSegmentState();
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_1",
      delta: "hello world",
    });

    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_1",
      transcript: "Hello, world.",
    });
    assert.equal(result, "Hello, world.");
  });

  it("handles multiple items in order", () => {
    const state = createTranscriptSegmentState();

    // First item
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_1",
      delta: "first ",
    });
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_1",
      transcript: "First. ",
    });

    // Second item
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_2",
      delta: "second",
    });
    assert.equal(state.items.get("item_2").pending, "second");

    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_2",
      transcript: "Second.",
    });
    assert.equal(result, "First. Second.");
  });

  it("handles out-of-order completed events by item id", () => {
    const state = createTranscriptSegmentState();

    // Both items start with deltas
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_1",
      delta: "first",
    });
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_2",
      delta: "second",
    });

    // item_2 completes before item_1
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_2",
      transcript: "Second. ",
    });

    // item_1 still shows pending
    let result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_1",
      delta: " more",
    });
    assert.equal(result, "first moreSecond. ");

    // item_1 finally completes
    result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_1",
      transcript: "First more. ",
    });
    assert.equal(result, "First more. Second. ");
  });

  it("does not duplicate text on completed event", () => {
    const state = createTranscriptSegmentState();

    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_1",
      delta: "hello world",
    });

    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_1",
      transcript: "Hello, world.",
    });
    assert.equal(result, "Hello, world.");
  });

  it("falls back to __default__ when item_id is missing", () => {
    const state = createTranscriptSegmentState();
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      delta: "no id",
    });
    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      transcript: "No id.",
    });
    assert.equal(result, "No id.");
  });
});
