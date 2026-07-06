//! Reusable Speech-to-Text engine backed by sherpa-onnx (Parakeet TDT-CTC 110M).
//!
//! This module extracts the core STT logic — resample, VAD, inference — into a
//! standalone component that can be instantiated by both the huddle pipeline and
//! the composer dictation feature with different configurations.
//!
//! ```text
//! Caller (48 kHz f32 PCM)
//!   → SttEngine::push_audio  [bounded sync_channel]
//!   → stt_worker thread
//!       rubato: 48 kHz → 16 kHz mono
//!       earshot VAD: accumulate speech frames
//!       sherpa-onnx Parakeet TDT-CTC 110M: transcribe on silence
//!   → text_rx  [tokio mpsc channel]
//!   → caller's async task
//! ```
//!
//! The worker runs on a dedicated `std::thread` (not async) because
//! sherpa-onnx is CPU-bound and not Send-safe across await points.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    thread,
    time::Duration,
};

use tokio::sync::mpsc as tokio_mpsc;

// ── Configuration ─────────────────────────────────────────────────────────────

/// Default silence frames before flush (~300ms at 16 kHz / 256 samples per frame).
/// Used by the huddle pipeline.
pub const DEFAULT_SILENCE_FLUSH_FRAMES: usize = 19;

/// Silence frames for dictation (~400ms) — slightly longer for more coherent sentences.
pub const DICTATION_SILENCE_FLUSH_FRAMES: usize = 25;

/// Periodic partial flush interval for dictation streaming (~2 seconds of speech
/// at 16 kHz). When speech exceeds this threshold without a silence gap, the engine
/// flushes a partial transcript so the user sees text appearing on-the-fly.
pub const DICTATION_PARTIAL_FLUSH_SAMPLES: usize = 16_000 * 2;

/// Default maximum speech buffer: 30 seconds at 16 kHz.
pub const DEFAULT_MAX_SPEECH_SAMPLES: usize = 16_000 * 30;

/// Configuration for the STT engine.
///
/// Allows callers to tune VAD behavior and optionally wire in TTS/PTT flags
/// for huddle-specific features (barge-in, echo gating, push-to-talk).
#[derive(Clone)]
pub struct SttEngineConfig {
    /// Path to the directory containing `model.int8.onnx` and `tokens.txt`.
    pub model_dir: PathBuf,
    /// Number of consecutive silence frames before flushing to STT.
    /// ~300ms = 19 frames, ~400ms = 25 frames at 16 kHz / 256 samples per frame.
    pub silence_flush_frames: usize,
    /// Maximum speech buffer size in samples (OOM guard).
    pub max_speech_samples: usize,
    /// Optional: when set, the engine flushes a partial transcript every N samples
    /// of continuous speech (even without a silence gap). This enables on-the-fly
    /// transcription for dictation. Set to `None` for huddle (silence-only flush).
    pub partial_flush_samples: Option<usize>,
    /// Optional: shared flag set by TTS while audio is playing (echo prevention).
    pub tts_active: Option<Arc<AtomicBool>>,
    /// Optional: TTS cancel flag — set by STT on barge-in detection.
    pub tts_cancel: Option<Arc<AtomicBool>>,
    /// Optional: push-to-talk flag — when `Some`, speech is only accumulated
    /// while the flag is true.
    pub ptt_active: Option<Arc<AtomicBool>>,
}

// ── Public engine handle ──────────────────────────────────────────────────────

/// Bounded audio queue capacity.
/// 100 ms batches at 48 kHz ≈ 19 KB each → 50 slots ≈ 5 s / ~1 MB max backlog.
const AUDIO_QUEUE_DEPTH: usize = 50;

/// Handle to the running STT engine.
///
/// Not Clone — wrap in `Arc` to share across threads.
///
/// The text receiver (`tokio::sync::mpsc::Receiver<String>`) is returned
/// separately from `new()` so the caller can move it directly into an async
/// task without holding a Mutex across await points.
#[derive(Debug)]
pub(crate) struct SttEngine {
    /// Send raw PCM bytes (f32 LE, 48 kHz mono) into the engine.
    audio_tx: SyncSender<Vec<u8>>,
    /// Signals the worker thread to stop.
    shutdown: Arc<AtomicBool>,
    /// Worker thread handle — taken on drop to join cleanly.
    thread: Option<thread::JoinHandle<()>>,
}

