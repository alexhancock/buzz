//! Transcription session endpoint — proxies OpenAI Realtime API client-secret minting.
//!
//! When `BUZZ_OPENAI_API_KEY` is configured, the relay can mint ephemeral client
//! secrets for the OpenAI Realtime API. The desktop app uses these to establish a
//! WebRTC connection for real-time speech-to-text dictation.

use axum::{extract::State, http::StatusCode, response::Json};
use serde::Serialize;
use std::sync::Arc;

use crate::state::AppState;

const OPENAI_REALTIME_SESSIONS_URL: &str = "https://api.openai.com/v1/realtime/sessions";
const DEFAULT_TRANSCRIPTION_MODEL: &str = "whisper-1";

#[derive(Serialize)]
pub struct TranscribeStatus {
    configured: bool,
    model: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeSession {
    client_secret: String,
    model: String,
}

/// `GET /transcribe/status` — check if transcription is configured.
pub async fn transcribe_status(State(state): State<Arc<AppState>>) -> Json<TranscribeStatus> {
    Json(TranscribeStatus {
        configured: state.config.openai_api_key.is_some(),
        model: transcription_model(),
    })
}

/// `POST /transcribe/session` — create an ephemeral OpenAI Realtime session.
///
/// Returns a short-lived client secret that the frontend uses to establish
/// a WebRTC connection directly with OpenAI for real-time transcription.
pub async fn create_transcribe_session(
    State(state): State<Arc<AppState>>,
) -> Result<Json<TranscribeSession>, (StatusCode, Json<serde_json::Value>)> {
    let api_key = state.config.openai_api_key.as_deref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "transcription_not_configured",
                "message": "Transcription is not configured on this relay"
            })),
        )
    })?;

    let model = transcription_model();

    let client = reqwest::Client::new();
    let response = client
        .post(OPENAI_REALTIME_SESSIONS_URL)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "model": "gpt-4o-mini-realtime-preview",
            "modalities": ["text"],
            "input_audio_transcription": {
                "model": model,
            },
            "turn_detection": {
                "type": "server_vad",
            }
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| {
            tracing::error!("OpenAI realtime session request failed: {e}");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "upstream_error",
                    "message": "Failed to create transcription session"
                })),
            )
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::error!("OpenAI realtime session error ({status}): {body}");
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "upstream_error",
                "message": "OpenAI rejected the transcription session request"
            })),
        ));
    }

    let body: serde_json::Value = response.json().await.map_err(|e| {
        tracing::error!("OpenAI realtime session response parse error: {e}");
        (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "upstream_error",
                "message": "Invalid response from transcription service"
            })),
        )
    })?;

    let client_secret = extract_client_secret(&body).ok_or_else(|| {
        tracing::error!("OpenAI realtime session response missing client_secret: {body}");
        (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": "upstream_error",
                "message": "Transcription service returned unexpected response"
            })),
        )
    })?;

    Ok(Json(TranscribeSession {
        client_secret,
        model,
    }))
}

fn transcription_model() -> String {
    std::env::var("BUZZ_TRANSCRIPTION_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_TRANSCRIPTION_MODEL.to_string())
}

fn extract_client_secret(value: &serde_json::Value) -> Option<String> {
    // Shape 1: { "client_secret": { "value": "..." } }
    if let Some(cs) = value.get("client_secret") {
        if let Some(v) = cs.get("value").and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
        // Shape 2: { "client_secret": "..." }
        if let Some(v) = cs.as_str() {
            return Some(v.to_string());
        }
    }
    // Shape 3: { "value": "..." }
    value
        .get("value")
        .and_then(|v| v.as_str())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::extract_client_secret;
    use serde_json::json;

    #[test]
    fn parses_nested_client_secret() {
        let body = json!({ "client_secret": { "value": "sec_abc123", "expires_at": 9999 } });
        assert_eq!(extract_client_secret(&body), Some("sec_abc123".to_string()));
    }

    #[test]
    fn parses_direct_string_client_secret() {
        let body = json!({ "client_secret": "sec_direct" });
        assert_eq!(extract_client_secret(&body), Some("sec_direct".to_string()));
    }

    #[test]
    fn parses_top_level_value() {
        let body = json!({ "value": "sec_toplevel" });
        assert_eq!(
            extract_client_secret(&body),
            Some("sec_toplevel".to_string())
        );
    }

    #[test]
    fn returns_none_for_missing_secret() {
        let body = json!({ "id": "sess_123", "model": "gpt-4o" });
        assert_eq!(extract_client_secret(&body), None);
    }
}
