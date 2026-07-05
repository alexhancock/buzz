import type * as React from "react";
import { useEffect, useRef } from "react";
import { useDictation } from "./useDictation";

interface UseComposerDictationOptions {
  /** Ref to a function that syncs contentRef from the Tiptap editor and returns it. */
  syncContentRef: React.MutableRefObject<() => string>;
  disabledRef: React.MutableRefObject<boolean>;
  isSendingRef: React.MutableRefObject<boolean>;
  isUploadingRef: React.MutableRefObject<boolean>;
  /** Updates contentRef + isContentEmpty state. */
  setComposerContent: (text: string) => void;
  /** Ref to a function that updates the Tiptap editor document. */
  setEditorContentRef: React.MutableRefObject<(text: string) => void>;
  submitMessageRef: React.MutableRefObject<() => void>;
  /** When this key changes (channel/thread switch), active dictation is stopped. */
  draftKey?: string | null;
}

/**
 * Thin wrapper around `useDictation` pre-wired for the MessageComposer's
 * state management (syncContentRef, setComposerContent, editor, submitMessageRef).
 */
export function useComposerDictation({
  syncContentRef,
  disabledRef,
  isSendingRef,
  isUploadingRef,
  setComposerContent,
  setEditorContentRef,
  submitMessageRef,
  draftKey,
}: UseComposerDictationOptions) {
  const isSendBlockedRef = useRef(false);
  isSendBlockedRef.current =
    disabledRef.current || isSendingRef.current || isUploadingRef.current;

  const dictation = useDictation({
    getText: () => syncContentRef.current(),
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
    isSendBlockedRef,
  });

  // Stop dictation when the channel/thread changes so that transcript events
  // from a stale WebRTC session don't leak into the wrong draft.
  // biome-ignore lint/correctness/useExhaustiveDependencies: draftKey is the sole trigger
  useEffect(() => {
    dictation.stopRecording();
  }, [draftKey]);

  return dictation;
}
