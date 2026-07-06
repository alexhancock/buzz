import type * as React from "react";
import { useEffect, useRef } from "react";
import { useDictation } from "./useDictation";

interface UseComposerDictationOptions {
  /** Ref to a function that syncs contentRef from the Tiptap editor and returns it. */
  syncContentRef: React.MutableRefObject<() => string>;
  /** Whether the composer is currently disabled (read-only, etc.). */
  disabled?: boolean;
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
  disabled = false,
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

  // Cancel dictation when the channel/thread changes so that transcript events
  // from a stale WebRTC session don't leak into the wrong draft.
  // biome-ignore lint/correctness/useExhaustiveDependencies: draftKey is the sole trigger
  useEffect(() => {
    dictation.cancelRecording();
  }, [draftKey]);

  // Auto-cancel dictation when the composer becomes disabled mid-recording
  // (e.g. channel becomes read-only, parent send state disables thread composer).
  // Without this, the WebRTC session keeps running with no way to stop it.
  useEffect(() => {
    if (disabled && dictation.isRecording) {
      dictation.cancelRecording();
    }
  }, [disabled, dictation.isRecording, dictation.cancelRecording]);

  return dictation;
}
