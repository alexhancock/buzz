import assert from "node:assert/strict";
import test from "node:test";

import {
  DEFAULT_AUTO_SUBMIT_PHRASE,
  getAutoSubmitMatch,
  parseAutoSubmitPhrases,
  replaceTrailingTranscribedText,
} from "./voiceInput.ts";

// ── parseAutoSubmitPhrases ──────────────────────────────────────────────────

test("parseAutoSubmitPhrases_returnsEmptyForNullish", () => {
  assert.deepEqual(parseAutoSubmitPhrases(null), []);
  assert.deepEqual(parseAutoSubmitPhrases(undefined), []);
  assert.deepEqual(parseAutoSubmitPhrases(""), []);
});

test("parseAutoSubmitPhrases_splitsNormalizesAndDedupes", () => {
  assert.deepEqual(parseAutoSubmitPhrases("Submit, send it, submit, "), [
    "submit",
    "send it",
  ]);
});

test("parseAutoSubmitPhrases_stripsTrailingPunctuation", () => {
  assert.deepEqual(parseAutoSubmitPhrases("submit!"), ["submit"]);
});

// ── replaceTrailingTranscribedText ──────────────────────────────────────────

test("replaceTrailingTranscribedText_appendsWhenNoPrevious", () => {
  assert.equal(
    replaceTrailingTranscribedText("Hello", "", "world"),
    "Hello world",
  );
});

test("replaceTrailingTranscribedText_appendsToEmptyBase", () => {
  assert.equal(replaceTrailingTranscribedText("", "", "hello"), "hello");
});

test("replaceTrailingTranscribedText_replacesTrailingInterim", () => {
  // Interim "hello wor" is refined to "hello world".
  assert.equal(
    replaceTrailingTranscribedText("hello wor", "hello wor", "hello world"),
    "hello world",
  );
});

test("replaceTrailingTranscribedText_preservesTextTypedBeforeDictation", () => {
  // User typed "Note: " then dictated; the manual prefix must survive.
  assert.equal(
    replaceTrailingTranscribedText("Note: hi", "hi", "hi there"),
    "Note: hi there",
  );
});

test("replaceTrailingTranscribedText_appendsWhenPreviousNoLongerMatches", () => {
  // If the previous transcript isn't the trailing text anymore, append.
  assert.equal(
    replaceTrailingTranscribedText("edited text", "old", "new"),
    "edited text new",
  );
});

test("replaceTrailingTranscribedText_noDoubleSpaceBeforePunctuation", () => {
  assert.equal(
    replaceTrailingTranscribedText("Hello", "", ", world"),
    "Hello, world",
  );
});

// ── getAutoSubmitMatch ──────────────────────────────────────────────────────

test("getAutoSubmitMatch_returnsNullWhenPhraseAbsent", () => {
  assert.equal(
    getAutoSubmitMatch("hello there", parseAutoSubmitPhrases("submit")),
    null,
  );
});

test("getAutoSubmitMatch_returnsNullWhenPhrasesEmpty", () => {
  // DEFAULT_AUTO_SUBMIT_PHRASE is empty (auto-submit disabled by default).
  assert.equal(
    getAutoSubmitMatch(
      "send this message submit",
      parseAutoSubmitPhrases(DEFAULT_AUTO_SUBMIT_PHRASE),
    ),
    null,
  );
});

test("getAutoSubmitMatch_matchesTrailingPhraseAndStripsIt", () => {
  const match = getAutoSubmitMatch(
    "send this message submit",
    parseAutoSubmitPhrases("submit"),
  );
  assert.ok(match);
  assert.equal(match.matchedPhrase, "submit");
  assert.equal(match.textWithoutPhrase, "send this message");
});

test("getAutoSubmitMatch_ignoresPhraseMidSentence", () => {
  // "submit" is not at the end, so it must not auto-send.
  assert.equal(
    getAutoSubmitMatch(
      "submit the form later",
      parseAutoSubmitPhrases("submit"),
    ),
    null,
  );
});

test("getAutoSubmitMatch_requiresWordBoundaryBeforePhrase", () => {
  // "resubmit" ends with "submit" but is not a standalone word → no match.
  assert.equal(
    getAutoSubmitMatch("resubmit", parseAutoSubmitPhrases("submit")),
    null,
  );
});

test("getAutoSubmitMatch_toleratesTrailingPunctuation", () => {
  const match = getAutoSubmitMatch(
    "ship it submit.",
    parseAutoSubmitPhrases("submit"),
  );
  assert.ok(match);
  assert.equal(match.textWithoutPhrase, "ship it");
});

test("getAutoSubmitMatch_matchesMultiWordPhrase", () => {
  const match = getAutoSubmitMatch(
    "please do this send it",
    parseAutoSubmitPhrases("send it"),
  );
  assert.ok(match);
  assert.equal(match.matchedPhrase, "send it");
  assert.equal(match.textWithoutPhrase, "please do this");
});

test("getAutoSubmitMatch_prefersLongestPhrase", () => {
  const match = getAutoSubmitMatch(
    "text please submit now",
    parseAutoSubmitPhrases("submit now, now"),
  );
  assert.ok(match);
  assert.equal(match.matchedPhrase, "submit now");
  assert.equal(match.textWithoutPhrase, "text please");
});
