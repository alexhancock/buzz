import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";

/**
 * Raw binary invoke — uses Tauri's internal IPC for zero-copy ArrayBuffer transfer.
 * Same pattern as huddle's audioWorklet.ts.
 */
function invokeRawBinary(cmd: string, payload: Uint8Array): Promise<unknown> {
  // biome-ignore lint/suspicious/noExplicitAny: Tauri internals have no public type definition
  const internals = (window as any).__TAURI_INTERNALS__;
  if (!internals?.invoke) {
    return Promise.reject(new Error("Tauri internals not available"));
  }
  return internals.invoke(cmd, payload);
}

interface UseLocalDictationOptions {
  disabled?: boolean;
  onRecordingStart?: () => void;
  onTranscriptText: (text: string) => void;
}

interface DictationStatus {
  available: boolean;
  active: boolean;
}

const DICTATION_TRANSCRIPT_EVENT = "dictation-transcript";
const DICTATION_STATE_EVENT = "dictation-state";

/** Interval (ms) to poll model availability after initial unavailability. */
const MODEL_POLL_INTERVAL_MS = 5_000;

/**
 * Batching interval for audio IPC (ms). The worklet posts every render quantum
 * (~2.67ms at 48kHz/128 samples). Sending each one individually overloads IPC.
 * We accumulate ~100ms of audio before sending to reduce IPC overhead from
 * ~375 calls/s to ~10 calls/s.
 */
const AUDIO_BATCH_MS = 100;

/**
 * Local STT dictation hook using the Parakeet model via Tauri native commands.
 *
 * Works fully offline — no relay or OpenAI API key needed. Uses the same
 * sherpa-onnx Parakeet TDT-CTC 110M model as huddle transcription.
 *
 * Audio capture uses the Web Audio API (AudioWorklet) on the frontend side,
 * then sends batched raw PCM bytes to the native STT engine via
 * `push_dictation_audio`.
 */
