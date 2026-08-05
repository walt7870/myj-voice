use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::config::AppConfig;
use crate::xfyun::auth::{build_signed_ws_url, current_rfc1123_date};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChatChunk {
    pub content: String,
    pub reasoning_content: Option<String>,
    pub is_final: bool,
}

pub fn build_chat_payload(
    app_id: &str,
    domain: &str,
    temperature: f32,
    max_tokens: u32,
    messages: Vec<ChatMessage>,
) -> Value {
    json!({
        "header": {
            "app_id": app_id,
            "uid": "mjy-voice-shop"
        },
        "parameter": {
            "chat": {
                "domain": domain,
                "temperature": temperature,
                "max_tokens": max_tokens
            }
        },
        "payload": {
            "message": {
                "text": messages
            }
        }
    })
}

pub fn parse_chat_chunk(message: &Value) -> Result<ChatChunk> {
    let code = message
        .pointer("/header/code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if code != 0 {
        let detail = message
            .pointer("/header/message")
            .and_then(Value::as_str)
            .unwrap_or("unknown upstream error");
        anyhow::bail!("llm error code: {code}, message: {detail}");
    }
    let choices = &message["payload"]["choices"];
    let first = &choices["text"][0];
    Ok(ChatChunk {
        content: first["content"].as_str().unwrap_or("").to_string(),
        reasoning_content: first["reasoning_content"].as_str().map(ToString::to_string),
        is_final: choices["status"].as_i64().unwrap_or(0) == 2,
    })
}

pub async fn stream_chat_chunks(
    config: AppConfig,
    messages: Vec<ChatMessage>,
    tx: mpsc::Sender<Result<ChatChunk>>,
) {
    let result = stream_chat_chunks_inner(config, messages, tx.clone()).await;
    if let Err(error) = result {
        let _ = tx.send(Err(error)).await;
    }
}

async fn stream_chat_chunks_inner(
    config: AppConfig,
    messages: Vec<ChatMessage>,
    tx: mpsc::Sender<Result<ChatChunk>>,
) -> Result<()> {
    anyhow::ensure!(
        !config.api_key.trim().is_empty(),
        "XF_API_KEY is required for LLM"
    );
    anyhow::ensure!(
        !config.api_secret.trim().is_empty(),
        "XF_API_SECRET is required for LLM"
    );
    let signed_url = build_signed_ws_url(
        &config.llm_endpoint,
        &config.api_key,
        &config.api_secret,
        &current_rfc1123_date(),
    )?;
    let (mut socket, _) = connect_async(signed_url).await?;
    let payload = build_chat_payload(
        &config.app_id,
        &config.llm_model,
        config.temperature,
        config.max_tokens,
        messages,
    );
    socket
        .send(Message::Text(payload.to_string().into()))
        .await?;
    while let Some(message) = socket.next().await {
        let message = message?;
        let Message::Text(raw) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&raw)?;
        let chunk = parse_chat_chunk(&value)?;
        let is_final = chunk.is_final;
        if tx.send(Ok(chunk)).await.is_err() {
            break;
        }
        if is_final {
            break;
        }
    }
    Ok(())
}

pub fn split_complete_sentences(buffer: &mut String, delta: &str) -> Vec<String> {
    buffer.push_str(delta);
    let mut ready = Vec::new();
    let mut last_boundary = 0;
    for (idx, ch) in buffer.char_indices() {
        let chars_before = buffer[..idx].chars().count();
        let is_strong = matches!(ch, '。' | '！' | '？' | '!' | '?' | '\n');
        let is_soft = matches!(ch, '，' | ',' | '；' | ';') && chars_before >= 8;
        if is_strong || is_soft {
            last_boundary = idx + ch.len_utf8();
        }
    }
    if last_boundary == 0 && buffer.chars().count() >= 22 {
        last_boundary = buffer
            .char_indices()
            .nth(21)
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(buffer.len());
    }
    if last_boundary > 0 {
        let drained = buffer.drain(..last_boundary).collect::<String>();
        let mut segment = String::new();
        for ch in drained.chars() {
            segment.push(ch);
            let segment_len = segment.chars().count();
            let is_strong = matches!(ch, '。' | '！' | '？' | '!' | '?' | '\n');
            let is_soft = matches!(ch, '，' | ',' | '；' | ';') && segment_len >= 9;
            if is_strong || is_soft {
                let text = segment.trim();
                if text.chars().count() >= 3 {
                    ready.push(text.to_string());
                }
                segment.clear();
            }
        }
        let text = segment.trim();
        if text.chars().count() >= 3 {
            ready.push(text.to_string());
        }
    }
    ready
}
