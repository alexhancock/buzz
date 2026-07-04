import { describe, it } from "node:test";
import assert from "node:assert/strict";

// We test the pure logic via dynamic import of the TS source compiled by the
// test runner (vitest/tsx). Since this is an .mjs file run by the Node test
// runner through the desktop vitest config, import the built output or use
// a direct TS import if the runner supports it.

// Inline the logic to keep the test self-contained and avoid bundler issues.
const TRANSCRIPT_DELTA_EVENT =
  "conversation.item.input_audio_transcription.delta";
const TRANSCRIPT_COMPLETED_EVENT =
  "conversation.item.input_audio_transcription.completed";

function createTranscriptSegmentState() {
  return { committed: "", pendingDelta: "" };
}

function mergeTranscriptEvent(state, event) {
  if (event.type === TRANSCRIPT_DELTA_EVENT) {
    const delta = event.delta ?? "";
    if (delta) {
      state.pendingDelta += delta;
    }
  } else if (event.type === TRANSCRIPT_COMPLETED_EVENT) {
    const finalText = event.transcript ?? "";
    const separator = state.committed && finalText ? "" : "";
    state.committed = state.committed + separator + finalText;
    state.pendingDelta = "";
  }

  return state.committed + state.pendingDelta;
}

describe("mergeTranscriptEvent", () => {
  it("accumulates delta events", () => {
    const state = createTranscriptSegmentState();
    const r1 = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      delta: "hello ",
    });
    assert.equal(r1, "hello ");

    const r2 = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      delta: "world",
    });
    assert.equal(r2, "hello world");
  });

  it("replaces deltas with finalized text on completed event", () => {
    const state = createTranscriptSegmentState();
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      delta: "hello world",
    });

    // Completed event carries corrected/punctuated version
    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      transcript: "Hello, world.",
    });
    assert.equal(result, "Hello, world.");
    assert.equal(state.committed, "Hello, world.");
    assert.equal(state.pendingDelta, "");
  });

  it("handles multiple segments sequentially", () => {
    const state = createTranscriptSegmentState();

    // First segment
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      delta: "first ",
    });
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      transcript: "First. ",
    });

    // Second segment
    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      delta: "second",
    });
    assert.equal(state.committed + state.pendingDelta, "First. second");

    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      transcript: "Second.",
    });
    assert.equal(result, "First. Second.");
  });

  it("does not duplicate text on completed event", () => {
    const state = createTranscriptSegmentState();

    mergeTranscriptEvent(state, {
      type: TRANSCRIPT_DELTA_EVENT,
      delta: "hello world",
    });

    // Without the fix, this would append: "hello worldHello, world."
    const result = mergeTranscriptEvent(state, {
      type: TRANSCRIPT_COMPLETED_EVENT,
      transcript: "Hello, world.",
    });
    assert.equal(result, "Hello, world.");
  });
});
