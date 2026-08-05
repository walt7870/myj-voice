use std::{
    collections::VecDeque,
    future::Future,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::xfyun::auth::{build_signed_ws_url, current_rfc1123_date};
use crate::{
    config::AppConfig,
    domain::audio::{AudioFormat, AudioProfile, AudioSampleRate},
    xfyun::audio::{tts_supports, TtsProvider},
};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TtsAudioProfileError {
    profile: AudioProfile,
    provider: TtsProvider,
}

impl TtsAudioProfileError {
    fn unsupported(profile: AudioProfile, provider: TtsProvider) -> Self {
        Self { profile, provider }
    }

    pub fn code(&self) -> &'static str {
        "unsupported_audio_profile"
    }
}

impl std::fmt::Display for TtsAudioProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "unsupported TTS audio profile for provider {}: format={}, rate={}",
            tts_provider_name(self.provider),
            self.profile.format.as_str(),
            self.profile.sample_rate.hz()
        )
    }
}

impl std::error::Error for TtsAudioProfileError {}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TtsUpstreamErrorKind {
    AudioProfileRejected,
    Other,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TtsUpstreamError {
    pub provider: TtsProvider,
    pub code: i64,
    pub message: String,
    pub kind: TtsUpstreamErrorKind,
}

impl TtsUpstreamError {
    fn new(provider: TtsProvider, code: i64, message: String) -> Self {
        let kind = classify_tts_upstream_error(provider, code, &message);
        Self {
            provider,
            code,
            message,
            kind,
        }
    }

    pub fn is_audio_profile_rejection(&self) -> bool {
        self.kind == TtsUpstreamErrorKind::AudioProfileRejected
    }
}

fn classify_tts_upstream_error(
    provider: TtsProvider,
    code: i64,
    message: &str,
) -> TtsUpstreamErrorKind {
    match (provider, code) {
        (TtsProvider::Standard | TtsProvider::SuperSmart, 10043) => {
            TtsUpstreamErrorKind::AudioProfileRejected
        }
        (TtsProvider::Standard | TtsProvider::SuperSmart, 10006 | 10007)
            if contains_explicit_audio_field(message) =>
        {
            TtsUpstreamErrorKind::AudioProfileRejected
        }
        (TtsProvider::Standard | TtsProvider::SuperSmart, 10163)
            if contains_explicit_audio_field(message) || contains_explicit_codec_name(message) =>
        {
            TtsUpstreamErrorKind::AudioProfileRejected
        }
        _ => TtsUpstreamErrorKind::Other,
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

impl std::fmt::Display for TtsUpstreamError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tts upstream error code {}: {}",
            self.code, self.message
        )
    }
}

impl std::error::Error for TtsUpstreamError {}

fn tts_provider_name(provider: TtsProvider) -> &'static str {
    match provider {
        TtsProvider::SuperSmart => "super_smart",
        TtsProvider::Standard => "standard",
    }
}

pub fn tts_encoding(
    profile: AudioProfile,
    provider: TtsProvider,
) -> std::result::Result<&'static str, TtsAudioProfileError> {
    if !tts_supports(provider, profile) {
        return Err(TtsAudioProfileError::unsupported(profile, provider));
    }

    match (provider, profile.format, profile.sample_rate) {
        (TtsProvider::SuperSmart, AudioFormat::Mp3, _) => Ok("lame"),
        (TtsProvider::SuperSmart, AudioFormat::Pcm, AudioSampleRate::Hz16000) => Ok("raw"),
        (TtsProvider::Standard, AudioFormat::Mp3, _) => Ok("lame"),
        (TtsProvider::Standard, AudioFormat::Pcm, _) => Ok("raw"),
        (TtsProvider::Standard, AudioFormat::Opus, AudioSampleRate::Hz8000) => Ok("opus"),
        (TtsProvider::Standard, AudioFormat::Opus, AudioSampleRate::Hz16000) => Ok("opus-wb"),
        (TtsProvider::Standard, AudioFormat::Speex, AudioSampleRate::Hz8000) => {
            Ok("speex-org-nb;7")
        }
        (TtsProvider::Standard, AudioFormat::Speex, AudioSampleRate::Hz16000) => {
            Ok("speex-org-wb;7")
        }
        _ => Err(TtsAudioProfileError::unsupported(profile, provider)),
    }
}