export function useLocalDictation({
  disabled = false,
  onRecordingStart,
  onTranscriptText,
}: UseLocalDictationOptions) {
  const [isRecording, setIsRecording] = useState(false);
  const [isStarting, setIsStarting] = useState(false);
  const [isTranscribing, setIsTranscribing] = useState(false);
  const [isAvailable, setIsAvailable] = useState(false);

  const streamRef = useRef<MediaStream | null>(null);
  const audioContextRef = useRef<AudioContext | null>(null);
  const workletRef = useRef<AudioWorkletNode | null>(null);
  const batchTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const audioBatchRef = useRef<Float32Array[]>([]);
  const unlistenTranscriptRef = useRef<UnlistenFn | null>(null);
  const unlistenStateRef = useRef<UnlistenFn | null>(null);
  const onRecordingStartRef = useRef(onRecordingStart);
  const onTranscriptTextRef = useRef(onTranscriptText);

  onRecordingStartRef.current = onRecordingStart;
  onTranscriptTextRef.current = onTranscriptText;

  const isEnabled = !disabled && isAvailable;

  // Check availability on mount and poll until available (model may be downloading).
  useEffect(() => {
    let cancelled = false;
    let pollTimer: ReturnType<typeof setInterval> | null = null;

    function checkAvailability() {
      invoke<DictationStatus>("get_dictation_status")
        .then((status) => {
          if (cancelled) return;
          setIsAvailable(status.available);
          // Stop polling once the model is ready.
          if (status.available && pollTimer) {
            clearInterval(pollTimer);
            pollTimer = null;
          }
        })
        .catch(() => {
          if (!cancelled) setIsAvailable(false);
        });
    }

    checkAvailability();

    // Poll periodically until available (handles background model download).
    pollTimer = setInterval(() => {
      if (cancelled) return;
      checkAvailability();
    }, MODEL_POLL_INTERVAL_MS);

    return () => {
      cancelled = true;
      if (pollTimer) clearInterval(pollTimer);
    };
  }, []);

  /** Flush accumulated audio batch to the native STT engine. Returns a promise
   *  that resolves once the IPC call completes (or immediately if nothing to flush). */
  const flushAudioBatch = useCallback((): Promise<void> => {
    const batch = audioBatchRef.current;
    if (batch.length === 0) return Promise.resolve();

    // Calculate total byte length and merge into a single buffer.
    let totalSamples = 0;
    for (const chunk of batch) {
      totalSamples += chunk.length;
    }
    const merged = new Float32Array(totalSamples);
    let offset = 0;
    for (const chunk of batch) {
      merged.set(chunk, offset);
      offset += chunk.length;
    }
    audioBatchRef.current = [];

    const bytes = new Uint8Array(
      merged.buffer,
      merged.byteOffset,
      merged.byteLength,
    );
    return invokeRawBinary("push_dictation_audio", bytes)
      .then(() => {})
      .catch(() => {});
  }, []);

  const cleanup = useCallback(() => {
    // Flush any remaining audio before teardown, then stop the native engine.
    void flushAudioBatch().then(() => {
      invoke("stop_dictation").catch(() => {});
    });
    // Stop batch timer.
    if (batchTimerRef.current) {
      clearInterval(batchTimerRef.current);
      batchTimerRef.current = null;
    }
    // Stop mic.
    if (streamRef.current) {
      for (const track of streamRef.current.getTracks()) {
        track.stop();
      }
      streamRef.current = null;
    }
    // Disconnect audio worklet.
    if (workletRef.current) {
      workletRef.current.disconnect();
      workletRef.current = null;
    }
    // Close audio context.
    if (audioContextRef.current) {
      void audioContextRef.current.close();
      audioContextRef.current = null;
    }
    // Unlisten events.
    if (unlistenTranscriptRef.current) {
      unlistenTranscriptRef.current();
      unlistenTranscriptRef.current = null;
    }
    if (unlistenStateRef.current) {
      unlistenStateRef.current();
      unlistenStateRef.current = null;
    }
  }, [flushAudioBatch]);

  // Cleanup on unmount.
  useEffect(() => cleanup, [cleanup]);

  const startRecording = useCallback(async () => {
    if (!isEnabled || isStarting || isRecording) return;

    setIsStarting(true);
    onRecordingStartRef.current?.();

    try {
      // 1. Listen for transcript events from the native layer.
      const unlistenTranscript = await listen<string>(
        DICTATION_TRANSCRIPT_EVENT,
        (event) => {
          if (event.payload) {
            onTranscriptTextRef.current(event.payload);
          }
        },
      );
      unlistenTranscriptRef.current = unlistenTranscript;

      const unlistenState = await listen<string>(
        DICTATION_STATE_EVENT,
        (event) => {
          if (event.payload === "stopped") {
            setIsRecording(false);
            setIsTranscribing(false);
            // Clean up event listeners now that the session is fully done.
            if (unlistenTranscriptRef.current) {
              unlistenTranscriptRef.current();
              unlistenTranscriptRef.current = null;
            }
            if (unlistenStateRef.current) {
              unlistenStateRef.current();
              unlistenStateRef.current = null;
            }
          }
        },
      );
      unlistenStateRef.current = unlistenState;

      // 2. Start the native STT engine.
      await invoke("start_dictation");

      // 3. Capture mic audio.
      const stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          autoGainControl: true,
          echoCancellation: true,
          noiseSuppression: true,
        },
      });
      streamRef.current = stream;

      // 4. Set up AudioWorklet to send PCM to native layer.
      const audioContext = new AudioContext({ sampleRate: 48000 });
      audioContextRef.current = audioContext;

      // Create a processor that accumulates audio frames and posts them
      // to the main thread. Batching happens on the main thread side via
      // a timer to reduce IPC overhead.
      const processorCode = `
        class DictationProcessor extends AudioWorkletProcessor {
          process(inputs) {
            const input = inputs[0];
            if (input && input[0] && input[0].length > 0) {
              this.port.postMessage(input[0].buffer);
            }
            return true;
          }
        }
        registerProcessor('dictation-processor', DictationProcessor);
      `;
      const blob = new Blob([processorCode], {
        type: "application/javascript",
      });
      const blobUrl = URL.createObjectURL(blob);
      try {
        await audioContext.audioWorklet.addModule(blobUrl);
      } finally {
        URL.revokeObjectURL(blobUrl);
      }

      const source = audioContext.createMediaStreamSource(stream);
      const worklet = new AudioWorkletNode(audioContext, "dictation-processor");
      workletRef.current = worklet;

      // Accumulate audio frames in a batch array; a timer flushes to native.
      worklet.port.onmessage = (event: MessageEvent<ArrayBuffer>) => {
        audioBatchRef.current.push(new Float32Array(event.data));
      };

      // Start the batch flush timer (~10 IPC calls/s instead of ~375).
      batchTimerRef.current = setInterval(flushAudioBatch, AUDIO_BATCH_MS);

      source.connect(worklet);
      // Don't connect to destination — we only capture, not play back.
      worklet.connect(audioContext.createGain()); // keep worklet alive without audible output

      setIsRecording(true);
      setIsTranscribing(true);
    } catch (error) {
      // Stop the native engine if it was started but a later step failed
      // (e.g. mic permission denied, AudioWorklet setup error).
      invoke("stop_dictation").catch(() => {});
      cleanup();
      setIsRecording(false);
      setIsTranscribing(false);

      const message =
        error instanceof Error ? error.message : "Local dictation failed";
      if (/not allowed|denied|permission/i.test(message)) {
        toast.error("Microphone access denied", {
          description:
            "Allow microphone access in System Settings to use dictation.",
        });
      } else if (/not found|no audio/i.test(message)) {
        toast.error("No microphone found", {
          description: "Connect a microphone and try again.",
        });
      } else if (/model not ready/i.test(message)) {
        toast.error("Voice model downloading", {
          description:
            "The speech model is still downloading. Try again shortly.",
        });
      } else {
        toast.error("Dictation failed", { description: message });
      }
    } finally {
      setIsStarting(false);
    }
  }, [cleanup, flushAudioBatch, isEnabled, isRecording, isStarting]);

  const stopRecording = useCallback(() => {
    // Stop batch timer immediately.
    if (batchTimerRef.current) {
      clearInterval(batchTimerRef.current);
      batchTimerRef.current = null;
    }
    // Stop mic and audio pipeline immediately so the user gets visual feedback.
    if (streamRef.current) {
      for (const track of streamRef.current.getTracks()) {
        track.stop();
      }
      streamRef.current = null;
    }
    if (workletRef.current) {
      workletRef.current.disconnect();
      workletRef.current = null;
    }
    if (audioContextRef.current) {
      void audioContextRef.current.close();
      audioContextRef.current = null;
    }
    setIsRecording(false);
    // Flush remaining audio and THEN stop the native engine, ensuring the
    // final batch arrives before the engine shuts down and flushes its buffer.
    // isTranscribing stays true — cleared when `dictation-state: stopped` arrives.
    void flushAudioBatch().then(() => {
      invoke("stop_dictation").catch(() => {});
    });
  }, [flushAudioBatch]);

  const cancelRecording = useCallback(() => {
    cleanup();
    invoke("stop_dictation").catch(() => {});
    setIsRecording(false);
    setIsTranscribing(false);
  }, [cleanup]);

  const toggleRecording = useCallback(() => {
    if (isRecording || isStarting) {
      stopRecording();
      return;
    }
    void startRecording();
  }, [isRecording, isStarting, startRecording, stopRecording]);

  return {
    isEnabled,
    isRecording,
    isStarting,
    isTranscribing,
    startRecording,
    stopRecording,
    cancelRecording,
    toggleRecording,
  };
}
