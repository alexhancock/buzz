import type * as React from "react";
import { useDictation } from "./useDictation";

interface UseComposerDictationOptions {
  contentRef: React.MutableRefObject<string>;
  disabled: boolean;
  isSending: boolean;
  setComposerContentFromText: (text: string) => void;
  submitMessageRef: React.MutableRefObject<() => void>;
}

/**
 * Thin wrapper around `useDictation` pre-wired for the MessageComposer's
 * state management (contentRef, setComposerContentFromText, submitMessageRef).
 */
export function useComposerDictation({
  contentRef,
  disabled,
  isSending,
  setComposerContentFromText,
  submitMessageRef,
}: UseComposerDictationOptions) {
  return useDictation({
    text: contentRef.current,
    setText: setComposerContentFromText,
    onSend: (text) => {
      setComposerContentFromText(text);
      queueMicrotask(() => submitMessageRef.current());
    },
    sendDisabled: disabled || isSending,
  });
}
