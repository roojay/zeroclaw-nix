//! OpenAI-compatible `/v1/chat/completions` endpoint backed by the full agent loop.
//!
//! Accepts OpenAI Chat Completions format from Home Assistant's
//! `extended_openai_conversation` integration, routes through
//! `crate::agent::process_message()` for full tool/memory/skill support,
//! and returns responses in OpenAI format (JSON or SSE burst).

use super::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
};
use futures_util::stream::Stream;
use serde::Deserialize;
use std::sync::OnceLock;

/// Context prefix for messages arriving via the OpenAI-compatible API.
/// Tells the LLM which channel it's responding on so it can adapt tone/length.
const OPENAI_PROXY_STYLE_PREFIX: &str = "\
[context: you are responding via the OpenAI-compatible API used by Home Assistant voice. \
Keep responses short and conversational — this is a voice assistant chain. \
Do NOT use markdown formatting.]\n";

// ── Identity cache ──

static IDENTITY_CACHE: OnceLock<Option<String>> = OnceLock::new();

fn get_identity() -> Option<&'static str> {
    IDENTITY_CACHE
        .get_or_init(|| {
            let workspace = std::env::var("ZEROCLAW_WORKSPACE")
                .unwrap_or_else(|_| "/var/lib/sid/.zeroclaw/workspace".to_string());
            let path = std::path::Path::new(&workspace).join("IDENTITY.md");
            match std::fs::read_to_string(&path) {
                Ok(content) if !content.trim().is_empty() => {
                    tracing::info!("Loaded identity from {}", path.display());
                    Some(content)
                }
                Ok(_) => None,
                Err(e) => {
                    tracing::warn!("Could not read {}: {e}", path.display());
                    None
                }
            }
        })
        .as_deref()
}

// ── OpenAI request types ──

#[derive(Debug, Deserialize)]
pub struct ChatCompletionsRequest {
    pub messages: Vec<OaiMessage>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub stream: Option<bool>,
    // Tools accepted but ignored — agent has its own tool registry
    #[serde(default)]
    pub tools: Option<serde_json::Value>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct OaiMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<serde_json::Value>,
    // tool_calls/tool_call_id accepted for deserialization but ignored
    #[serde(default)]
    pub tool_calls: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

// ── Message extraction ──

/// Extract a single message string from the OpenAI messages array.
///
/// Collects system messages and the last user message, prepends identity
/// context, and returns a single string for the agent loop.
fn extract_message(messages: &[OaiMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Prepend identity
    if let Some(identity) = get_identity() {
        parts.push(identity.to_string());
    }

    // Collect system messages
    for msg in messages {
        if msg.role == "system" {
            if let Some(content) = &msg.content {
                let text = content_to_string(content);
                if !text.is_empty() {
                    parts.push(text);
                }
            }
        }
    }

    // Find last user message
    if let Some(user_msg) = messages.iter().rev().find(|m| m.role == "user") {
        if let Some(content) = &user_msg.content {
            let text = content_to_string(content);
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }

    parts.join("\n\n")
}

fn content_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}

// ── Handler ──

pub async fn handle_chat_completions(
    State(state): State<AppState>,
    body: Result<Json<ChatCompletionsRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("/v1/chat/completions JSON parse error: {e}");
            return error_response(
                StatusCode::BAD_REQUEST,
                &format!("Invalid JSON: {e}"),
                "invalid_request_error",
            )
            .into_response();
        }
    };

    let message = extract_message(&req.messages);
    if message.is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "No user message found",
            "invalid_request_error",
        )
        .into_response();
    }

    let is_stream = req.stream.unwrap_or(false);

    // Run the full agent loop with channel context
    let config = state.config.lock().clone();
    let contextualized = format!("{OPENAI_PROXY_STYLE_PREFIX}{message}");
    let response = match crate::agent::process_message(config, &contextualized, None).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("Agent loop error: {e}");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Agent error: {e}"),
                "agent_error",
            )
            .into_response();
        }
    };

    if is_stream {
        handle_streaming(&response).into_response()
    } else {
        handle_non_streaming(&response).into_response()
    }
}

// ── Non-streaming ──

fn handle_non_streaming(response: &str) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "model": "sid",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": response,
            },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0,
        }
    }))
}

// ── Streaming (single-burst SSE) ──

fn handle_streaming(
    response: &str,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let chat_id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let response_text = response.to_string();

    let role_chunk = oai_chunk(&chat_id, serde_json::json!({"role": "assistant"}), None);
    let content_chunk = oai_chunk(&chat_id, serde_json::json!({"content": response_text}), None);
    let finish_chunk = oai_chunk(&chat_id, serde_json::json!({}), Some("stop"));

    let stream = futures_util::stream::iter(vec![
        Ok(Event::default().data(serde_json::to_string(&role_chunk).unwrap_or_default())),
        Ok(Event::default().data(serde_json::to_string(&content_chunk).unwrap_or_default())),
        Ok(Event::default().data(serde_json::to_string(&finish_chunk).unwrap_or_default())),
        Ok(Event::default().data("[DONE]".to_string())),
    ]);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn oai_chunk(
    id: &str,
    delta: serde_json::Value,
    finish_reason: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "model": "sid",
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }]
    })
}

fn error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "message": message,
                "type": error_type,
            }
        })),
    )
}
