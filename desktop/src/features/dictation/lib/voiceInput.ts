export const DEFAULT_AUTO_SUBMIT_PHRASE = "submit";

const TRAILING_PUNCTUATION_REGEX = /[\s"'`.,!?;:)\]}]+$/u;

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function normalizePhrase(value: string): string {
  return value
    .toLowerCase()
    .replace(/\s+/g, " ")
    .trim()
    .replace(TRAILING_PUNCTUATION_REGEX, "")
    .trim();
}

export function parseAutoSubmitPhrases(
  rawValue: string | null | undefined,
): string[] {
  if (!rawValue) return [];
  return Array.from(
    new Set(
      rawValue
        .split(",")
        .map((value) => normalizePhrase(value))
        .filter(Boolean),
    ),
  );
}

function appendTranscribedText(baseText: string, fragment: string): string {
  const normalizedFragment = fragment.replace(/\s+/g, " ").trim();
  if (!normalizedFragment) return baseText;
  if (!baseText.trim()) return normalizedFragment;
  if (/[\s([{/-]$/.test(baseText) || /^[,.;!?)]/.test(normalizedFragment)) {
    return `${baseText}${normalizedFragment}`;
  }
  return `${baseText} ${normalizedFragment}`;
}

export function replaceTrailingTranscribedText(
  fullText: string,
  previousTranscribedText: string,
  nextTranscribedText: string,
): string {
  if (!previousTranscribedText) {
    return appendTranscribedText(fullText, nextTranscribedText);
  }

  if (fullText.endsWith(previousTranscribedText)) {
    return appendTranscribedText(
      fullText.slice(0, -previousTranscribedText.length),
      nextTranscribedText,
    );
  }

  const trimmedPreviousText = previousTranscribedText.trim();
  if (trimmedPreviousText && fullText.endsWith(trimmedPreviousText)) {
    return appendTranscribedText(
      fullText.slice(0, -trimmedPreviousText.length),
      nextTranscribedText,
    );
  }

  return appendTranscribedText(fullText, nextTranscribedText);
}

export function getAutoSubmitMatch(
  transcribedText: string,
  autoSubmitPhrases: string[],
): { matchedPhrase: string; textWithoutPhrase: string } | null {
  const normalizedTranscribedText = normalizePhrase(transcribedText);
  if (!normalizedTranscribedText) return null;

  const sortedPhrases = [...autoSubmitPhrases].sort(
    (left, right) => right.length - left.length,
  );

  for (const phrase of sortedPhrases) {
    if (!normalizedTranscribedText.endsWith(phrase)) continue;

    const phraseStartIndex = normalizedTranscribedText.length - phrase.length;
    if (
      phraseStartIndex > 0 &&
      normalizedTranscribedText[phraseStartIndex - 1] !== " "
    ) {
      continue;
    }

    const trimmedText = transcribedText.replace(TRAILING_PUNCTUATION_REGEX, "");
    const phraseWords = phrase.split(" ").filter(Boolean).map(escapeRegExp);
    const phrasePattern = new RegExp(
      `(^|\\s)(${phraseWords.join("\\s+")})\\s*$`,
      "iu",
    );
    const rawMatch = trimmedText.match(phrasePattern);
    const phraseStartOffset =
      rawMatch && rawMatch.index !== undefined
        ? rawMatch.index + (rawMatch[1]?.length ?? 0)
        : trimmedText.length - phrase.length;
    const textWithoutPhrase = trimmedText.slice(0, phraseStartOffset).trimEnd();

    return { matchedPhrase: phrase, textWithoutPhrase };
  }

  return null;
}
