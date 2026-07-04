import type * as React from "react";
import { useDictation } from "./useDictation";

interface UseComposerDictationOptions {
  contentRef: React.MutableRefObject<string>;
  disabled: boolean;
  isSending: boolean;
  /** Updates contentRef + isContentEmpty state. */
  setComposerContent: (text: string) => void;
  /** Ref to a function that updates the Tiptap editor document. */
  setEditorContentRef: React.MutableRefObject<(text: string) => void>;
  submitMessageRef: React.MutableRefObject<() => void>;
}

/**
 * Thin wrapper around `useDictation` pre-wired for the MessageComposer's
 * state management (contentRef, setComposerContent, editor, submitMessageRef).
 */
export function useComposerDictation({
  contentRef,
  disabled,
  isSending,
  setComposerContent,
  setEditorContentRef,
  submitMessageRef,
}: UseComposerDictationOptions) {
  return useDictation({
    text: contentRef.current,
    setText: (text) => {
      setComposerContent(text);
      setEditorContentRef.current(text);
    },
    onSend: (text) => {
      setComposerContent(text);
      setEditorContentRef.current(text);
      // Submit synchronously — the content ref is already set above, so
      // syncComposerContentFromEditor() will serialize the editor which now
      // holds the dictated text.
      submitMessageRef.current();
    },
    sendDisabled: disabled || isSending,
  });
}
