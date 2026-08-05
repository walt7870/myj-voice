use std::fmt;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::xfyun::{
    audio::{iat_supports, IatProvider},
    auth::{build_signed_ws_url, current_rfc1123_date},
};
use crate::{
    config::AppConfig,
    domain::audio::{AudioFormat, AudioProfile, AudioSampleRate},
};

pub const MAX_IAT_PACKET_BYTES: usize = 64 * 1024;
pub const STANDARD_IAT_MAX_RAW_FRAME_BYTES: usize = 9_750;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IatFrameKind {
    First,
    Continue,
    Last,
}

impl IatFrameKind {
    fn status(self) -> i32 {
        match self {
            Self::First => 0,
            Self::Continue => 1,
            Self::Last => 2,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IatText {
    pub text: String,
    pub is_final: bool,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AudioPacketError {
    code: &'static str,
    message: String,
}

impl AudioPacketError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_audio_packet",
            message: message.into(),
        }
    }

    fn unsupported(profile: AudioProfile, provider: IatProvider) -> Self {
        Self {
            code: "unsupported_audio_profile",
            message: format!(
                "IAT provider {provider:?} does not support {}/{}",
                profile.format.as_str(),
                profile.sample_rate.hz()
            ),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for AudioPacketError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AudioPacketError {}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum IatUpstreamErrorKind {
    AudioProfileRejected,
    Other,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct IatUpstreamError {
    pub code: i64,
    pub message: String,
    pub provider: IatProvider,
    pub kind: IatUpstreamErrorKind,
}

impl IatUpstreamError {
    fn new(provider: IatProvider, code: i64, message: String) -> Self {
        let kind = classify_iat_upstream_error(provider, code, &message);
        Self {
            code,
            message,
            provider,
            kind,
        }
    }

    pub fn is_audio_profile_rejection(&self) -> bool {
        self.kind == IatUpstreamErrorKind::AudioProfileRejected
    }
}

fn classify_iat_upstream_error(
    provider: IatProvider,
    code: i64,
    message: &str,
) -> IatUpstreamErrorKind {
    match (provider, code) {
        (IatProvider::Standard | IatProvider::SuperSmart, 10043) => {
            IatUpstreamErrorKind::AudioProfileRejected
        }
        (IatProvider::Standard | IatProvider::SuperSmart, 10006 | 10007)
            if contains_explicit_audio_field(message) =>
        {
            IatUpstreamErrorKind::AudioProfileRejected
        }
        (IatProvider::Standard | IatProvider::SuperSmart, 10163)
            if contains_explicit_audio_field(message) || contains_explicit_codec_name(message) =>
        {
            IatUpstreamErrorKind::AudioProfileRejected
        }
        _ => IatUpstreamErrorKind::Other,
    }
}

fn contains_explicit_audio_field(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "audio rate",
        "audio format",
        "sample_rate",
        "encoding",
        "codec",
    ]
    .iter()
    .any(|marker| message.contains(marker))
        || contains_ascii_token(&message, "aue")
        || contains_ascii_token(&message, "auf")
}

fn contains_explicit_codec_name(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    contains_ascii_token(&message, "speex") || contains_ascii_token(&message, "opus")
}

fn contains_ascii_token(message: &str, token: &str) -> bool {
    message.match_indices(token).any(|(start, _)| {
        let end = start + token.len();
        let before_is_word = message[..start]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_');
        let after_is_word = message[end..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_');
        !before_is_word && !after_is_word
    })
}

impl fmt::Display for IatUpstreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "iat upstream error code {}: {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for IatUpstreamError {}

fn legacy_profile(format: AudioFormat) -> Result<AudioProfile> {
    let format = match format {
        AudioFormat::Mp3 => AudioFormat::Mp3,
        AudioFormat::Pcm16k => AudioFormat::Pcm,
        _ => {
            return Err(AudioPacketError::unsupported(
                AudioProfile::new(format, AudioSampleRate::Hz16000),
                IatProvider::SuperSmart,
            )
            .into())
        }
    };
    Ok(AudioProfile::new(format, AudioSampleRate::Hz16000))
}

pub fn iat_encoding(
    profile: AudioProfile,
    provider: IatProvider,
) -> std::result::Result<&'static str, AudioPacketError> {
    if !iat_supports(provider, profile) {
        return Err(AudioPacketError::unsupported(profile, provider));
    }

    match (provider, profile.format, profile.sample_rate) {
        (IatProvider::SuperSmart, AudioFormat::Mp3, AudioSampleRate::Hz16000) => Ok("lame"),
        (IatProvider::SuperSmart, AudioFormat::Pcm, AudioSampleRate::Hz16000) => Ok("raw"),
        (IatProvider::Standard, AudioFormat::Mp3, _) => Ok("lame"),
        (IatProvider::Standard, AudioFormat::Pcm, _) => Ok("raw"),
        (IatProvider::Standard, AudioFormat::Speex, AudioSampleRate::Hz8000) => Ok("speex"),
        (IatProvider::Standard, AudioFormat::Speex, AudioSampleRate::Hz16000) => Ok("speex-wb"),
        _ => Err(AudioPacketError::unsupported(profile, provider)),
    }
}

fn speex_packet_size(profile: AudioProfile) -> Option<usize> {
    match (profile.format, profile.sample_rate) {
        (AudioFormat::Speex, AudioSampleRate::Hz8000) => Some(38),
        (AudioFormat::Speex, AudioSampleRate::Hz16000) => Some(60),
        _ => None,
    }
}

pub fn validate_input_packet(
    profile: AudioProfile,
    audio: &[u8],
) -> std::result::Result<(), AudioPacketError> {
    validate_frame_packet(profile, audio, IatFrameKind::Continue)
}

pub fn validate_input_packet_for_provider(
    profile: AudioProfile,
    audio: &[u8],
    provider: IatProvider,
) -> std::result::Result<(), AudioPacketError> {
    validate_frame_packet_for_provider(profile, audio, IatFrameKind::Continue, provider)
}

fn validate_frame_packet_for_provider(
    profile: AudioProfile,
    audio: &[u8],
    kind: IatFrameKind,
    provider: IatProvider,
) -> std::result::Result<(), AudioPacketError> {
    validate_frame_packet(profile, audio, kind)?;
    if provider == IatProvider::Standard && audio.len() > STANDARD_IAT_MAX_RAW_FRAME_BYTES {
        return Err(AudioPacketError::invalid(format!(
            "standard IAT raw audio frame is {} bytes; maximum is {STANDARD_IAT_MAX_RAW_FRAME_BYTES}",
            audio.len()
        )));
    }
    Ok(())
}

fn validate_frame_packet(
    profile: AudioProfile,
    audio: &[u8],
    kind: IatFrameKind,
) -> std::result::Result<(), AudioPacketError> {
    if audio.len() > MAX_IAT_PACKET_BYTES {
        return Err(AudioPacketError::invalid(format!(
            "audio packet is {} bytes; maximum is {MAX_IAT_PACKET_BYTES}",
            audio.len()
        )));
    }
    if audio.is_empty() {
        return if kind == IatFrameKind::Last {
            Ok(())
        } else {
            Err(AudioPacketError::invalid(
                "empty audio is only valid for the last IAT frame",
            ))
        };
    }
    if matches!(profile.format, AudioFormat::Pcm | AudioFormat::Pcm16k) && audio.len() % 2 != 0 {
        return Err(AudioPacketError::invalid(
            "PCM audio packet must contain complete 16-bit samples",
        ));
    }
    if let Some(expected) = speex_packet_size(profile) {
        if audio.len() != expected {
            return Err(AudioPacketError::invalid(format!(
                "Speex packet is {} bytes; expected {expected} bytes for {} Hz quality 7",
                audio.len(),
                profile.sample_rate.hz()
            )));
        }
    }
    Ok(())
}

pub fn build_iat_frame(app_id: &str, kind: IatFrameKind, audio: &[u8]) -> Result<Value> {
    build_iat_frame_for_format(app_id, kind, audio, AudioFormat::Pcm16k)
}

pub fn build_iat_frame_for_format(
    app_id: &str,
    kind: IatFrameKind,
    audio: &[u8],
    format: AudioFormat,
) -> Result<Value> {
    build_iat_frame_for_profile(
        app_id,
        kind,
        audio,
        legacy_profile(format)?,
        IatProvider::SuperSmart,
    )
}

pub fn build_iat_frame_for_profile(
    app_id: &str,
    kind: IatFrameKind,
    audio: &[u8],
    profile: AudioProfile,
    provider: IatProvider,
) -> Result<Value> {
    match provider {
        IatProvider::SuperSmart => build_aiges_iat_frame(app_id, kind, audio, profile),
        IatProvider::Standard => build_standard_iat_frame(app_id, kind, audio, profile),
    }
}

fn build_aiges_iat_frame(
    app_id: &str,
    kind: IatFrameKind,
    audio: &[u8],
    profile: AudioProfile,
) -> Result<Value> {
    let encoding = iat_encoding(profile, IatProvider::SuperSmart)?;
    validate_frame_packet_for_provider(profile, audio, kind, IatProvider::SuperSmart)?;
    Ok(json!({
        "header": {
            "status": kind.status(),
            "app_id": app_id
        },
        "parameter": {
            "iat": {
                "domain": "slm",
                "language": "zh_cn",
                "accent": "mandarin",
                "dwa": "wpgs",
                "result": {
                    "encoding": "utf8",
                    "compress": "raw",
                    "format": "plain"
                }
            }
        },
        "payload": {
            "audio": {
                "audio": STANDARD.encode(audio),
                "sample_rate": profile.sample_rate.hz(),
                "encoding": encoding
            }
        }
    }))
}

pub fn build_standard_iat_frame(
    app_id: &str,
    kind: IatFrameKind,
    audio: &[u8],
    profile: AudioProfile,
) -> Result<Value> {
    let encoding = iat_encoding(profile, IatProvider::Standard)?;
    validate_frame_packet_for_provider(profile, audio, kind, IatProvider::Standard)?;
    let data = json!({
        "status": kind.status(),
        "format": format!("audio/L16;rate={}", profile.sample_rate.hz()),
        "encoding": encoding,
        "audio": STANDARD.encode(audio)
    });
    if kind != IatFrameKind::First {
        return Ok(json!({"data": data}));
    }

    let mut business = json!({
        "language": "zh_cn",
        "domain": "iat",
        "accent": "mandarin",
        "dwa": "wpgs"
    });
    if let Some(size) = speex_packet_size(profile) {
        business["speex_size"] = json!(size);
    }
    Ok(json!({
        "common": {"app_id": app_id},
        "business": business,
        "data": data
    }))
}

pub fn build_iat_segment_frames(
    app_id: &str,
    audio: &[u8],
    chunk_size: usize,
) -> Result<Vec<Value>> {
    build_iat_segment_frames_for_format(app_id, audio, chunk_size, AudioFormat::Pcm16k)
}

pub fn build_iat_segment_frames_for_format(
    app_id: &str,
    audio: &[u8],
    chunk_size: usize,
    format: AudioFormat,
) -> Result<Vec<Value>> {
    build_iat_segment_frames_for_profile(
        app_id,
        audio,
        chunk_size,
        legacy_profile(format)?,
        IatProvider::SuperSmart,
    )
}

pub fn build_iat_segment_frames_for_profile(
    app_id: &str,
    audio: &[u8],
    chunk_size: usize,
    profile: AudioProfile,
    provider: IatProvider,
) -> Result<Vec<Value>> {
    anyhow::ensure!(chunk_size > 0, "iat chunk size must be greater than zero");
    let mut frames = Vec::new();
    if audio.is_empty() {
        return Err(AudioPacketError::invalid("audio segment is empty").into());
    }
    let packet_size = if profile.format == AudioFormat::Speex {
        audio.len()
    } else {
        chunk_size
    };
    for (index, chunk) in audio.chunks(packet_size).enumerate() {
        let kind = if index == 0 {
            IatFrameKind::First
        } else {
            IatFrameKind::Continue
        };
        frames.push(build_iat_frame_for_profile(
            app_id, kind, chunk, profile, provider,
        )?);
    }
    frames.push(build_iat_frame_for_profile(
        app_id,
        IatFrameKind::Last,
        &[],
        profile,
        provider,
    )?);
    Ok(frames)
}

pub async fn recognize_pcm(config: &AppConfig, audio: &[u8]) -> Result<String> {
    recognize_audio_for_format(config, audio, AudioFormat::Pcm16k).await
}

pub async fn recognize_audio_for_format(
    config: &AppConfig,
    audio: &[u8],
    format: AudioFormat,
) -> Result<String> {
    recognize_audio(
        config,
        audio,
        legacy_profile(format)?,
        IatProvider::parse(&config.iat_provider)?,
    )
    .await
}

pub async fn recognize_audio(
    config: &AppConfig,
    audio: &[u8],
    profile: AudioProfile,
    provider: IatProvider,
) -> Result<String> {
    let frames =
        build_iat_segment_frames_for_profile(&config.app_id, audio, 1280, profile, provider)?;
    anyhow::ensure!(
        !config.api_key.trim().is_empty(),
        "XF_API_KEY is required for IAT"
    );
    anyhow::ensure!(
        !config.api_secret.trim().is_empty(),
        "XF_API_SECRET is required for IAT"
    );
    let signed_url = build_signed_ws_url(
        &config.iat_endpoint,
        &config.api_key,
        &config.api_secret,
        &current_rfc1123_date(),
    )?;
    let (mut socket, _) = connect_async(signed_url)
        .await
        .context("connect iat websocket")?;
    for frame in frames {
        socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .context("send iat frame")?;
        sleep(Duration::from_millis(5)).await;
    }

    let mut recognized = String::new();
    while let Some(message) = socket.next().await {
        let message = message.context("read iat websocket")?;
        let Message::Text(raw) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&raw).context("parse iat response")?;
        let parsed = parse_iat_text_for_provider(&value, provider)?;
        if !parsed.text.trim().is_empty() {
            tracing::info!(
                target: "mjy_voice_shop_rs::iat",
                text_len = parsed.text.chars().count(),
                provider = ?provider,
                final_frame = parsed.is_final,
                format = profile.format.as_str(),
                sample_rate = profile.sample_rate.hz(),
                "iat text chunk"
            );
            recognized = merge_iat_text(&recognized, &parsed.text);
        }
        if parsed.is_final {
            break;
        }
    }
    anyhow::ensure!(!recognized.trim().is_empty(), "IAT returned empty text");
    Ok(recognized)
}

pub fn merge_iat_text(current: &str, next: &str) -> String {
    let next = next.trim();
    if next.is_empty() {
        return current.to_string();
    }
    if current.is_empty() {
        return next.to_string();
    }
    if current.contains(next) {
        return current.to_string();
    }
    if next.contains(current) {
        return next.to_string();
    }
    format!("{current}{next}")
}

pub fn parse_iat_text(message: &Value) -> Result<IatText> {
    let code = message
        .pointer("/header/code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if code != 0 {
        let message = message
            .pointer("/header/message")
            .or_else(|| message.pointer("/message"))
            .and_then(Value::as_str)
            .unwrap_or("super smart IAT request rejected")
            .to_string();
        return Err(IatUpstreamError::new(IatProvider::SuperSmart, code, message).into());
    }
    let status = message
        .pointer("/header/status")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let Some(encoded) = message
        .pointer("/payload/result/text")
        .and_then(Value::as_str)
    else {
        return Ok(IatText {
            text: String::new(),
            is_final: status == 2,
        });
    };
    let decoded = STANDARD
        .decode(encoded)
        .context("invalid iat result base64")?;
    let payload: IatResultPayload =
        serde_json::from_slice(&decoded).context("invalid iat result json")?;
    Ok(IatText {
        text: iat_result_text(payload),
        is_final: status == 2,
    })
}

pub fn parse_standard_iat_text(message: &Value) -> Result<IatText> {
    let code = message.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        let message = message
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("standard IAT request rejected")
            .to_string();
        return Err(IatUpstreamError::new(IatProvider::Standard, code, message).into());
    }
    let status = message
        .pointer("/data/status")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let Some(result) = message.pointer("/data/result") else {
        return Ok(IatText {
            text: String::new(),
            is_final: status == 2,
        });
    };
    let payload: IatResultPayload =
        serde_json::from_value(result.clone()).context("invalid standard iat result json")?;
    Ok(IatText {
        text: iat_result_text(payload),
        is_final: status == 2,
    })
}

pub fn parse_iat_text_for_provider(message: &Value, provider: IatProvider) -> Result<IatText> {
    match provider {
        IatProvider::SuperSmart => parse_iat_text(message),
        IatProvider::Standard => parse_standard_iat_text(message),
    }
}

fn iat_result_text(payload: IatResultPayload) -> String {
    let mut text = String::new();
    for segment in payload.ws {
        if let Some(candidate) = segment.cw.into_iter().next() {
            text.push_str(&candidate.w);
        }
    }
    text
}

#[derive(Debug, Deserialize, Serialize)]
struct IatResultPayload {
    ws: Vec<IatSegment>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IatSegment {
    cw: Vec<IatCandidate>,
}

#[derive(Debug, Deserialize, Serialize)]
struct IatCandidate {
    w: String,
}