impl SttEngine {
    /// Spawn the engine worker thread.
    ///
    /// Returns `(Self, Receiver<String>)`. The receiver yields transcribed text
    /// segments. It is returned separately so the caller can move it into an
    /// async task without holding a Mutex.
    ///
    /// Returns `Err` only if the thread cannot be spawned (OS error).
    /// If model files are missing, the worker logs and exits cleanly —
    /// the engine handle is still returned but will never produce text.
    pub fn new(config: SttEngineConfig) -> Result<(Self, tokio_mpsc::Receiver<String>), String> {
        let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE_DEPTH);
        let (text_tx, text_rx) = tokio_mpsc::channel::<String>(64);
        let shutdown = Arc::new(AtomicBool::new(false));

        let shutdown_worker = Arc::clone(&shutdown);
        let handle = thread::Builder::new()
            .name("stt-engine".into())
            .spawn(move || {
                stt_worker(config, audio_rx, text_tx, shutdown_worker);
            })
            .map_err(|e| format!("failed to spawn stt-engine thread: {e}"))?;

        let engine = Self {
            audio_tx,
            shutdown,
            thread: Some(handle),
        };
        Ok((engine, text_rx))
    }

    /// Signal the worker thread to stop.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Returns `true` if the worker thread has exited (init failure, crash, or normal exit).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Feed raw PCM bytes (f32 LE, 48 kHz mono) into the engine.
    ///
    /// Non-blocking. Drops audio silently if the engine can't keep up —
    /// better to lose frames than to stall the caller.
    pub fn push_audio(&self, pcm_bytes: Vec<u8>) -> Result<(), String> {
        if !pcm_bytes.len().is_multiple_of(4) {
            return Err(format!(
                "audio input not 4-byte aligned ({} bytes) — expected f32 LE samples",
                pcm_bytes.len()
            ));
        }
        let _ = self.audio_tx.try_send(pcm_bytes);
        Ok(())
    }
}

impl Drop for SttEngine {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ── Shared utility ────────────────────────────────────────────────────────────

/// Drain and discard all pending messages until shutdown or disconnect.
///
/// Used by both the STT and TTS worker threads for graceful degradation
/// when model files are missing or initialization fails.
pub(crate) fn drain_until_shutdown<T>(rx: std::sync::mpsc::Receiver<T>, shutdown: &AtomicBool) {
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

// ── Worker thread ─────────────────────────────────────────────────────────────

/// Consecutive VAD speech frames required before triggering barge-in during TTS.
/// 20 frames × 256 samples / 16 kHz ≈ 320 ms — filters speaker-to-mic feedback.
const BARGE_IN_DEBOUNCE_FRAMES: usize = 20;

/// earshot requires exactly 256 samples per frame at 16 kHz.
const VAD_FRAME_SAMPLES: usize = 256;

/// VAD probability threshold — above this is considered speech.
const VAD_THRESHOLD: f32 = 0.5;

/// How long the worker waits on the audio channel before checking the shutdown flag.
const RECV_TIMEOUT: Duration = Duration::from_millis(50);

/// Cooldown after TTS stops before STT re-enables.
/// Prevents the tail of TTS audio from being transcribed as speech.
const TTS_COOLDOWN: Duration = Duration::from_millis(50);

/// Number of ONNX Runtime intra-op threads used by the offline recognizer.
const STT_NUM_THREADS: i32 = 1;

fn stt_worker(
    config: SttEngineConfig,
    audio_rx: Receiver<Vec<u8>>,
    text_tx: tokio_mpsc::Sender<String>,
    shutdown: Arc<AtomicBool>,
) {
    // ── 1. Initialise rubato resampler (48 kHz → 16 kHz, mono) ───────────────
    use rubato::{Fft, FixedSync, Resampler};

    let mut resampler = match Fft::<f32>::new(48_000, 16_000, 1024, 2, 1, FixedSync::Input) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("buzz-desktop: STT resampler init failed: {e}");
            return;
        }
    };
    let chunk_in = resampler.input_frames_next();

    // ── 2. Initialise earshot VAD ─────────────────────────────────────────────
    use earshot::{DefaultPredictor, Detector};
    let mut vad = Detector::new(DefaultPredictor::new());