fn legacy_tts_profile(format: AudioFormat) -> AudioProfile {
    match format {
        AudioFormat::Mp3 => AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
        AudioFormat::Pcm16k => AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
        _ => panic!("legacy TTS API only supports mp3 or pcm16k"),
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TtsAudioChunk {
    pub audio: Vec<u8>,
    pub is_last: bool,
}

#[derive(Debug, Clone)]
pub struct TimedTtsAudioChunk {
    pub audio: Vec<u8>,
    pub is_last: bool,
    pub relay_started: Instant,
}

pub struct TtsAudioStream {
    receiver: mpsc::Receiver<Result<TimedTtsAudioChunk>>,
    task: tokio::task::JoinHandle<()>,
}

impl TtsAudioStream {
    pub fn into_parts(
        self,
    ) -> (
        mpsc::Receiver<Result<TimedTtsAudioChunk>>,
        tokio::task::JoinHandle<()>,
    ) {
        (self.receiver, self.task)
    }

    fn into_receiver(self) -> mpsc::Receiver<Result<TimedTtsAudioChunk>> {
        self.receiver
    }
}

impl TimedTtsAudioChunk {
    fn new(chunk: TtsAudioChunk, relay_started: Instant) -> Self {
        Self {
            audio: chunk.audio,
            is_last: chunk.is_last,
            relay_started,
        }
    }

    fn into_chunk(self) -> TtsAudioChunk {
        TtsAudioChunk {
            audio: self.audio,
            is_last: self.is_last,
        }
    }
}

pub const MAX_STANDARD_OPUS_PACKET_BYTES: usize = 1_275;
pub const MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES: usize = 64 * 1024;
pub const MAX_STANDARD_TTS_PROVIDER_BLOCK_BASE64_BYTES: usize =
    MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES.div_ceil(3) * 4;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TtsPacketizationErrorKind {
    ProviderBlockTooLarge,
    InvalidPacketLength,
    TruncatedLengthPrefix,
    TruncatedPacket,
    EmptyCompressedStream,
    StreamAlreadyFinished,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TtsPacketizationError {
    pub profile: AudioProfile,
    pub kind: TtsPacketizationErrorKind,
    pub buffered_bytes: usize,
}

impl TtsPacketizationError {
    fn new(profile: AudioProfile, kind: TtsPacketizationErrorKind, buffered_bytes: usize) -> Self {
        Self {
            profile,
            kind,
            buffered_bytes,
        }
    }
}

impl std::fmt::Display for TtsPacketizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "invalid standard TTS {} packet stream at {}Hz: {:?} ({} buffered bytes)",
            self.profile.format.as_str(),
            self.profile.sample_rate.hz(),
            self.kind,
            self.buffered_bytes
        )
    }
}

impl std::error::Error for TtsPacketizationError {}

#[derive(Debug)]
pub struct StandardTtsPacketizer {
    profile: AudioProfile,
    pending: Vec<u8>,
    held_packet: Option<Vec<u8>>,
    finished: bool,
}

impl StandardTtsPacketizer {
    pub fn new(profile: AudioProfile) -> Result<Self> {
        tts_encoding(profile, TtsProvider::Standard)?;
        Ok(Self {
            profile,
            pending: Vec::new(),
            held_packet: None,
            finished: false,
        })
    }

    pub fn buffered_bytes(&self) -> usize {
        self.pending.len()
    }

    pub fn held_packet_bytes(&self) -> usize {
        self.held_packet.as_ref().map_or(0, Vec::len)
    }

    fn is_compressed(&self) -> bool {
        matches!(self.profile.format, AudioFormat::Opus | AudioFormat::Speex)
    }

    pub fn push(&mut self, chunk: TtsAudioChunk) -> Result<Vec<TtsAudioChunk>> {
        if self.finished {
            return Err(TtsPacketizationError::new(
                self.profile,
                TtsPacketizationErrorKind::StreamAlreadyFinished,
                self.pending.len(),
            )
            .into());
        }

        if !self.is_compressed() {
            self.finished = chunk.is_last;
            return Ok(vec![chunk]);
        }

        let retained_batch_bytes = self
            .pending
            .len()
            .saturating_add(self.held_packet_bytes())
            .saturating_add(chunk.audio.len());
        if chunk.audio.len() > MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES
            || retained_batch_bytes > MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES
        {
            return Err(TtsPacketizationError::new(
                self.profile,
                TtsPacketizationErrorKind::ProviderBlockTooLarge,
                self.pending.len(),
            )
            .into());
        }

        self.pending.extend_from_slice(&chunk.audio);
        let mut packets = match self.profile.format {
            AudioFormat::Opus => self.take_opus_packets()?,
            AudioFormat::Speex => self.take_speex_packets(),
            _ => unreachable!("continuous formats returned above"),
        };

        if chunk.is_last {
            self.ensure_no_truncated_final()?;
            if packets.is_empty() {
                let Some(audio) = self.held_packet.take() else {
                    return Err(TtsPacketizationError::new(
                        self.profile,
                        TtsPacketizationErrorKind::EmptyCompressedStream,
                        0,
                    )
                    .into());
                };
                self.finished = true;
                return Ok(vec![TtsAudioChunk {
                    audio,
                    is_last: true,
                }]);
            }
            if let Some(held) = self.held_packet.take() {
                packets.insert(
                    0,
                    TtsAudioChunk {
                        audio: held,
                        is_last: false,
                    },
                );
            }
            if let Some(last) = packets.last_mut() {
                last.is_last = true;
            }
            self.finished = true;
            return Ok(packets);
        }

        if packets.is_empty() {
            return Ok(Vec::new());
        }

        let newest = packets.pop().expect("non-empty packet list").audio;
        if let Some(held) = self.held_packet.replace(newest) {
            packets.insert(
                0,
                TtsAudioChunk {
                    audio: held,
                    is_last: false,
                },
            );
        }
        Ok(packets)
    }

