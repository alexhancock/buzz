import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import {
  createTranscribeSession,
  getTranscribeStatus,
} from "../api/transcribeSession";
import {
  type AudioBufferCapture,
  type TranscriptEvent,
  type TranscriptSegmentState,
  TRANSCRIPT_COMPLETED_EVENT,
  TRANSCRIPT_DELTA_EVENT,
  connectPeerConnection,
  createAudioBufferCapture,
  createPeerConnection,
  createTranscriptSegmentState,
  flushAudioBuffer,
  getTranscriptText,
  mergeTranscriptEvent,
} from "../lib/realtimeAudio";

interface UseRealtimeDictationOptions {
  disabled?: boolean;
  onRecordingStart?: () => void;
  onTranscriptText: (text: string) => void;
}

function closeResources(resources: {
  audioCapture?: AudioBufferCapture | null;
  dataChannel?: RTCDataChannel | null;
  peerConnection?: RTCPeerConnection | null;
  stream?: MediaStream | null;
}) {
  resources.audioCapture?.close();
  resources.dataChannel?.close();
  resources.peerConnection?.close();
  for (const track of resources.stream?.getTracks() ?? []) {
    track.stop();
  }
}

export function useRealtimeDictation({
  disabled = false,
  onRecordingStart,
  onTranscriptText,
}: UseRealtimeDictationOptions) {
  const [isRecording, setIsRecording] = useState(false);
  const [isStarting, setIsStarting] = useState(false);
  const [isTranscribing, setIsTranscribing] = useState(false);
  const [isConfigured, setIsConfigured] = useState(false);

  const peerConnectionRef = useRef<RTCPeerConnection | null>(null);
  const dataChannelRef = useRef<RTCDataChannel | null>(null);
  const streamRef = useRef<MediaStream | null>(null);
  const audioCaptureRef = useRef<AudioBufferCapture | null>(null);
  const segmentStateRef = useRef<TranscriptSegmentState>(
    createTranscriptSegmentState(),
  );
  const activeRunIdRef = useRef(0);
  const onRecordingStartRef = useRef(onRecordingStart);
  const onTranscriptTextRef = useRef(onTranscriptText);

  onRecordingStartRef.current = onRecordingStart;
  onTranscriptTextRef.current = onTranscriptText;

  const isEnabled = !disabled && isConfigured;

  // Check if transcription is configured on mount
  useEffect(() => {
    let cancelled = false;
    getTranscribeStatus()
      .then((status) => {
        if (!cancelled) setIsConfigured(status.configured);
      })
      .catch(() => {
        if (!cancelled) setIsConfigured(false);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const cleanupResources = useCallback(() => {
    activeRunIdRef.current += 1;
    closeResources({
      audioCapture: audioCaptureRef.current,
      dataChannel: dataChannelRef.current,
      peerConnection: peerConnectionRef.current,
      stream: streamRef.current,
    });
    audioCaptureRef.current = null;
    dataChannelRef.current = null;
    peerConnectionRef.current = null;
    streamRef.current = null;
  }, []);

  const cleanup = useCallback(() => {
    cleanupResources();
    setIsRecording(false);
    setIsStarting(false);
    setIsTranscribing(false);
  }, [cleanupResources]);

  useEffect(() => cleanupResources, [cleanupResources]);

  const handleRealtimeEvent = useCallback((event: TranscriptEvent) => {
    if (event.type === "error") {
      console.error("OpenAI realtime server error", event);
      toast.error(event.error?.message ?? "Voice input error");
      return;
    }

    if (
      event.type !== TRANSCRIPT_DELTA_EVENT &&
      event.type !== TRANSCRIPT_COMPLETED_EVENT
    ) {
      return;
    }

    const prevText = getTranscriptText(segmentStateRef.current);
    const merged = mergeTranscriptEvent(segmentStateRef.current, event);

    if (merged === prevText) return;

    onTranscriptTextRef.current(merged);
    setIsTranscribing(event.type !== TRANSCRIPT_COMPLETED_EVENT);
  }, []);

  const startRecording = useCallback(async () => {
    if (!isEnabled || isStarting || isRecording) return;

    const runId = activeRunIdRef.current + 1;
    activeRunIdRef.current = runId;
    const isStaleRun = () => activeRunIdRef.current !== runId;

    let stream: MediaStream | null = null;
    let audioCapture: AudioBufferCapture | null = null;
    let peerConnection: RTCPeerConnection | null = null;
    let dataChannel: RTCDataChannel | null = null;

    setIsStarting(true);
    segmentStateRef.current = createTranscriptSegmentState();
    onRecordingStartRef.current?.();

    try {
      // 1. Capture mic immediately for instant feedback
      stream = await navigator.mediaDevices.getUserMedia({
        audio: {
          autoGainControl: true,
          echoCancellation: true,
          noiseSuppression: true,
        },
      });
      if (isStaleRun()) {
        closeResources({ stream });
        return;
      }
      streamRef.current = stream;
      setIsRecording(true);

      // 2. Buffer PCM via AudioWorklet while network calls proceed
      audioCapture = await createAudioBufferCapture(stream);
      if (isStaleRun()) {
        closeResources({ audioCapture, stream });
        return;
      }
      audioCaptureRef.current = audioCapture;

      // 3. Create session via relay
      const session = await createTranscribeSession();
      if (isStaleRun()) {
        closeResources({ audioCapture, stream });
        return;
      }

      // 4. Set up WebRTC
      peerConnection = createPeerConnection();
      peerConnectionRef.current = peerConnection;
      const activeStream = stream;
      stream.getAudioTracks().forEach((track) => {
        peerConnection?.addTrack(track, activeStream);
      });

      dataChannel = peerConnection.createDataChannel("oai-events");
      dataChannelRef.current = dataChannel;
      dataChannel.addEventListener("message", (message) => {
        try {
          handleRealtimeEvent(JSON.parse(String(message.data)));
        } catch {
          // Ignore non-JSON events
        }
      });

      // Flush buffered audio once data channel opens
      const channelToFlush = dataChannel;
      const captureToFlush = audioCapture;
      dataChannel.addEventListener("open", () => {
        // If the user stopped (or restarted) recording between the SDP
        // exchange and the channel opening, drop this run's buffered audio.
        if (isStaleRun()) {
          captureToFlush.close();
          return;
        }
        flushAudioBuffer(channelToFlush, captureToFlush.chunks);
        captureToFlush.close();
        audioCaptureRef.current = null;
      });

      // 5. SDP exchange
      await connectPeerConnection({
        peerConnection,
        clientSecret: session.clientSecret,
      });
      if (isStaleRun()) {
        closeResources({ audioCapture, dataChannel, peerConnection, stream });
        return;
      }
    } catch (error) {
      closeResources({ audioCapture, dataChannel, peerConnection, stream });
      if (!isStaleRun()) {
        audioCaptureRef.current = null;
        dataChannelRef.current = null;
        peerConnectionRef.current = null;
        streamRef.current = null;
        setIsRecording(false);
        setIsTranscribing(false);

        const message =
          error instanceof Error ? error.message : "Voice input failed";
        if (/not allowed|denied|permission/i.test(message)) {
          toast.error("Microphone access denied", {
            description:
              "Allow microphone access in System Settings to use dictation.",
          });
        } else if (/not found|no audio/i.test(message)) {
          toast.error("No microphone found", {
            description: "Connect a microphone and try again.",
          });
        } else {
          toast.error("Voice input failed", { description: message });
        }
      }
    } finally {
      if (!isStaleRun()) setIsStarting(false);
    }
  }, [handleRealtimeEvent, isEnabled, isRecording, isStarting]);

  const stopRecording = useCallback(() => cleanup(), [cleanup]);

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
    toggleRecording,
  };
}
