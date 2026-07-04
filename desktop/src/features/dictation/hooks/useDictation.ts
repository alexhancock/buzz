import { useCallback, useMemo, useRef } from "react";
import {
  DEFAULT_AUTO_SUBMIT_PHRASE,
  getAutoSubmitMatch,
  parseAutoSubmitPhrases,
  replaceTrailingTranscribedText,
} from "../lib/voiceInput";
import { useRealtimeDictation } from "./useRealtimeDictation";

interface UseDictationOptions {
  /** Current composer text */
  text: string;
  /** Set composer text */
  setText: (value: string) => void;
  /** Send the message */
  onSend: (text: string) => void;
  /** Whether sending is currently blocked */
  sendDisabled?: boolean;
}

export function useDictation({
  text,
  setText,
  onSend,
  sendDisabled = false,
}: UseDictationOptions) {
  const autoSubmitPhrases = useMemo(
    () => parseAutoSubmitPhrases(DEFAULT_AUTO_SUBMIT_PHRASE),
    [],
  );
  const stopRecordingRef = useRef<() => void>(() => {});
  const textRef = useRef(text);
  textRef.current = text;
  const lastTranscriptRef = useRef("");

  const handleTranscript = useCallback(
    (transcript: string) => {
      const previous = lastTranscriptRef.current;
      const latest = textRef.current;
      const merged = replaceTrailingTranscribedText(
        latest,
        previous,
        transcript,
      );
      const match = getAutoSubmitMatch(transcript, autoSubmitPhrases);

      if (!match) {
        setText(merged);
        textRef.current = merged;
        lastTranscriptRef.current = transcript;
        return;
      }

      const textWithoutPhrase = replaceTrailingTranscribedText(
        latest,
        previous,
        match.textWithoutPhrase,
      );
      if (!textWithoutPhrase.trim()) return;

      stopRecordingRef.current();

      if (sendDisabled) {
        setText(textWithoutPhrase);
        textRef.current = textWithoutPhrase;
        return;
      }

      onSend(textWithoutPhrase.trim());
      setText("");
      textRef.current = "";
      lastTranscriptRef.current = "";
    },
    [autoSubmitPhrases, onSend, sendDisabled, setText],
  );

  const dictation = useRealtimeDictation({
    onRecordingStart: () => {
      lastTranscriptRef.current = "";
    },
    onTranscriptText: handleTranscript,
  });
  stopRecordingRef.current = dictation.stopRecording;

  return dictation;
}