    fn take_opus_packets(&mut self) -> Result<Vec<TtsAudioChunk>> {
        let mut packets = Vec::new();
        let mut consumed = 0usize;
        loop {
            let remaining = &self.pending[consumed..];
            if remaining.len() < 2 {
                break;
            }
            let packet_len = u16::from_be_bytes([remaining[0], remaining[1]]) as usize;
            if packet_len == 0 || packet_len > MAX_STANDARD_OPUS_PACKET_BYTES {
                return Err(TtsPacketizationError::new(
                    self.profile,
                    TtsPacketizationErrorKind::InvalidPacketLength,
                    self.pending.len(),
                )
                .into());
            }
            if remaining.len() < packet_len + 2 {
                break;
            }
            packets.push(TtsAudioChunk {
                audio: remaining[2..packet_len + 2].to_vec(),
                is_last: false,
            });
            consumed += packet_len + 2;
        }
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        Ok(packets)
    }

    fn take_speex_packets(&mut self) -> Vec<TtsAudioChunk> {
        let frame_size = match self.profile.sample_rate {
            AudioSampleRate::Hz8000 => 38,
            AudioSampleRate::Hz16000 => 60,
            AudioSampleRate::Hz24000 => unreachable!("standard TTS does not support Speex 24kHz"),
        };
        let mut packets = Vec::new();
        let complete_bytes = self.pending.len() / frame_size * frame_size;
        for frame in self.pending[..complete_bytes].chunks_exact(frame_size) {
            packets.push(TtsAudioChunk {
                audio: frame.to_vec(),
                is_last: false,
            });
        }
        if complete_bytes > 0 {
            self.pending.drain(..complete_bytes);
        }
        packets
    }

    fn ensure_no_truncated_final(&self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let kind = if self.profile.format == AudioFormat::Opus && self.pending.len() < 2 {
            TtsPacketizationErrorKind::TruncatedLengthPrefix
        } else {
            TtsPacketizationErrorKind::TruncatedPacket
        };
        Err(TtsPacketizationError::new(self.profile, kind, self.pending.len()).into())
    }
}

#[derive(Debug)]
pub struct TimedStandardTtsPacketizer {
    inner: StandardTtsPacketizer,
    held_started: Option<Instant>,
    pending_started: Option<Instant>,
}

impl TimedStandardTtsPacketizer {
    pub fn new(profile: AudioProfile) -> Result<Self> {
        Ok(Self {
            inner: StandardTtsPacketizer::new(profile)?,
            held_started: None,
            pending_started: None,
        })
    }

    pub fn inner(&self) -> &StandardTtsPacketizer {
        &self.inner
    }

    pub fn push(&mut self, chunk: TimedTtsAudioChunk) -> Result<Vec<TimedTtsAudioChunk>> {
        let started = chunk.relay_started;
        let is_last = chunk.is_last;
        let before_pending = self.inner.buffered_bytes();
        let old_pending_started = self.pending_started;
        let old_held_started = self.held_started;
        let packets = self.inner.push(chunk.into_chunk())?;

        if !self.inner.is_compressed() {
            return Ok(packets
                .into_iter()
                .map(|packet| TimedTtsAudioChunk::new(packet, started))
                .collect());
        }

        let old_held_count = usize::from(old_held_started.is_some() && !packets.is_empty());
        let new_completed = if is_last {
            packets.len().saturating_sub(old_held_count)
        } else if packets.is_empty() {
            usize::from(self.inner.held_packet_bytes() > 0 && old_held_started.is_none())
        } else {
            packets.len() + 1 - old_held_count
        };
        let first_new_started = if before_pending > 0 {
            old_pending_started.unwrap_or(started)
        } else {
            started
        };
        let mut starts = VecDeque::with_capacity(packets.len());
        if let Some(old_held_started) = old_held_started.filter(|_| !packets.is_empty()) {
            starts.push_back(old_held_started);
        }
        for index in 0..new_completed {
            starts.push_back(if index == 0 {
                first_new_started
            } else {
                started
            });
        }

        self.held_started = if !is_last && new_completed > 0 {
            starts.pop_back()
        } else if packets.is_empty() {
            old_held_started
        } else {
            None
        };
        self.pending_started = if self.inner.buffered_bytes() == 0 {
            None
        } else if new_completed > 0 || before_pending == 0 {
            Some(started)
        } else {
            old_pending_started.or(Some(started))
        };

        anyhow::ensure!(
            starts.len() == packets.len(),
            "timed standard TTS packet accounting mismatch"
        );
        Ok(packets
            .into_iter()
            .zip(starts)
            .map(|(packet, packet_started)| TimedTtsAudioChunk::new(packet, packet_started))
            .collect())
    }
}

fn standard_base64_exact_decoded_len(encoded: &str) -> Option<usize> {
    let bytes = encoded.as_bytes();
    if bytes.is_empty() {
        return Some(0);
    }
    if bytes.len() % 4 != 0 {
        return None;
    }

    let padding = bytes.iter().rev().take_while(|byte| **byte == b'=').count();
    if padding > 2 {
        return None;
    }
    let data_len = bytes.len() - padding;
    if bytes[..data_len]
        .iter()
        .any(|byte| standard_base64_sextet(*byte).is_none())
        || bytes[..data_len].contains(&b'=')
    {
        return None;
    }

    match padding {
        1 if standard_base64_sextet(bytes[bytes.len() - 2])? & 0b11 != 0 => return None,
        2 if standard_base64_sextet(bytes[bytes.len() - 3])? & 0b1111 != 0 => return None,
        _ => {}
    }
    Some(bytes.len() / 4 * 3 - padding)
}

