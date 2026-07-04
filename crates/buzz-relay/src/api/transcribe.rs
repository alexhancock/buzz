//! Transcription session endpoint — proxies OpenAI Realtime API client-secret minting.
//!
//! When `BUZZ_OPENAI_API_KEY` is configured, the relay can mint ephemeral client
//! secrets for the OpenAI Realtime API. The desktop app uses these to establish a
//! WebRTC connection for real-time speech-to-text dictation.
//!
//! Both endpoints require NIP-98 auth (same as `/events`, `/query`, `/count`).

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
};
use serde::Serialize;
use serde_json::Value;

use crate::state::AppState;

use super::api_error;

const OPENAI_REALTIME_SESSIONS_URL: &str = "https://api.openai.com/v1/realtime/sessions";
const DEFAULT_TRANSCRIPTION_MODEL: &str = "whisper-1";

/// Response for `GET /transcribe/status`.
#[derive(Serialize)]
pub struct TranscribeStatus {
    configured: bool,
    model: String,
}

/// Response for `POST /transcribe/session`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscribeSession {
    client_secret: String,
    model: String,
}

/// `GET /transcribe/status` — check if transcription is configured.
///
/// Requires NIP-98 auth. Returns whether the relay has an OpenAI API key
/// configured for real-time transcription.
pub async fn transcribe_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<TranscribeStatus>, (StatusCode, Json<Value>)> {
    authenticate(&state, &headers, "/transcribe/status", "GET").await?;

    Ok(Json(TranscribeStatus {
        configured: state.config.openai_api_key.is_some(),
        model: transcription_model(),
    }))
}

/// `POST /transcribe/session` — create an ephemeral OpenAI Realtime session.
///
/// Requires NIP-98 auth. Returns a short-lived client secret that the frontend
/// uses to establish a WebRTC connection directly with OpenAI for real-time
/// transcription.
pub async fn create_transcribe_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<TranscribeSession>, (StatusCode, Json<Value>)> {
    authenticate(&state, &headers, "/transcribe/session", "POST").await?;

    let api_key = state.config.openai_api_key.as_deref().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "transcription is not configured on this relay",
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
            api_error(
                StatusCode::BAD_GATEWAY,
                "failed to create transcription session",
            )
        })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::error!("OpenAI realtime session error ({status}): {body}");
        return Err(api_error(
            StatusCode::BAD_GATEWAY,
            "OpenAI rejected the transcription session request",
        ));
    }

    let body: Value = response.json().await.map_err(|e| {
        tracing::error!("OpenAI realtime session response parse error: {e}");
        api_error(
            StatusCode::BAD_GATEWAY,
            "invalid response from transcription service",
        )
    })?;

    let client_secret = extract_client_secret(&body).ok_or_else(|| {
        tracing::error!("OpenAI realtime session response missing client_secret: {body}");
        api_error(
            StatusCode::BAD_GATEWAY,
            "transcription service returned unexpected response",
        )
    })?;

    Ok(Json(TranscribeSession {
        client_secret,
        model,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Authenticate the request using the same NIP-98 / X-Pubkey pattern as the
/// bridge endpoints, plus replay detection and relay membership enforcement.
async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    method: &str,
) -> Result<(), (StatusCode, Json<Value>)> {
    let raw_host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let tenant = crate::tenant::bind_community(&state.db, raw_host)
        .await
        .map_err(|_| {
            api_error(
                StatusCode::NOT_FOUND,
                "relay: no community is configured for this host",
            )
        })?;

    let url = super::bridge::nip98_expected_url(&state.config.relay_url, &tenant, path);
    let (pubkey, event_id_bytes) = super::bridge::verify_bridge_auth(
        headers,
        method,
        &url,
        None,
        state.config.require_auth_token,
    )?;
    super::bridge::check_nip98_replay(state, &tenant, event_id_bytes).await?;

    // Enforce relay membership (with NIP-OA fallback via x-auth-tag header).
    let pubkey_bytes = pubkey.to_bytes().to_vec();
    let auth_tag = headers.get("x-auth-tag").and_then(|v| v.to_str().ok());
    super::relay_members::enforce_relay_membership(
        state,
        tenant.community(),
        &pubkey_bytes,
        auth_tag,
    )
    .await?;

    Ok(())
}

fn transcription_model() -> String {
    std::env::var("BUZZ_TRANSCRIPTION_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_TRANSCRIPTION_MODEL.to_string())
}

fn extract_client_secret(value: &Value) -> Option<String> {
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
