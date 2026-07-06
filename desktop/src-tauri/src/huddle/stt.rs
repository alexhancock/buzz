//! Speech-to-Text pipeline for huddle voice transcription.
//!
//! This is a thin wrapper around `crate::stt_engine::SttEngine` configured
//! with huddle-specific settings (TTS barge-in, PTT gating, huddle silence
//! threshold).
//!
//! ```text
//! AudioWorklet (48 kHz f32 PCM)
//!   → push_audio_pcm (Tauri cmd)
//!   → SttPipeline::push_audio
//!   → SttEngine worker thread
//!       rubato: 48 kHz → 16 kHz mono
//!       earshot VAD: accumulate speech frames
//!       sherpa-onnx Parakeet TDT-CTC 110M: transcribe on silence
//!   → text_rx  [mpsc channel]
//!   → tokio task (start_stt_pipeline)
//!       builds kind:9 event → relay
//! ```

use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};

use tokio::sync::mpsc as tokio_mpsc;

use crate::stt_engine::{
    SttEngine, SttEngineConfig, DEFAULT_MAX_SPEECH_SAMPLES, DEFAULT_SILENCE_FLUSH_FRAMES,
};

// ── Public pipeline handle ────────────────────────────────────────────────────

/// Handle to the running huddle STT pipeline.
///
/// Wraps `SttEngine` with huddle-specific construction (TTS flags, PTT).
/// Not Clone — wrap in `Arc` to share across threads.
#[derive(Debug)]
pub struct SttPipeline {
    engine: SttEngine,
}

impl SttPipeline {
    /// Spawn the huddle STT pipeline.
    ///
    /// `tts_active` is a shared flag set by the TTS pipeline while audio is
    /// playing. The STT worker uses it to:
    ///   - discard accumulated speech (echo prevention / barge-in gating)
    ///   - apply a cooldown after TTS stops before re-enabling STT
    ///   - detect barge-in: speech onset during TTS → set `tts_cancel`
    ///
    /// `tts_cancel` (optional) is the TTS pipeline's cancel flag. When the STT
    /// worker detects speech onset while TTS is active, it sets this flag to
    /// stop playback immediately (barge-in). Pass `None` if TTS is unavailable.
    ///
    /// `ptt_active` (optional) is the push-to-talk flag. When `Some`, the STT
    /// pipeline only accumulates speech while the flag is true (key held).
    /// When `None`, the pipeline runs in continuous VAD mode.
    ///
    /// Returns `Err` only if the thread cannot be spawned (OS error).
    pub fn new(
        model_dir: PathBuf,
        tts_active: Arc<AtomicBool>,
        tts_cancel: Option<Arc<AtomicBool>>,
        ptt_active: Option<Arc<AtomicBool>>,
    ) -> Result<(Self, tokio_mpsc::Receiver<String>), String> {
        let config = SttEngineConfig {
            model_dir,
            silence_flush_frames: DEFAULT_SILENCE_FLUSH_FRAMES,
            max_speech_samples: DEFAULT_MAX_SPEECH_SAMPLES,
            partial_flush_samples: None,
            tts_active: Some(tts_active),
            tts_cancel,
            ptt_active,
        };

        let (engine, text_rx) = SttEngine::new(config)?;
        Ok((Self { engine }, text_rx))
    }

    /// Signal the worker thread to stop.
    pub fn shutdown(&self) {
        self.engine.shutdown();
    }

    /// Returns `true` if the worker thread has exited.
    pub fn is_finished(&self) -> bool {
        self.engine.is_finished()
    }

    /// Feed raw PCM bytes (f32 LE, 48 kHz mono) into the pipeline.
    ///
    /// Non-blocking. Drops audio silently if the pipeline can't keep up.
    pub fn push_audio(&self, pcm_bytes: Vec<u8>) -> Result<(), String> {
        self.engine.push_audio(pcm_bytes)
    }
}