    // ── 3. Initialise sherpa-onnx recognizer ─────────────────────────────────
    use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};

    let tokens_path = config.model_dir.join("tokens.txt");
    let model_path = config.model_dir.join("model.int8.onnx");
    if !tokens_path.exists() || !model_path.exists() {
        eprintln!(
            "buzz-desktop: STT model not found at {} — STT disabled",
            config.model_dir.display()
        );
        drain_until_shutdown(audio_rx, &shutdown);
        return;
    }

    let mut cfg = OfflineRecognizerConfig::default();
    cfg.model_config.nemo_ctc.model = Some(model_path.to_string_lossy().into_owned());
    cfg.model_config.tokens = Some(tokens_path.to_string_lossy().into_owned());
    cfg.model_config.num_threads = STT_NUM_THREADS;
    cfg.model_config.debug = false;

    let recognizer = match OfflineRecognizer::create(&cfg) {
        Some(r) => r,
        None => {
            eprintln!("buzz-desktop: OfflineRecognizer::create returned None — STT disabled");
            drain_until_shutdown(audio_rx, &shutdown);
            return;
        }
    };

    // ── 4. Processing state ───────────────────────────────────────────────────
    let mut input_buf_48k: Vec<f32> = Vec::with_capacity(chunk_in * 2);
    let mut leftover_16k: Vec<f32> = Vec::new();
    let mut speech_buf: Vec<f32> = Vec::new();
    let mut silence_frames: usize = 0;
    let mut in_speech = false;
    let mut barge_in_frames: usize = 0;
    let mut tts_stopped_at: Option<std::time::Instant> = None;

    // ── 5. Main loop ──────────────────────────────────────────────────────────
    let has_tts = config.tts_active.is_some();
    let tts_active_flag = config
        .tts_active
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let tts_cancel_flag = config.tts_cancel;
    let ptt_active_flag = config.ptt_active;

    let mut tts_was_active = false;
    let mut ptt_was_active = ptt_active_flag
        .as_ref()
        .is_some_and(|p| p.load(Ordering::Acquire));

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }

        // Track TTS transitions (only relevant when TTS flags are wired).
        if has_tts {
            let tts_now = tts_active_flag.load(Ordering::Acquire);
            if tts_was_active && !tts_now {
                tts_stopped_at = Some(std::time::Instant::now());
            }
            tts_was_active = tts_now;
        }

        // Track PTT transitions — flush on key release.
        if let Some(ref ptt) = ptt_active_flag {
            let ptt_now = ptt.load(Ordering::Acquire);
            if ptt_was_active && !ptt_now && in_speech && !speech_buf.is_empty() {
                flush_to_stt(&speech_buf, &recognizer, &text_tx);
                speech_buf.clear();
                silence_frames = 0;
                in_speech = false;
            }
            ptt_was_active = ptt_now;
        }

        let bytes = match audio_rx.recv_timeout(RECV_TIMEOUT) {
            Ok(b) => b,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Drain any additional pending messages to batch-process.
        let mut batch = vec![bytes];
        while let Ok(b) = audio_rx.try_recv() {
            batch.push(b);
        }

        for bytes in batch {
            let samples_48k = bytes_to_f32(&bytes);
            input_buf_48k.extend_from_slice(&samples_48k);

            while input_buf_48k.len() >= chunk_in {
                let chunk: Vec<f32> = input_buf_48k.drain(..chunk_in).collect();
                let resampled = resample_chunk(&mut resampler, &chunk);
                process_16k_samples(
                    &resampled,
                    &mut leftover_16k,
                    &mut vad,
                    &mut speech_buf,
                    &mut silence_frames,
                    &mut in_speech,
                    &mut barge_in_frames,
                    &recognizer,
                    &text_tx,
                    has_tts,
                    &tts_active_flag,
                    tts_cancel_flag.as_deref(),
                    &mut tts_stopped_at,
                    ptt_active_flag.as_ref(),
                    config.silence_flush_frames,
                    config.max_speech_samples,
                    config.partial_flush_samples,
                );
            }
        }
    }

    // ── Final flush — transcribe any remaining speech on shutdown/disconnect ──
    if !speech_buf.is_empty() {
        flush_to_stt(&speech_buf, &recognizer, &text_tx);
    }
}

/// Resample a mono 48 kHz chunk to 16 kHz using rubato.
fn resample_chunk(resampler: &mut rubato::Fft<f32>, chunk_48k: &[f32]) -> Vec<f32> {
    use audioadapter_buffers::direct::InterleavedSlice;
    use rubato::Resampler;

    let input = match InterleavedSlice::new(chunk_48k, 1, chunk_48k.len()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("buzz-desktop: STT resample input error: {e}");
            return Vec::new();
        }
    };

    match resampler.process(&input, 0, None) {
        Ok(out) => out.take_data(),
        Err(e) => {
            eprintln!("buzz-desktop: STT resample error: {e}");
            Vec::new()
        }
    }
}

