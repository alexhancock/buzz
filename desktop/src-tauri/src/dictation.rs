//! Local dictation pipeline — uses the Parakeet STT engine for offline
//! speech-to-text in the message composer.
//!
//! Unlike the huddle STT pipeline (which posts kind:9 events to the relay),
//! dictation emits transcribed text back to the frontend via Tauri events
//! so the composer can display it in real-time.
//!
//! Key differences from huddle STT:
//! - No TTS barge-in / echo gating (no agent voice in composer context)
//! - No PTT (dictation uses a toggle button, not push-to-talk)
//! - Slightly longer silence threshold for more coherent sentences
//! - Text goes to the frontend, not to the relay

use std::sync::Arc;

use tauri::{Emitter, State};

use crate::app_state::AppState;
use crate::huddle::models;
use crate::stt_engine::{
    SttEngine, SttEngineConfig, DEFAULT_MAX_SPEECH_SAMPLES, DICTATION_PARTIAL_FLUSH_SAMPLES,
    DICTATION_SILENCE_FLUSH_FRAMES,
};

/// Tauri event name emitted when a dictation transcript segment is ready.
const DICTATION_TRANSCRIPT_EVENT: &str = "dictation-transcript";

/// Tauri event name emitted when dictation state changes (started/stopped).
const DICTATION_STATE_EVENT: &str = "dictation-state";

/// State for the active dictation session.
///
/// Stored in `AppState` behind a `Mutex`. Only one dictation session can be
/// active at a time (starting a new one stops the previous).
pub(crate) struct DictationState {
    /// The running STT engine, if dictation is active.
    engine: Option<Arc<SttEngine>>,
}

impl DictationState {
    pub fn new() -> Self {
        Self { engine: None }
    }
}

/// `start_dictation` — begin local STT dictation.
///
/// Starts the Parakeet STT engine and spawns a task that emits
/// `dictation-transcript` events to the frontend as text is recognized.
/// Returns an error if models are not downloaded yet.
#[tauri::command]
pub async fn start_dictation(state: State<'_, AppState>) -> Result<(), String> {
    // Check if models are ready.
    if !models::is_stt_ready() {
        // Kick off download if not already in progress.
        if let Some(mgr) = models::global_model_manager() {
            mgr.start_stt_download(state.http_client.clone());
        }
        return Err("STT model not ready — download in progress".to_string());
    }

    let model_dir = models::stt_model_dir().ok_or("STT model directory not found")?;

    // Stop any existing dictation session first.
    stop_dictation_inner(&state);

    let config = SttEngineConfig {
        model_dir,
        silence_flush_frames: DICTATION_SILENCE_FLUSH_FRAMES,
        max_speech_samples: DEFAULT_MAX_SPEECH_SAMPLES,
        partial_flush_samples: Some(DICTATION_PARTIAL_FLUSH_SAMPLES),
        tts_active: None,
        tts_cancel: None,
        ptt_active: None,
    };

    let (engine, text_rx) = SttEngine::new(config)?;
    let engine = Arc::new(engine);

    // Store the engine in state.
    {
        let mut ds = state
            .dictation_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ds.engine = Some(Arc::clone(&engine));
    }

    // Spawn a task that forwards transcribed text to the frontend.
    let app_handle = state
        .app_handle
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();

    if let Some(handle) = app_handle {
        let _ = handle.emit(DICTATION_STATE_EVENT, "started");
        spawn_dictation_forwarder(text_rx, handle);
    }

    Ok(())
}

/// `stop_dictation` — stop the active dictation session.
///
/// The final transcript (if any) is emitted asynchronously by the forwarder
/// task. The `dictation-state: stopped` event is emitted by the forwarder
/// after all pending transcripts have been forwarded, ensuring the frontend
/// receives the final text before the stopped signal.
#[tauri::command]
pub fn stop_dictation(state: State<'_, AppState>) -> Result<(), String> {
    stop_dictation_inner(&state);
    // Note: `stopped` is emitted by the forwarder task after draining all
    // pending transcripts — not here. This avoids a race where the frontend
    // sees `stopped` before the final transcript arrives.
    Ok(())
}

/// `push_dictation_audio` — feed raw PCM bytes into the dictation pipeline.
///
/// Expects a raw binary body of f32 LE samples at 48 kHz mono.
/// If no dictation session is active, the bytes are silently discarded.
#[tauri::command]
pub fn push_dictation_audio(
    request: tauri::ipc::Request<'_>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    /// Maximum IPC audio batch size: 100 KB.
    const MAX_AUDIO_BATCH_BYTES: usize = 100 * 1024;

    match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => {
            if bytes.len() > MAX_AUDIO_BATCH_BYTES {
                return Err(format!(
                    "audio batch too large: {} bytes (max {})",
                    bytes.len(),
                    MAX_AUDIO_BATCH_BYTES
                ));
            }
            let ds = state
                .dictation_state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref engine) = ds.engine {
                engine.push_audio(bytes.to_vec())?;
            }
            Ok(())
        }
        _ => Err("expected raw binary body".to_string()),
    }
}

/// `get_dictation_status` — check if local dictation is available and/or active.
#[tauri::command]
pub fn get_dictation_status(state: State<'_, AppState>) -> DictationStatus {
    let model_ready = models::is_stt_ready();
    let is_active = state
        .dictation_state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .engine
        .is_some();

    DictationStatus {
        available: model_ready,
        active: is_active,
    }
}

/// Response for `get_dictation_status`.
#[derive(serde::Serialize, Clone)]
pub struct DictationStatus {
    /// Whether the local STT model is downloaded and ready.
    pub available: bool,
    /// Whether a dictation session is currently active.
    pub active: bool,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn stop_dictation_inner(state: &AppState) {
    let old_engine = {
        let mut ds = state
            .dictation_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ds.engine.take()
    };
    if let Some(engine) = old_engine {
        engine.shutdown();
        // Drop outside the lock — thread join may block briefly.
        drop(engine);
    }
}

/// Spawn an async task that reads transcribed text and emits Tauri events.
///
/// When the channel closes (engine stopped), the forwarder emits
/// `dictation-state: stopped` so the frontend knows all pending transcripts
/// have been delivered.
fn spawn_dictation_forwarder(
    mut text_rx: tokio::sync::mpsc::Receiver<String>,
    app_handle: tauri::AppHandle,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(text) = text_rx.recv().await {
            if text.is_empty() {
                continue;
            }
            if app_handle.emit(DICTATION_TRANSCRIPT_EVENT, &text).is_err() {
                break; // App window closed.
            }
        }
        // All transcripts forwarded — signal the frontend that dictation is done.
        let _ = app_handle.emit(DICTATION_STATE_EVENT, "stopped");
    });
}
