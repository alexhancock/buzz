import type * as React from "react";
import { useEffect, useId, useRef } from "react";
import {
  clearActiveDictationComposer,
  isActiveDictationComposer,
  setActiveDictationComposer,
} from "../lib/activeComposer";
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
  /** Ref to the composer's container element for focus tracking. */
  composerRef?: React.RefObject<HTMLElement | null>;
}

/**
 * Thin wrapper around `useDictation` pre-wired for the MessageComposer's
 * state management (syncContentRef, setComposerContent, editor, submitMessageRef).
 *
 * Uses the local Parakeet STT engine — fully offline, no relay or API key needed.
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
  composerRef,
}: UseComposerDictationOptions) {
  const instanceId = useId();
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

  // Track which composer is active (most recently focused) so that the global
  // ⌘D shortcut only dispatches to one instance when multiple are mounted.
  useEffect(() => {
    const el = composerRef?.current;
    if (!el) {
      // If no ref provided, this composer is always considered active (single-composer case).
      setActiveDictationComposer(instanceId);
      return () => clearActiveDictationComposer(instanceId);
    }

    function handleFocusIn() {
      setActiveDictationComposer(instanceId);
    }

    el.addEventListener("focusin", handleFocusIn);
    return () => {
      el.removeEventListener("focusin", handleFocusIn);
      clearActiveDictationComposer(instanceId);
    };
  }, [instanceId, composerRef]);

  // Cancel dictation when the channel/thread changes so that transcript events
  // from a stale local STT session don't leak into the wrong draft.
  // Only cancel if this instance is actually recording — avoids killing another
  // composer's session since the native engine is a singleton.
  const isRecordingRef = useRef(false);
  isRecordingRef.current = dictation.isRecording || dictation.isStarting;
  // biome-ignore lint/correctness/useExhaustiveDependencies: draftKey is the sole trigger
  useEffect(() => {
    if (isRecordingRef.current) {
      dictation.cancelRecording();
    }
  }, [draftKey]);

  // Auto-cancel dictation when the composer becomes disabled mid-recording
  // (e.g. channel becomes read-only, parent send state disables thread composer).
  // Without this, the STT session keeps running with no way to stop it.
  useEffect(() => {
    if (disabled && dictation.isRecording) {
      dictation.cancelRecording();
    }
  }, [disabled, dictation.isRecording, dictation.cancelRecording]);

  // ⌘D push-to-talk — hold to record, release to stop.
  // Dispatched from AppShell's keydown/keyup handlers.
  // Only the active (most recently focused) composer responds, and only when
  // not disabled/send-blocked.
  // biome-ignore lint/correctness/useExhaustiveDependencies: disabledRef/isSendBlockedRef are stable refs read at call time
  useEffect(() => {
    function handleKeyDown() {
      // Only respond if this is the active composer instance.
      if (!isActiveDictationComposer(instanceId)) return;
      // Don't start dictation in disabled/blocked composers.
      if (disabledRef.current || isSendBlockedRef.current) return;
      if (!dictation.isRecording && !dictation.isStarting) {
        dictation.startRecording();
      }
    }
    function handleKeyUp() {
      if (dictation.isRecording || dictation.isStarting) {
        dictation.stopRecording();
      }
    }
    window.addEventListener("buzz:dictation-key-down", handleKeyDown);
    window.addEventListener("buzz:dictation-key-up", handleKeyUp);
    return () => {
      window.removeEventListener("buzz:dictation-key-down", handleKeyDown);
      window.removeEventListener("buzz:dictation-key-up", handleKeyUp);
    };
  }, [
    instanceId,
    dictation.isRecording,
    dictation.isStarting,
    dictation.startRecording,
    dictation.stopRecording,
  ]);

  return dictation;
}