/// Feed 16 kHz samples through the VAD and accumulate speech.
/// Flushes to STT when silence exceeds the configured threshold.
#[allow(clippy::too_many_arguments)]
fn process_16k_samples(
    samples: &[f32],
    leftover: &mut Vec<f32>,
    vad: &mut earshot::Detector<earshot::DefaultPredictor>,
    speech_buf: &mut Vec<f32>,
    silence_frames: &mut usize,
    in_speech: &mut bool,
    barge_in_frames: &mut usize,
    recognizer: &sherpa_onnx::OfflineRecognizer,
    text_tx: &tokio_mpsc::Sender<String>,
    has_tts: bool,
    tts_active: &Arc<AtomicBool>,
    tts_cancel: Option<&AtomicBool>,
    tts_stopped_at: &mut Option<std::time::Instant>,
    ptt_active: Option<&Arc<AtomicBool>>,
    silence_flush_threshold: usize,
    max_speech_samples: usize,
    partial_flush_samples: Option<usize>,
) {
    leftover.extend_from_slice(samples);

    while leftover.len() >= VAD_FRAME_SAMPLES {
        let frame: Vec<f32> = leftover.drain(..VAD_FRAME_SAMPLES).collect();
        let clamped: Vec<f32> = frame.iter().map(|&s| s.clamp(-1.0, 1.0)).collect();
        let prob = vad.predict_f32(&clamped);
        let is_speech = prob > VAD_THRESHOLD;

        // PTT gating: when PTT key is not held, treat as silence.
        let is_speech = if let Some(ptt) = ptt_active {
            is_speech && ptt.load(Ordering::Acquire)
        } else {
            is_speech
        };

        // TTS echo prevention (only when TTS flags are wired).
        if has_tts {
            let tts_playing = tts_active.load(Ordering::Acquire);

            if tts_playing {
                if ptt_active.is_some() {
                    // PTT mode — skip accumulation.
                    *in_speech = false;
                    *barge_in_frames = 0;
                    speech_buf.clear();
                    *silence_frames = 0;
                    continue;
                }

                // VAD mode — barge-in detection.
                if is_speech {
                    *barge_in_frames += 1;
                    if *barge_in_frames >= BARGE_IN_DEBOUNCE_FRAMES {
                        if let Some(cancel) = tts_cancel {
                            cancel.store(true, Ordering::Release);
                        }
                        *barge_in_frames = 0;
                    }
                } else {
                    *barge_in_frames = 0;
                }
                *in_speech = false;
                speech_buf.clear();
                *silence_frames = 0;
                continue;
            }

            // TTS cooldown window.
            if let Some(stopped) = *tts_stopped_at {
                if stopped.elapsed() < TTS_COOLDOWN {
                    if !is_speech {
                        *in_speech = false;
                    }
                    speech_buf.clear();
                    *silence_frames = 0;
                    *barge_in_frames = 0;
                    continue;
                } else {
                    *tts_stopped_at = None;
                    *in_speech = false;
                    *silence_frames = 0;
                    *barge_in_frames = 0;
                }
            }
        }

        if is_speech {
            *silence_frames = 0;
            *in_speech = true;
            speech_buf.extend_from_slice(&frame);

            // OOM guard.
            if speech_buf.len() >= max_speech_samples {
                flush_to_stt(speech_buf, recognizer, text_tx);
                speech_buf.clear();
                *silence_frames = 0;
                *in_speech = false;
            }
            // Periodic partial flush — emit intermediate transcript so text
            // appears on-the-fly during continuous speech (dictation mode).
            else if let Some(threshold) = partial_flush_samples {
                if speech_buf.len() >= threshold {
                    flush_to_stt(speech_buf, recognizer, text_tx);
                    speech_buf.clear();
                    // Stay in_speech — the user is still talking.
                }
            }
        } else if *in_speech {
            speech_buf.extend_from_slice(&frame);
            *silence_frames += 1;

            // In PTT mode, don't flush on silence — the PTT release edge handles it.
            if ptt_active.is_none() && *silence_frames >= silence_flush_threshold {
                flush_to_stt(speech_buf, recognizer, text_tx);
                speech_buf.clear();
                *silence_frames = 0;
                *in_speech = false;
            }
        }
    }
}

/// Run sherpa-onnx on the accumulated speech buffer and send the text.
fn flush_to_stt(
    speech_buf: &[f32],
    recognizer: &sherpa_onnx::OfflineRecognizer,
    text_tx: &tokio_mpsc::Sender<String>,
) {
    if speech_buf.is_empty() {
        return;
    }

    let stream = recognizer.create_stream();
    stream.accept_waveform(16_000, speech_buf);
    recognizer.decode(&stream);

    let text = stream
        .get_result()
        .map(|r| r.text.trim().to_string())
        .unwrap_or_default();

    if !text.is_empty() {
        if let Err(e) = text_tx.blocking_send(text) {
            eprintln!("buzz-desktop: STT text channel closed: {e}");
        }
    }
}

/// Convert raw bytes (f32 LE) to f32 samples.
fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}
