import { describe, it } from "node:test";
import assert from "node:assert/strict";

// Inline the logic to keep the test self-contained and avoid bundler issues.
const TRANSCRIPT_DELTA_EVENT =
  "conversation.item.input_audio_transcription.delta";
const TRANSCRIPT_COMPLETED_EVENT =
  "conversation.item.input_audio_transcription.completed";
const BUFFER_COMMITTED_EVENT = "input_audio_buffer.committed";

function createTranscriptSegmentState() {
  return { itemOrder: [], items: new Map() };
}

function getOrCreateItem(state, itemId, previousItemId) {
  let seg = state.items.get(itemId);
  if (!seg) {
    seg = { pending: "", finalized: null };
    state.items.set(itemId, seg);
    if (previousItemId) {
      const prevIndex = state.itemOrder.indexOf(previousItemId);
      if (prevIndex !== -1) {
        state.itemOrder.splice(prevIndex + 1, 0, itemId);
      } else {
        state.itemOrder.push(itemId);
      }
    } else {
      state.itemOrder.push(itemId);
    }
  }
  return seg;
}

function getTranscriptText(state) {
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

function mergeTranscriptEvent(state, event) {
  const itemId = event.item_id ?? "__default__";

  if (event.type === BUFFER_COMMITTED_EVENT) {
    getOrCreateItem(state, itemId, event.previous_item_id ?? undefined);
  } else if (event.type === TRANSCRIPT_DELTA_EVENT) {
    const seg = getOrCreateItem(state, itemId);
    const delta = event.delta ?? "";
    if (delta) {
      seg.pending += delta;
    }
  } else if (event.type === TRANSCRIPT_COMPLETED_EVENT) {
    const seg = getOrCreateItem(state, itemId);
    seg.finalized = event.transcript ?? "";
  }

  return getTranscriptText(state);
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
      transcript: "First.",
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
      transcript: "Second.",
    });

    // item_1 still shows pending
    let result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      item_id: "item_1",
      delta: " more",
    });
    assert.equal(result, "first more Second.");

    // item_1 finally completes
    result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_1",
      transcript: "First more.",
    });
    assert.equal(result, "First more. Second.");
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

  it("preserves committed item order when completions arrive out of order", () => {
    const state = createTranscriptSegmentState();

    // Server commits items in order: item_1 then item_2
    mergeTranscriptEvent(state, {
      type: BUFFER_COMMITTED_EVENT,
      item_id: "item_1",
      previous_item_id: null,
    });
    mergeTranscriptEvent(state, {
      type: BUFFER_COMMITTED_EVENT,
      item_id: "item_2",
      previous_item_id: "item_1",
    });

    // But item_2's completion arrives first
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_2",
      transcript: "Second.",
    });

    // Then item_1's completion arrives
    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_1",
      transcript: "First.",
    });

    // Order must follow committed order, not arrival order
    assert.equal(result, "First. Second.");
  });

  it("inserts item after previous_item_id even if later items already exist", () => {
    const state = createTranscriptSegmentState();

    // Commit item_1
    mergeTranscriptEvent(state, {
      type: BUFFER_COMMITTED_EVENT,
      item_id: "item_1",
    });

    // Commit item_3 (item_2 not yet committed)
    mergeTranscriptEvent(state, {
      type: BUFFER_COMMITTED_EVENT,
      item_id: "item_3",
      previous_item_id: "item_1",
    });

    // Now commit item_2 between item_1 and item_3
    // (previous_item_id = item_1, so it goes after item_1)
    mergeTranscriptEvent(state, {
      type: BUFFER_COMMITTED_EVENT,
      item_id: "item_2",
      previous_item_id: "item_1",
    });

    // Add transcripts
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_1",
      transcript: "One.",
    });
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_2",
      transcript: "Two.",
    });
    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_3",
      transcript: "Three.",
    });

    // item_2 was inserted after item_1 (before item_3)
    assert.equal(result, "One. Two. Three.");
  });

  it("handles completion-only flow with committed order", () => {
    const state = createTranscriptSegmentState();

    // Server commits both items
    mergeTranscriptEvent(state, {
      type: BUFFER_COMMITTED_EVENT,
      item_id: "item_A",
    });
    mergeTranscriptEvent(state, {
      type: BUFFER_COMMITTED_EVENT,
      item_id: "item_B",
      previous_item_id: "item_A",
    });

    // Only completed events arrive (no deltas) — in reverse order
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_B",
      transcript: "Bravo.",
    });
    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      item_id: "item_A",
      transcript: "Alpha.",
    });

    assert.equal(result, "Alpha. Bravo.");
  });
});