fn standard_base64_sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub fn precheck_standard_tts_provider_audio(
    encoded: &str,
    packetizer: &StandardTtsPacketizer,
) -> Result<()> {
    if !packetizer.is_compressed() {
        return Ok(());
    }
    if encoded.len() > MAX_STANDARD_TTS_PROVIDER_BLOCK_BASE64_BYTES {
        return Err(TtsPacketizationError::new(
            packetizer.profile,
            TtsPacketizationErrorKind::ProviderBlockTooLarge,
            packetizer.buffered_bytes(),
        )
        .into());
    }
    let Some(decoded_bytes) = standard_base64_exact_decoded_len(encoded) else {
        return Ok(());
    };
    let retained_batch_bytes = packetizer
        .buffered_bytes()
        .saturating_add(packetizer.held_packet_bytes())
        .saturating_add(decoded_bytes);
    if decoded_bytes > MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES
        || retained_batch_bytes > MAX_STANDARD_TTS_PROVIDER_BLOCK_BYTES
    {
        return Err(TtsPacketizationError::new(
            packetizer.profile,
            TtsPacketizationErrorKind::ProviderBlockTooLarge,
            packetizer.buffered_bytes(),
        )
        .into());
    }
    Ok(())
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TtsTextFrame {
    pub text: String,
    pub status: u8,
    pub seq: u32,
}

pub fn build_tts_payload(app_id: &str, voice: &str, text: &str) -> Value {
    build_tts_payload_for_format(app_id, voice, text, AudioFormat::Mp3)
}

pub fn build_tts_payload_for_format(
    app_id: &str,
    voice: &str,
    text: &str,
    format: AudioFormat,
) -> Value {
    build_tts_payload_for_profile(app_id, voice, text, legacy_tts_profile(format))
        .expect("legacy TTS profile must be supported")
}

pub fn build_tts_payload_for_profile(
    app_id: &str,
    voice: &str,
    text: &str,
    profile: AudioProfile,
) -> std::result::Result<Value, TtsAudioProfileError> {
    build_tts_payload_frame_for_profile(app_id, voice, text, 2, 0, profile)
}

pub fn build_tts_payload_frame(
    app_id: &str,
    voice: &str,
    text: &str,
    status: u8,
    seq: u32,
) -> Value {
    build_tts_payload_frame_for_format(app_id, voice, text, status, seq, AudioFormat::Mp3)
}

pub fn build_tts_payload_frame_for_format(
    app_id: &str,
    voice: &str,
    text: &str,
    status: u8,
    seq: u32,
    format: AudioFormat,
) -> Value {
    build_tts_payload_frame_for_profile(
        app_id,
        voice,
        text,
        status,
        seq,
        legacy_tts_profile(format),
    )
    .expect("legacy TTS profile must be supported")
}

pub fn build_tts_payload_frame_for_profile(
    app_id: &str,
    voice: &str,
    text: &str,
    status: u8,
    seq: u32,
    profile: AudioProfile,
) -> std::result::Result<Value, TtsAudioProfileError> {
    let encoding = tts_encoding(profile, TtsProvider::SuperSmart)?;

    Ok(json!({
        "header": {
            "app_id": app_id,
            "status": status
        },
        "parameter": {
            "tts": {
                "vcn": voice,
                "volume": 50,
                "rhy": 0,
                "speed": 50,
                "pitch": 50,
                "bgs": 0,
                "reg": 0,
                "rdn": 0,
                "audio": {
                    "encoding": encoding,
                    "sample_rate": profile.sample_rate.hz(),
                    "channels": profile.channels(),
                    "bit_depth": profile.bit_depth().unwrap_or(16),
                    "frame_size": 0
                }
            }
        },
        "payload": {
            "text": {
                "encoding": "utf8",
                "compress": "raw",
                "format": "plain",
                "status": status,
                "seq": seq,
                "text": STANDARD.encode(text.as_bytes())
            }
        }
    }))
}

pub fn build_standard_tts_payload(app_id: &str, voice: &str, text: &str) -> Value {
    build_standard_tts_payload_for_format(app_id, voice, text, AudioFormat::Mp3)
}

pub fn build_standard_tts_payload_for_format(
    app_id: &str,
    voice: &str,
    text: &str,
    format: AudioFormat,
) -> Value {
    build_standard_tts_payload_for_profile(app_id, voice, text, legacy_tts_profile(format))
        .expect("legacy standard TTS profile must be supported")
}

pub fn build_standard_tts_payload_for_profile(
    app_id: &str,
    voice: &str,
    text: &str,
    profile: AudioProfile,
) -> std::result::Result<Value, TtsAudioProfileError> {
    let encoding = tts_encoding(profile, TtsProvider::Standard)?;
    let mut payload = json!({
        "common": {
            "app_id": app_id
        },
        "business": {
            "aue": encoding,
            "auf": format!("audio/L16;rate={}", profile.sample_rate.hz()),
            "vcn": voice,
            "tte": "utf8",
            "speed": 50,
            "volume": 50,
            "pitch": 50
        },
        "data": {
            "status": 2,
            "text": STANDARD.encode(text.as_bytes())
        }
    });
    if profile.format == AudioFormat::Mp3 {
        payload["business"]["sfl"] = json!(1);
    }
    Ok(payload)
}

pub fn parse_tts_audio(message: &Value) -> Result<TtsAudioChunk> {
    parse_tts_audio_frame(message)?.context("missing tts audio payload")
}

pub fn parse_tts_audio_frame(message: &Value) -> Result<Option<TtsAudioChunk>> {
    let code = message
        .pointer("/header/code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if code != 0 {
        let detail = message
            .pointer("/header/message")
            .or_else(|| message.pointer("/message"))
            .and_then(Value::as_str)
            .unwrap_or("unknown upstream error");
        return Err(
            TtsUpstreamError::new(TtsProvider::SuperSmart, code, detail.to_string()).into(),
        );
    }
    let Some(encoded) = message
        .pointer("/payload/audio/audio")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let status = message
        .pointer("/payload/audio/status")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Ok(Some(TtsAudioChunk {
        audio: STANDARD
            .decode(encoded)
            .context("invalid tts audio base64")?,
        is_last: status == 2,
    }))
}

pub fn parse_standard_tts_audio(message: &Value) -> Result<TtsAudioChunk> {
    parse_standard_tts_audio_frame(message)?.context("missing standard tts data.audio")
}

pub fn parse_standard_tts_audio_frame(message: &Value) -> Result<Option<TtsAudioChunk>> {
    let code = message
        .pointer("/code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if code != 0 {
        let detail = message
            .pointer("/message")
            .and_then(Value::as_str)
            .unwrap_or("unknown upstream error");
        return Err(TtsUpstreamError::new(TtsProvider::Standard, code, detail.to_string()).into());
    }
    let Some(encoded) = message.pointer("/data/audio").and_then(Value::as_str) else {
        return Ok(None);
    };
    let status = message
        .pointer("/data/status")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    Ok(Some(TtsAudioChunk {
        audio: STANDARD
            .decode(encoded)
            .context("invalid standard tts audio base64")?,
        is_last: status == 2,
    }))
}

pub async fn forward_standard_tts_audio_frame(
    message: &Value,
    packetizer: &mut StandardTtsPacketizer,
    tx: &mpsc::Sender<Result<TtsAudioChunk>>,
) -> Result<Option<bool>> {
    if message
        .pointer("/code")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        == 0
    {
        if let Some(encoded) = message.pointer("/data/audio").and_then(Value::as_str) {
            precheck_standard_tts_provider_audio(encoded, packetizer)?;
        }
    }
    let provider_chunk = match parse_standard_tts_audio_frame(message)? {
        Some(chunk) => chunk,
        None if packetizer.is_compressed()
            && message.pointer("/data/status").and_then(Value::as_i64) == Some(2) =>
        {
            TtsAudioChunk {
                audio: Vec::new(),
                is_last: true,
            }
        }
        None => return Ok(None),
    };
    let is_last = provider_chunk.is_last;
    for packet in packetizer.push(provider_chunk)? {
        tx.send(Ok(packet))
            .await
            .map_err(|_| anyhow::anyhow!("TTS audio receiver closed"))?;
    }
    Ok(Some(is_last))
}

pub async fn forward_tts_audio_frame(
    message: &Value,
    tx: &mpsc::Sender<Result<TtsAudioChunk>>,
) -> Result<Option<bool>> {
    let Some(chunk) = parse_tts_audio_frame(message)? else {
        return Ok(None);
    };
    let is_last = chunk.is_last;
    tx.send(Ok(chunk))
        .await
        .map_err(|_| anyhow::anyhow!("TTS audio receiver closed"))?;
    Ok(Some(is_last))
}

async fn forward_standard_tts_audio_frame_timed(
    message: &Value,
    packetizer: &mut TimedStandardTtsPacketizer,
    tx: &mpsc::Sender<Result<TimedTtsAudioChunk>>,
) -> Result<Option<bool>> {
    let provider_block_started = Instant::now();
    if message
        .pointer("/code")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        == 0
    {
        if let Some(encoded) = message.pointer("/data/audio").and_then(Value::as_str) {
            precheck_standard_tts_provider_audio(encoded, packetizer.inner())?;
        }
    }
    let provider_chunk = match parse_standard_tts_audio_frame(message)? {
        Some(chunk) => chunk,
        None if packetizer.inner().is_compressed()
            && message.pointer("/data/status").and_then(Value::as_i64) == Some(2) =>
        {
            TtsAudioChunk {
                audio: Vec::new(),
                is_last: true,
            }
        }
        None => return Ok(None),
    };
    let is_last = provider_chunk.is_last;
    for packet in packetizer.push(TimedTtsAudioChunk::new(
        provider_chunk,
        provider_block_started,
    ))? {
        tx.send(Ok(packet))
            .await
            .map_err(|_| anyhow::anyhow!("TTS audio receiver closed"))?;
    }
    Ok(Some(is_last))
}

async fn forward_tts_audio_frame_timed(
    message: &Value,
    tx: &mpsc::Sender<Result<TimedTtsAudioChunk>>,
) -> Result<Option<bool>> {
    let provider_block_started = Instant::now();
    let Some(chunk) = parse_tts_audio_frame(message)? else {
        return Ok(None);
    };
    let is_last = chunk.is_last;
    tx.send(Ok(TimedTtsAudioChunk::new(chunk, provider_block_started)))
        .await
        .map_err(|_| anyhow::anyhow!("TTS audio receiver closed"))?;
    Ok(Some(is_last))
}

#[derive(Debug, Default)]
pub struct TtsStreamProgress {
    received_audio: bool,
    received_final: bool,
}

impl TtsStreamProgress {
    pub fn observe(&mut self, is_last: bool) {
        self.received_audio = true;
        self.received_final |= is_last;
    }

    pub fn ensure_complete(&self) -> Result<()> {
        anyhow::ensure!(self.received_final, "TTS stream ended before final frame");
        anyhow::ensure!(self.received_audio, "TTS returned no audio frames");
        Ok(())
    }
}

pub async fn run_tts_stream_session<F>(
    audio_tx: &mpsc::Sender<Result<TtsAudioChunk>>,
    timeout_duration: Duration,
    session: F,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    tokio::select! {
        biased;
        _ = audio_tx.closed() => anyhow::bail!("TTS audio receiver closed"),
        result = tokio::time::timeout(timeout_duration, session) => match result {
            Ok(result) => result,
            Err(_) => anyhow::bail!("TTS stream timed out before completion"),
        }
    }
}

async fn run_timed_tts_stream_session<F>(
    audio_tx: &mpsc::Sender<Result<TimedTtsAudioChunk>>,
    timeout_duration: Duration,
    session: F,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    tokio::select! {
        biased;
        _ = audio_tx.closed() => anyhow::bail!("TTS audio receiver closed"),
        result = tokio::time::timeout(timeout_duration, session) => match result {
            Ok(result) => result,
            Err(_) => anyhow::bail!("TTS stream timed out before completion"),
        }
    }
}

pub async fn couple_tts_text_io<W, R>(writer: W, reader: R) -> Result<()>
where
    W: Future<Output = Result<()>>,
    R: Future<Output = Result<()>>,
{
    tokio::try_join!(writer, reader)?;
    Ok(())
}

pub async fn synthesize_mp3_chunks(config: &AppConfig, text: &str) -> Result<Vec<Vec<u8>>> {
    match TtsProvider::parse(&config.tts_provider)? {
        TtsProvider::Standard => synthesize_standard_tts_chunks(config, text).await,
        TtsProvider::SuperSmart => synthesize_super_smart_tts_chunks(config, text).await,
    }
}

pub async fn stream_mp3_chunks(
    config: AppConfig,
    text: String,
) -> mpsc::Receiver<Result<TimedTtsAudioChunk>> {
    stream_audio_chunks(config, text, AudioFormat::Mp3).await
}

pub async fn stream_audio_chunks(
    config: AppConfig,
    text: String,
    format: AudioFormat,
) -> mpsc::Receiver<Result<TimedTtsAudioChunk>> {
    let profile = legacy_tts_profile(format);
    let provider = match TtsProvider::parse(&config.tts_provider) {
        Ok(provider) => provider,
        Err(error) => return receiver_with_error(error.into()),
    };
    stream_audio_profile_chunks(config, text, profile, provider).await
}

pub async fn stream_audio_profile_chunks(
    config: AppConfig,
    text: String,
    profile: AudioProfile,
    provider: TtsProvider,
) -> mpsc::Receiver<Result<TimedTtsAudioChunk>> {
    start_audio_profile_chunks(config, text, profile, provider).into_receiver()
}

pub fn start_audio_profile_chunks(
    config: AppConfig,
    text: String,
    profile: AudioProfile,
    provider: TtsProvider,
) -> TtsAudioStream {
    let (tx, rx) = mpsc::channel(32);
    let task = tokio::spawn(async move {
        let result = async {
            let configured = TtsProvider::parse(&config.tts_provider)?;
            anyhow::ensure!(
                configured == provider,
                "TTS provider mismatch: config={}, requested={}",
                tts_provider_name(configured),
                tts_provider_name(provider)
            );
            tts_encoding(profile, provider)?;
            match provider {
                TtsProvider::Standard => {
                    stream_standard_tts_chunks(&config, &text, profile, tx.clone()).await
                }
                TtsProvider::SuperSmart => {
                    stream_super_smart_tts_chunks(&config, &text, profile, tx.clone()).await
                }
            }
        }
        .await;
        if let Err(error) = result {
            let _ = tx.send(Err(error)).await;
        };
    });
    TtsAudioStream { receiver: rx, task }
}

fn receiver_with_error(error: anyhow::Error) -> mpsc::Receiver<Result<TimedTtsAudioChunk>> {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let _ = tx.send(Err(error)).await;
    });
    rx
}

pub async fn stream_super_smart_tts_text_frames(
    config: AppConfig,
    text_rx: mpsc::Receiver<TtsTextFrame>,
) -> mpsc::Receiver<Result<TimedTtsAudioChunk>> {
    stream_super_smart_tts_text_frames_for_format(config, text_rx, AudioFormat::Mp3).await
}

pub async fn stream_super_smart_tts_text_frames_for_format(
    config: AppConfig,
    text_rx: mpsc::Receiver<TtsTextFrame>,
    format: AudioFormat,
) -> mpsc::Receiver<Result<TimedTtsAudioChunk>> {
    stream_super_smart_tts_text_frames_for_profile(config, text_rx, legacy_tts_profile(format))
        .await
}

pub async fn stream_super_smart_tts_text_frames_for_profile(
    config: AppConfig,
    text_rx: mpsc::Receiver<TtsTextFrame>,
    profile: AudioProfile,
) -> mpsc::Receiver<Result<TimedTtsAudioChunk>> {
    start_super_smart_tts_text_frames_for_profile(config, text_rx, profile).into_receiver()
}

pub fn start_super_smart_tts_text_frames_for_profile(
    config: AppConfig,
    text_rx: mpsc::Receiver<TtsTextFrame>,
    profile: AudioProfile,
) -> TtsAudioStream {
    let (audio_tx, audio_rx) = mpsc::channel(64);
    let task = tokio::spawn(async move {
        let session = async {
            let provider = TtsProvider::parse(&config.tts_provider)?;
            anyhow::ensure!(
                provider == TtsProvider::SuperSmart,
                "streaming text frames require super_smart TTS provider"
            );
            stream_super_smart_tts_text_frames_inner(&config, text_rx, profile, audio_tx.clone())
                .await
        };
        let result =
            run_timed_tts_stream_session(&audio_tx, Duration::from_secs(75), session).await;
        if let Err(error) = result {
            let _ = audio_tx.send(Err(error)).await;
        }
    });
    TtsAudioStream {
        receiver: audio_rx,
        task,
    }
}

async fn stream_super_smart_tts_text_frames_inner(
    config: &AppConfig,
    mut text_rx: mpsc::Receiver<TtsTextFrame>,
    profile: AudioProfile,
    audio_tx: mpsc::Sender<Result<TimedTtsAudioChunk>>,
) -> Result<()> {
    tts_encoding(profile, TtsProvider::SuperSmart)?;
    let first_frame = tokio::select! {
        biased;
        _ = audio_tx.closed() => anyhow::bail!("TTS audio receiver closed"),
        frame = text_rx.recv() => frame.context("streaming TTS received no text frames")?,
    };
    anyhow::ensure!(
        !config.api_key.trim().is_empty(),
        "XF_API_KEY is required for TTS"
    );
    anyhow::ensure!(
        !config.api_secret.trim().is_empty(),
        "XF_API_SECRET is required for TTS"
    );
    let signed_url = build_signed_ws_url(
        &config.tts_endpoint,
        &config.api_key,
        &config.api_secret,
        &current_rfc1123_date(),
    )?;
    let connect = connect_async(signed_url);
    let (socket, _) = tokio::select! {
        biased;
        _ = audio_tx.closed() => anyhow::bail!("TTS audio receiver closed"),
        result = connect => result.context("connect streaming tts websocket")?,
    };
    let (mut writer, mut reader) = socket.split();
    let app_id = config.app_id.clone();
    let voice = config.tts_voice.clone();

    let writer = async move {
        let mut frame = first_frame;
        loop {
            let payload = build_tts_payload_frame_for_profile(
                &app_id,
                &voice,
                &frame.text,
                frame.status,
                frame.seq,
                profile,
            )?;
            writer
                .send(Message::Text(payload.to_string().into()))
                .await
                .context("send streaming tts text frame")?;
            if frame.status == 2 {
                return Ok(());
            }
            frame = text_rx
                .recv()
                .await
                .context("streaming TTS text input closed before final status")?;
        }
    };

    let reader = async move {
        let mut progress = TtsStreamProgress::default();
        while let Some(message) = reader.next().await {
            let message = message.context("read streaming tts websocket")?;
            let Message::Text(raw) = message else {
                continue;
            };
            let value: Value =
                serde_json::from_str(&raw).context("parse streaming tts response")?;
            append_tts_log(&format_tts_response_summary("super_smart_stream", &value));
            if let Some(is_last) = forward_tts_audio_frame_timed(&value, &audio_tx).await? {
                progress.observe(is_last);
                if is_last {
                    break;
                }
            }
        }
        progress.ensure_complete()
    };

    couple_tts_text_io(writer, reader).await
}

async fn stream_super_smart_tts_chunks(
    config: &AppConfig,
    text: &str,
    profile: AudioProfile,
    tx: mpsc::Sender<Result<TimedTtsAudioChunk>>,
) -> Result<()> {
    tts_encoding(profile, TtsProvider::SuperSmart)?;
    anyhow::ensure!(
        !config.api_key.trim().is_empty(),
        "XF_API_KEY is required for TTS"
    );
    anyhow::ensure!(
        !config.api_secret.trim().is_empty(),
        "XF_API_SECRET is required for TTS"
    );
    let signed_url = build_signed_ws_url(
        &config.tts_endpoint,
        &config.api_key,
        &config.api_secret,
        &current_rfc1123_date(),
    )?;
    let (mut socket, _) = connect_async(signed_url)
        .await
        .context("connect tts websocket")?;
    socket
        .send(Message::Text(
            build_tts_payload_for_profile(&config.app_id, &config.tts_voice, text, profile)?
                .to_string()
                .into(),
        ))
        .await
        .context("send tts payload")?;

    let mut progress = TtsStreamProgress::default();
    while let Some(message) = socket.next().await {
        let message = message.context("read tts websocket")?;
        let Message::Text(raw) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&raw).context("parse tts response")?;
        append_tts_log(&format_tts_response_summary("super_smart", &value));
        if let Some(is_last) = forward_tts_audio_frame_timed(&value, &tx).await? {
            progress.observe(is_last);
            if is_last {
                break;
            }
        }
    }
    progress.ensure_complete()
}

async fn stream_standard_tts_chunks(
    config: &AppConfig,
    text: &str,
    profile: AudioProfile,
    tx: mpsc::Sender<Result<TimedTtsAudioChunk>>,
) -> Result<()> {
    tts_encoding(profile, TtsProvider::Standard)?;
    anyhow::ensure!(
        !config.api_key.trim().is_empty(),
        "XF_API_KEY is required for standard TTS"
    );
    anyhow::ensure!(
        !config.api_secret.trim().is_empty(),
        "XF_API_SECRET is required for standard TTS"
    );
    let signed_url = build_signed_ws_url(
        &config.tts_standard_endpoint,
        &config.api_key,
        &config.api_secret,
        &current_rfc1123_date(),
    )?;
    let (mut socket, _) = connect_async(signed_url)
        .await
        .context("connect standard tts websocket")?;
    socket
        .send(Message::Text(
            build_standard_tts_payload_for_profile(
                &config.app_id,
                &config.tts_standard_voice,
                text,
                profile,
            )?
            .to_string()
            .into(),
        ))
        .await
        .context("send standard tts payload")?;

    let mut progress = TtsStreamProgress::default();
    let mut packetizer = TimedStandardTtsPacketizer::new(profile)?;
    while let Some(message) = socket.next().await {
        let message = message.context("read standard tts websocket")?;
        let Message::Text(raw) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&raw).context("parse standard tts response")?;
        if let Some(is_last) =
            forward_standard_tts_audio_frame_timed(&value, &mut packetizer, &tx).await?
        {
            progress.observe(is_last);
            if is_last {
                break;
            }
        }
    }
    progress.ensure_complete()
}

async fn synthesize_super_smart_tts_chunks(config: &AppConfig, text: &str) -> Result<Vec<Vec<u8>>> {
    anyhow::ensure!(
        !config.api_key.trim().is_empty(),
        "XF_API_KEY is required for TTS"
    );
    anyhow::ensure!(
        !config.api_secret.trim().is_empty(),
        "XF_API_SECRET is required for TTS"
    );
    let signed_url = build_signed_ws_url(
        &config.tts_endpoint,
        &config.api_key,
        &config.api_secret,
        &current_rfc1123_date(),
    )?;
    let (mut socket, _) = connect_async(signed_url)
        .await
        .context("connect tts websocket")?;
    socket
        .send(Message::Text(
            build_tts_payload(&config.app_id, &config.tts_voice, text)
                .to_string()
                .into(),
        ))
        .await
        .context("send tts payload")?;

    let mut chunks = Vec::new();
    let mut progress = TtsStreamProgress::default();
    while let Some(message) = socket.next().await {
        let message = message.context("read tts websocket")?;
        let Message::Text(raw) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&raw).context("parse tts response")?;
        append_tts_log(&format_tts_response_summary("super_smart", &value));
        let Some(chunk) = parse_tts_audio_frame(&value)? else {
            continue;
        };
        let is_last = chunk.is_last;
        progress.observe(is_last);
        chunks.push(chunk.audio);
        if is_last {
            break;
        }
    }
    progress.ensure_complete()?;
    Ok(chunks)
}

async fn synthesize_standard_tts_chunks(config: &AppConfig, text: &str) -> Result<Vec<Vec<u8>>> {
    anyhow::ensure!(
        !config.api_key.trim().is_empty(),
        "XF_API_KEY is required for standard TTS"
    );
    anyhow::ensure!(
        !config.api_secret.trim().is_empty(),
        "XF_API_SECRET is required for standard TTS"
    );
    let signed_url = build_signed_ws_url(
        &config.tts_standard_endpoint,
        &config.api_key,
        &config.api_secret,
        &current_rfc1123_date(),
    )?;
    let (mut socket, _) = connect_async(signed_url)
        .await
        .context("connect standard tts websocket")?;
    socket
        .send(Message::Text(
            build_standard_tts_payload(&config.app_id, &config.tts_standard_voice, text)
                .to_string()
                .into(),
        ))
        .await
        .context("send standard tts payload")?;

    let mut chunks = Vec::new();
    let mut progress = TtsStreamProgress::default();
    while let Some(message) = socket.next().await {
        let message = message.context("read standard tts websocket")?;
        let Message::Text(raw) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&raw).context("parse standard tts response")?;
        let Some(chunk) = parse_standard_tts_audio_frame(&value)? else {
            continue;
        };
        let is_last = chunk.is_last;
        progress.observe(is_last);
        chunks.push(chunk.audio);
        if is_last {
            break;
        }
    }
    progress.ensure_complete()?;
    Ok(chunks)
}

fn format_tts_response_summary(provider: &str, value: &Value) -> String {
    let header_status = value
        .pointer("/header/status")
        .and_then(Value::as_i64)
        .or_else(|| value.pointer("/data/status").and_then(Value::as_i64))
        .unwrap_or(-1);
    let code = value
        .pointer("/header/code")
        .and_then(Value::as_i64)
        .or_else(|| value.pointer("/code").and_then(Value::as_i64))
        .unwrap_or(0);
    let audio_len = value
        .pointer("/payload/audio/audio")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/data/audio").and_then(Value::as_str))
        .map(str::len)
        .unwrap_or(0);
    format!("{provider} code={code} status={header_status} audio_base64_len={audio_len}")
}

fn append_tts_log(line: &str) {
    let _ = std::fs::create_dir_all("logs");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("logs/tts.log")
    {
        use std::io::Write;
        let _ = writeln!(file, "{} {line}", chrono::Utc::now().to_rfc3339());
    }
}
