# Native Compressed Audio Profiles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add connection-level input/output codec and sample-rate profiles with MP3/16kHz defaults, Xfyun-native Speex/Opus support where the selected provider supports it, and no server-side transcoding or packet aggregation.

**Architecture:** Replace the fixed `AudioFormat` model with an `AudioProfile` containing codec and sample rate. A provider capability module owns the direction-specific Xfyun matrix and protocol parameter mapping; WebSocket handlers validate profiles before upgrade and pass immutable profiles through IAT/TTS. Standard Xfyun IAT provides PCM/MP3/Speex input, standard TTS provides PCM/MP3/Opus/open-source Speex output, while private providers expose only verified profiles.

**Tech Stack:** Rust, Axum WebSocket, Tokio, serde, Xfyun AIGes and standard WebSocket APIs, vanilla JavaScript, Python and C++ device SDKs, Node contract checks.

---

### Task 1: Type-safe codec and sample-rate profiles

**Files:**
- Modify: `src/domain/audio.rs`
- Test: `tests/protocol_tests.rs`

- [ ] **Step 1: Replace fixed-format assertions with failing profile tests**

Add tests that establish exact protocol names, MP3/16kHz defaults, PCM metadata, and separate error codes:

```rust
use mjy_voice_shop_rs::domain::audio::{
    AudioFormat, AudioProfile, AudioSampleRate, VoiceConnectionAudio,
};

#[test]
fn voice_audio_defaults_to_mp3_16k_in_both_directions() {
    let audio = VoiceConnectionAudio::from_query(None, None, None, None).unwrap();
    assert_eq!(audio.input, AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000));
    assert_eq!(audio.output, AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000));
}

#[test]
fn voice_audio_accepts_independent_codec_and_rate_profiles() {
    let audio = VoiceConnectionAudio::from_query(
        Some("speex"), Some("8000"), Some("opus"), Some("16000"),
    ).unwrap();
    assert_eq!(audio.input.format, AudioFormat::Speex);
    assert_eq!(audio.input.sample_rate.hz(), 8_000);
    assert_eq!(audio.output.format, AudioFormat::Opus);
    assert_eq!(audio.output.sample_rate.hz(), 16_000);
    assert_eq!(AudioProfile::pcm(AudioSampleRate::Hz8000).bit_depth(), Some(16));
}

#[test]
fn voice_audio_rejects_unknown_format_rate_and_speex_24k() {
    assert_eq!(
        VoiceConnectionAudio::from_query(Some("wav"), None, None, None)
            .unwrap_err().code(),
        "unsupported_audio_format"
    );
    assert_eq!(
        VoiceConnectionAudio::from_query(None, Some("44100"), None, None)
            .unwrap_err().code(),
        "unsupported_audio_rate"
    );
    assert_eq!(
        VoiceConnectionAudio::from_query(Some("speex"), Some("24000"), None, None)
            .unwrap_err().code(),
        "unsupported_audio_rate"
    );
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo test voice_audio_ --test protocol_tests -- --nocapture`

Expected: FAIL because `AudioProfile`, `AudioSampleRate`, and `VoiceConnectionAudio` do not exist.

- [ ] **Step 3: Implement the profile model**

Add the protocol-level `Pcm` codec and profile types in `src/domain/audio.rs`. Keep `Pcm16k` and the existing legacy `parse` temporarily so the existing WebSocket continues to work during Tasks 1-4. New profiles use `parse_profile`, which never accepts the string `pcm16k`; Task 5 removes the legacy variant and parser after all callers use `AudioProfile`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AudioFormat { Mp3, Pcm, Opus, Speex, Pcm16k }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AudioSampleRate { Hz8000, Hz16000, Hz24000 }

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct AudioProfile {
    pub format: AudioFormat,
    pub sample_rate: AudioSampleRate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoiceConnectionAudio {
    pub input: AudioProfile,
    pub output: AudioProfile,
}

impl AudioFormat {
    pub fn parse_profile(value: Option<&str>) -> Result<Self, AudioProfileError> {
        match value.unwrap_or("mp3") {
            "mp3" => Ok(Self::Mp3),
            "pcm" => Ok(Self::Pcm),
            "opus" => Ok(Self::Opus),
            "speex" => Ok(Self::Speex),
            value => Err(AudioProfileError::unsupported_format(value)),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Pcm => "pcm",
            Self::Opus => "opus",
            Self::Speex => "speex",
            Self::Pcm16k => "pcm16k",
        }
    }
}

impl AudioSampleRate {
    pub fn parse(value: Option<&str>) -> Result<Self, AudioProfileError> {
        match value.unwrap_or("16000") {
            "8000" => Ok(Self::Hz8000),
            "16000" => Ok(Self::Hz16000),
            "24000" => Ok(Self::Hz24000),
            value => Err(AudioProfileError::unsupported_rate(value)),
        }
    }

    pub fn hz(self) -> u32 {
        match self { Self::Hz8000 => 8_000, Self::Hz16000 => 16_000, Self::Hz24000 => 24_000 }
    }
}

impl AudioProfile {
    pub const fn new(format: AudioFormat, sample_rate: AudioSampleRate) -> Self {
        Self { format, sample_rate }
    }

    pub const fn pcm(sample_rate: AudioSampleRate) -> Self {
        Self::new(AudioFormat::Pcm, sample_rate)
    }

    pub fn channels(self) -> u8 { 1 }
    pub fn bit_depth(self) -> Option<u8> {
        matches!(self.format, AudioFormat::Pcm | AudioFormat::Pcm16k).then_some(16)
    }
}

impl VoiceConnectionAudio {
    pub fn from_query(
        in_format: Option<&str>,
        in_rate: Option<&str>,
        out_format: Option<&str>,
        out_rate: Option<&str>,
    ) -> Result<Self, AudioProfileError> {
        fn profile(format: Option<&str>, rate: Option<&str>) -> Result<AudioProfile, AudioProfileError> {
            let profile = AudioProfile::new(AudioFormat::parse_profile(format)?, AudioSampleRate::parse(rate)?);
            if profile.format == AudioFormat::Speex && profile.sample_rate == AudioSampleRate::Hz24000 {
                return Err(AudioProfileError::unsupported_rate("24000"));
            }
            Ok(profile)
        }
        Ok(Self {
            input: profile(in_format, in_rate)?,
            output: profile(out_format, out_rate)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioProfileError {
    code: &'static str,
    value: String,
}

impl AudioProfileError {
    fn unsupported_format(value: &str) -> Self {
        Self { code: "unsupported_audio_format", value: value.to_string() }
    }
    fn unsupported_rate(value: &str) -> Self {
        Self { code: "unsupported_audio_rate", value: value.to_string() }
    }
    pub fn code(&self) -> &'static str { self.code }
}

impl std::fmt::Display for AudioProfileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.value)
    }
}

impl std::error::Error for AudioProfileError {}
```

The parser above rejects Speex/24kHz as `unsupported_audio_rate` and returns MP3/16kHz for all missing values.

- [ ] **Step 4: Run focused and protocol tests**

Run: `cargo test --test protocol_tests voice_audio_ -- --nocapture`

Expected: all `voice_audio_` tests PASS and existing callers continue to compile through the temporary internal `Pcm16k` variant.

- [ ] **Step 5: Commit the isolated domain model when its staged diff contains only audio changes**

```bash
git add src/domain/audio.rs tests/protocol_tests.rs
git diff --cached --check
git commit -m "功能：增加连接级音频档位模型"
```

### Task 2: Provider configuration and capability matrix

**Files:**
- Create: `src/xfyun/audio.rs`
- Modify: `src/xfyun/mod.rs`
- Modify: `src/config.rs`
- Test: `tests/protocol_tests.rs`

- [ ] **Step 1: Write failing direction-specific capability tests**

```rust
use mjy_voice_shop_rs::xfyun::audio::{
    iat_supports, tts_supports, IatProvider, TtsProvider,
};

#[test]
fn standard_iat_supports_speex_but_not_opus() {
    assert!(iat_supports(IatProvider::Standard, AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000)));
    assert!(iat_supports(IatProvider::Standard, AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000)));
    assert!(!iat_supports(IatProvider::Standard, AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000)));
    assert!(!iat_supports(IatProvider::Standard, AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000)));
}

#[test]
fn standard_tts_supports_open_codecs_at_8k_and_16k() {
    for format in [AudioFormat::Pcm, AudioFormat::Mp3, AudioFormat::Opus, AudioFormat::Speex] {
        assert!(tts_supports(TtsProvider::Standard, AudioProfile::new(format, AudioSampleRate::Hz8000)));
        assert!(tts_supports(TtsProvider::Standard, AudioProfile::new(format, AudioSampleRate::Hz16000)));
        assert!(!tts_supports(TtsProvider::Standard, AudioProfile::new(format, AudioSampleRate::Hz24000)));
    }
}

#[test]
fn private_providers_expose_only_verified_profiles() {
    assert!(iat_supports(IatProvider::SuperSmart, AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000)));
    assert!(iat_supports(IatProvider::SuperSmart, AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000)));
    assert!(!iat_supports(IatProvider::SuperSmart, AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000)));
    assert!(tts_supports(TtsProvider::SuperSmart, AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000)));
    assert!(!tts_supports(TtsProvider::SuperSmart, AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000)));
}
```

- [ ] **Step 2: Run the capability tests and verify RED**

Run: `cargo test supports_ --test protocol_tests -- --nocapture`

Expected: FAIL because the provider capability module does not exist.

- [ ] **Step 3: Implement provider parsing and immutable capability lists**

Create `src/xfyun/audio.rs` with `IatProvider::{SuperSmart, Standard}` and `TtsProvider::{SuperSmart, Standard}`. Implement these exact matrices:

```text
IAT super_smart: mp3/16000, pcm/16000
IAT standard:    mp3/{8000,16000}, pcm/{8000,16000}, speex/{8000,16000}
TTS super_smart: mp3/{8000,16000,24000}, pcm/16000
TTS standard:    mp3,pcm,opus,speex x {8000,16000}
```

Expose `supported_iat_profiles(provider)` and `supported_tts_profiles(provider)` from the same static lists used by `iat_supports` and `tts_supports`; do not duplicate the matrix in the web layer.

```rust
pub fn supported_iat_profiles(provider: IatProvider) -> &'static [AudioProfile] {
    match provider {
        IatProvider::SuperSmart => &[
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
        ],
        IatProvider::Standard => &[
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
        ],
    }
}

pub fn iat_supports(provider: IatProvider, profile: AudioProfile) -> bool {
    supported_iat_profiles(provider).contains(&profile)
}

pub fn supported_tts_profiles(provider: TtsProvider) -> &'static [AudioProfile] {
    match provider {
        TtsProvider::SuperSmart => &[
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz24000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
        ],
        TtsProvider::Standard => &[
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
        ],
    }
}

pub fn tts_supports(provider: TtsProvider, profile: AudioProfile) -> bool {
    supported_tts_profiles(provider).contains(&profile)
}

impl IatProvider {
    pub fn parse(value: &str) -> Result<Self, AudioProviderError> {
        match value.trim() {
            "super_smart" => Ok(Self::SuperSmart),
            "standard" => Ok(Self::Standard),
            value => Err(AudioProviderError::unsupported("IAT", value)),
        }
    }
}

impl TtsProvider {
    pub fn parse(value: &str) -> Result<Self, AudioProviderError> {
        match value.trim() {
            "super_smart" => Ok(Self::SuperSmart),
            "standard" | "online" => Ok(Self::Standard),
            value => Err(AudioProviderError::unsupported("TTS", value)),
        }
    }
}

#[derive(Debug)]
pub struct AudioProviderError(String);

impl AudioProviderError {
    fn unsupported(direction: &str, value: &str) -> Self {
        Self(format!("unsupported {direction} provider: {value}"))
    }
}

impl std::fmt::Display for AudioProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AudioProviderError {}
```

Surface `AudioProviderError` as a configuration error. This prevents an unknown provider from silently selecting a matrix.

- [ ] **Step 4: Add backward-readable provider configuration**

Add `iat_provider` to `AppConfig` and `PublicAppConfig` with a serde default and environment override:

```rust
#[serde(default = "default_iat_provider")]
pub iat_provider: String,

fn default_iat_provider() -> String { "super_smart".to_string() }
```

`AppConfig::default_from_env` reads `XF_IAT_PROVIDER`; old stored JSON without this field must deserialize as `super_smart`. Keep the existing `XF_IAT_ENDPOINT` so provider selection does not overwrite deployment-specific endpoints.

- [ ] **Step 5: Run configuration and capability tests**

Run: `cargo test --test protocol_tests -- --nocapture`

Expected: capability tests and existing config backward-compatibility tests PASS.

- [ ] **Step 6: Commit the capability layer**

```bash
git add src/xfyun/audio.rs src/xfyun/mod.rs src/config.rs tests/protocol_tests.rs
git diff --cached --check
git commit -m "功能：增加讯飞音频能力矩阵"
```

### Task 3: Provider-aware IAT payloads and native Speex input

**Files:**
- Modify: `src/xfyun/iat.rs`
- Modify: `src/web/mod.rs`
- Test: `tests/protocol_tests.rs`

- [ ] **Step 1: Write failing payload tests for sample rates and Speex quality 7**

```rust
#[test]
fn builds_standard_iat_speex_frames_with_open_source_frame_sizes() {
    let nb = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000);
    let wb = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);
    let nb_frame = build_standard_iat_frame("app", IatFrameKind::First, &[1; 38], nb).unwrap();
    let wb_frame = build_standard_iat_frame("app", IatFrameKind::First, &[1; 60], wb).unwrap();
    assert_eq!(nb_frame["data"]["encoding"], "speex");
    assert_eq!(nb_frame["business"]["speex_size"], 38);
    assert_eq!(wb_frame["data"]["encoding"], "speex-wb");
    assert_eq!(wb_frame["business"]["speex_size"], 60);
    assert_eq!(wb_frame["data"]["format"], "audio/L16;rate=16000");
}

#[test]
fn rejects_wrong_speex_packet_size_without_buffering() {
    let profile = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);
    let error = validate_input_packet(profile, &[0; 59]).unwrap_err();
    assert_eq!(error.code(), "invalid_audio_packet");
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo test iat_speex --test protocol_tests -- --nocapture`

Expected: FAIL because standard IAT payload and packet validation functions do not exist.

- [ ] **Step 3: Add exact IAT encoding mapping**

Implement provider-aware payload builders:

```rust
fn iat_encoding(profile: AudioProfile) -> &'static str {
    match (profile.format, profile.sample_rate) {
        (AudioFormat::Pcm, _) => "raw",
        (AudioFormat::Mp3, _) => "lame",
        (AudioFormat::Speex, AudioSampleRate::Hz8000) => "speex",
        (AudioFormat::Speex, AudioSampleRate::Hz16000) => "speex-wb",
        _ => unreachable!("profile must pass provider capability validation"),
    }
}

fn speex_size(profile: AudioProfile) -> Option<usize> {
    match (profile.format, profile.sample_rate) {
        (AudioFormat::Speex, AudioSampleRate::Hz8000) => Some(38),
        (AudioFormat::Speex, AudioSampleRate::Hz16000) => Some(60),
        _ => None,
    }
}
```

For standard IAT, the first frame contains `common`, `business`, and `data`; continuation/final frames contain `data`. Set `data.format` to `audio/L16;rate=<hz>`, set `data.encoding` from `iat_encoding`, and add `business.speex_size` only for open-source Speex. Keep the existing AIGes `header/parameter/payload` builder for `super_smart`.

- [ ] **Step 4: Validate packets without decoding or aggregation**

`validate_input_packet` must reject empty payloads, PCM odd-byte payloads, payloads above the existing WebSocket event limit, and Speex packets whose size is not exactly 38 bytes at 8kHz or 60 bytes at 16kHz. MP3 and PCM remain continuous chunks. The function must return immediately and must not retain data between calls.

- [ ] **Step 5: Select IAT protocol in both segment and live-stream paths**

Pass `AudioProfile` and parsed `IatProvider` to `recognize_audio`, `build_iat_segment_frames`, and the live writer in `run_live_asr_to_channel`. Add a standard-response parser for `code`, `data.result`, and `data.status` while retaining the current AIGes `header/payload` parser. Map an upstream codec/rate rejection to `upstream_audio_profile_rejected`; retain `asr_failed` for recognition and network failures.

- [ ] **Step 6: Run IAT and WebSocket tests**

Run: `cargo test iat -- --nocapture`

Expected: private IAT regression tests, standard PCM/MP3/Speex payload tests, packet validation tests, and live stream tests PASS.

- [ ] **Step 7: Commit only IAT-specific files if no unrelated web hunks would be staged**

```bash
git add src/xfyun/iat.rs tests/protocol_tests.rs
git diff --cached --check
git commit -m "功能：支持讯飞标准 IAT 原生 Speex"
```

Leave `src/web/mod.rs` unstaged at this point because it already contains unrelated user-owned changes; include it only in the final reviewed integration commit.

### Task 4: Native TTS rate, Opus, and open-source Speex mappings

**Files:**
- Modify: `src/xfyun/tts.rs`
- Test: `tests/protocol_tests.rs`

- [ ] **Step 1: Write failing standard TTS mapping tests**

```rust
#[test]
fn standard_tts_maps_native_codecs_and_rates() {
    let cases = [
        (AudioFormat::Mp3, AudioSampleRate::Hz16000, "lame"),
        (AudioFormat::Pcm, AudioSampleRate::Hz8000, "raw"),
        (AudioFormat::Opus, AudioSampleRate::Hz8000, "opus"),
        (AudioFormat::Opus, AudioSampleRate::Hz16000, "opus-wb"),
        (AudioFormat::Speex, AudioSampleRate::Hz8000, "speex-org-nb;7"),
        (AudioFormat::Speex, AudioSampleRate::Hz16000, "speex-org-wb;7"),
    ];
    for (format, rate, aue) in cases {
        let profile = AudioProfile::new(format, rate);
        let payload = build_standard_tts_payload_for_profile("app", "voice", "你好", profile);
        assert_eq!(payload["business"]["aue"], aue);
        assert_eq!(payload["business"]["auf"], format!("audio/L16;rate={}", rate.hz()));
    }
}

#[test]
fn mp3_streaming_flag_and_16k_default_are_explicit() {
    let profile = AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000);
    let standard = build_standard_tts_payload_for_profile("app", "voice", "你好", profile);
    let smart = build_tts_payload_for_profile("app", "voice", "你好", profile);
    assert_eq!(standard["business"]["sfl"], 1);
    assert_eq!(smart["parameter"]["tts"]["audio"]["sample_rate"], 16000);
}
```

- [ ] **Step 2: Run the TTS mapping tests and verify RED**

Run: `cargo test standard_tts_maps --test protocol_tests -- --nocapture`

Expected: FAIL because the profile-aware mappings do not exist.

- [ ] **Step 3: Implement exact standard TTS parameters**

Replace provider-derived sample rates with the connection profile. Standard TTS uses:

```text
pcm         -> aue=raw
mp3         -> aue=lame, sfl=1
opus/8k     -> aue=opus
opus/16k    -> aue=opus-wb
speex/8k    -> aue=speex-org-nb;7
speex/16k   -> aue=speex-org-wb;7
all         -> auf=audio/L16;rate=<profile hz>
```

Super-smart TTS receives `encoding`, `sample_rate`, `channels=1`, and `bit_depth=16` from the validated profile. Its capability matrix prevents Opus/Speex from reaching this builder until those private endpoint combinations have been separately verified.

- [ ] **Step 4: Preserve upstream chunk bytes and boundaries**

Change `stream_audio_chunks` and `stream_super_smart_tts_text_frames_for_profile` to take `AudioProfile`. Forward each decoded Xfyun response audio field as one `TtsAudioChunk` without decode, resample, re-encode, Ogg wrapping, or time-based buffering. Do not enable standard Opus/Speex profiles unless a real-account smoke test confirms each response audio field is a complete device-decodable packet boundary.

- [ ] **Step 5: Run all TTS protocol tests**

Run: `cargo test tts -- --nocapture`

Expected: existing streaming and parser tests plus PCM/MP3/Opus/Speex mapping tests PASS.

- [ ] **Step 6: Commit the provider TTS changes**

```bash
git add src/xfyun/tts.rs tests/protocol_tests.rs
git diff --cached --check
git commit -m "功能：支持讯飞原生压缩 TTS 档位"
```

### Task 5: WebSocket validation, capability discovery, and event metadata

**Files:**
- Modify: `src/web/mod.rs`
- Test: `src/web/mod.rs`
- Test: `tests/app_tests.rs`

- [ ] **Step 1: Write failing query, rejection, and metadata tests**

Add focused tests for:

```rust
let audio = resolve_voice_audio(
    Some("speex"), Some("8000"), Some("opus"), Some("16000"),
    IatProvider::Standard, TtsProvider::Standard,
).unwrap();
assert_eq!(audio.input.sample_rate.hz(), 8000);
assert_eq!(audio.output.format, AudioFormat::Opus);

let error = resolve_voice_audio(
    Some("opus"), Some("16000"), None, None,
    IatProvider::Standard, TtsProvider::Standard,
).unwrap_err();
assert_eq!(error.code(), "unsupported_audio_profile");

let event = StreamEvent::tts_audio_chunk("AAE=".into(), 3, true,
    AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000));
assert_eq!(event.payload["format"], "opus");
assert_eq!(event.payload["sample_rate"], 16000);
assert!(event.payload.get("bit_depth").is_none());
```

Extend the device config test to require all four query names and provider-filtered `audio_profiles.input.supported` / `audio_profiles.output.supported` arrays.

- [ ] **Step 2: Run focused web tests and verify RED**

Run:

```bash
cargo test voice_audio -- --nocapture
cargo test device_config_describes_voice_stream_protocol_for_sdks --test app_tests -- --nocapture
```

Expected: FAIL because rate parameters, profile validation, and dynamic capability output are missing.

- [ ] **Step 3: Parse and reject profiles before WebSocket upgrade**

Expand the shared query model:

```rust
#[derive(Debug, Deserialize)]
struct VoiceWsQuery {
    device_id: Option<String>,
    token: Option<String>,
    in_format: Option<String>,
    in_rate: Option<String>,
    out_format: Option<String>,
    out_rate: Option<String>,
}
```

Both `/api/device/voice` and `/api/chat/voice` load the current provider configuration, parse `VoiceConnectionAudio`, then check the direction-specific capability matrix before calling `ws.on_upgrade`. Return HTTP 400 JSON with `unsupported_audio_format`, `unsupported_audio_rate`, or `unsupported_audio_profile` as appropriate.

- [ ] **Step 4: Propagate immutable profiles and validate each input chunk**

Replace `VoiceConnectionFormats` and `AudioFormat` parameters throughout `handle_ws`, `run_live_asr_to_channel`, `handle_interrupt_audio_segment`, `run_turn_to_channel`, and `run_turn`. Call `validate_input_packet` before sending each decoded chunk to the live IAT channel. Use the profile sample rate in PCM duration calculations instead of the current hard-coded 16000.

After all callers have migrated, remove the temporary `AudioFormat::Pcm16k` variant and legacy parser, then rename `parse_profile` to `parse`. Reject `audio_segment` for Opus/Speex with `invalid_audio_packet`; packetized codecs use `audio_stream_start/chunk/end`, with exactly one codec packet per chunk.

- [ ] **Step 5: Generate capability discovery from the provider matrix**

Change `device_config` to accept `State<AppState>`, load `AppConfig`, and serialize the same `supported_iat_profiles` / `supported_tts_profiles` values used by handshake validation. The default for both directions is `{ "format": "mp3", "sample_rate": 16000 }`; query order is `device_id`, `token`, `in_format`, `in_rate`, `out_format`, `out_rate`.

- [ ] **Step 6: Emit profile-accurate TTS metadata and relay timing diagnostics**

`StreamEvent::tts_audio_chunk` takes `AudioProfile` directly and emits `format`, `sample_rate`, `channels`, and PCM-only `bit_depth`. Record monotonic elapsed time immediately around upstream/client send calls under the diagnostic event names `voice_audio_uplink_relay_duration` and `voice_audio_downlink_relay_duration`, tagged with format, sample rate, and provider; do not add a queue or background task solely for metrics.

- [ ] **Step 7: Run app and protocol regressions**

Run: `cargo test --tests -- --nocapture`

Expected: all Rust tests PASS, including default MP3/16kHz, cross-provider rejection, concurrent connection isolation, and byte-preserving relay tests.

- [ ] **Step 8: Review the complete web diff before staging**

Run:

```bash
git diff -- src/web/mod.rs tests/app_tests.rs
git diff --check
```

Because `src/web/mod.rs` contains existing refund and interaction changes, do not commit it separately unless the staged diff can be proven to contain no unrelated user-owned work.

### Task 6: Browser, SDKs, contract checks, and integration documentation

**Files:**
- Modify: `static/app.js`
- Modify: `SDKs/python/device_client.py`
- Modify: `SDKs/python/run_pcm_demo.sh`
- Modify: `SDKs/python/run_text_demo.sh`
- Modify: `SDKs/python/README.md`
- Modify: `SDKs/cpp/device_client.cpp`
- Modify: `SDKs/cpp/run_pcm_demo.sh`
- Modify: `SDKs/cpp/run_text_demo.sh`
- Modify: `SDKs/cpp/README.md`
- Modify: `SDKs/README.md`
- Modify: `scripts/audio-format-contract-check.mjs`
- Modify: `package.json`
- Modify: `docs/接口接入说明.md`
- Modify: `docs/voice-integrity-test-cases.md`

- [ ] **Step 1: Make the contract check fail on the old two-parameter clients**

Require all clients to contain `in_format`, `in_rate`, `out_format`, and `out_rate`; require the browser URL to be `in_format=pcm&in_rate=16000&out_format=mp3&out_rate=16000`; reject remaining protocol uses of `pcm16k` in SDK code.

Add `"audio:check": "node scripts/audio-format-contract-check.mjs"` to `package.json` and run `npm run audio:check`.

Expected: FAIL because the rate parameters are not yet present.

- [ ] **Step 2: Update browser and Python SDK connection parameters**

The Python URL builder becomes:

```python
query = urllib.parse.urlencode({
    "device_id": device_id,
    "token": token,
    "in_format": in_format,
    "in_rate": in_rate,
    "out_format": out_format,
    "out_rate": out_rate,
})
```

Add `--in-rate` and `--out-rate` choices `(8000, 16000, 24000)` with defaults `16000`; format choices are `mp3`, `pcm`, `opus`, `speex`. PCM playback uses the selected `out_rate`; Opus/Speex playback examples use `ffplay` only when the received packet representation has passed the real-device compatibility smoke test.

Replace the PCM-specific upload argument with `--audio`: PCM files are split by the selected sample rate, Speex files are read as fixed 38-byte/60-byte quality-7 frames, and MP3 remains a continuous byte stream. The SDK does not encode PCM into Speex/Opus and must state that packetized input files are already encoded by the device codec.

- [ ] **Step 3: Update C++ SDK connection and fixed-frame streaming**

Add `int in_rate = 16000` and `int out_rate = 16000` to `Args`, include both query values in the handshake, and validate the same codec/rate values as the server. Rename `--pcm` to `--audio`; calculate PCM 40ms bytes as `in_rate * 2 * 40 / 1000`, and read already-encoded Speex files as one quality-7 frame per `audio_stream_chunk` with 38 bytes at 8kHz or 60 bytes at 16kHz. Do not add a host-side codec library or imply that the demo encodes PCM.

- [ ] **Step 4: Rewrite integration documentation around profiles**

Update `docs/接口接入说明.md` and SDK READMEs so every example uses the four parameters and defaults to MP3/16kHz. Document the direction matrix, PCM S16LE mono requirements, Speex quality 7 frame sizes, Opus output-only limitation for standard IAT, event metadata, error codes, capability-first client flow, AC7911BA 16kHz recommendation, and the no-transcode/no-buffer contract.

- [ ] **Step 5: Run SDK and browser contract checks**

Run:

```bash
npm run audio:check
npm run sdk:check
npm run voice:check
```

Expected: all checks PASS and no SDK protocol reference still uses `pcm16k`.

- [ ] **Step 6: Review and commit only audio-specific client/documentation changes**

```bash
git diff --check
git add SDKs scripts/audio-format-contract-check.mjs package.json docs/接口接入说明.md docs/voice-integrity-test-cases.md
git diff --cached --check
```

Do not stage unrelated existing static/admin or documentation changes. Commit the reviewed subset as `文档：更新多档位音频接入说明`.

### Task 7: Full verification, real Xfyun gate, and JD deployment

**Files:**
- Modify: `src/bin/cloud_smoke.rs`
- Modify: `scripts/cloud-smoke.sh`
- Modify: `docs/规划迭代记录.md`

- [ ] **Step 1: Add cloud smoke assertions for defaults and rejection behavior**

The smoke client must assert that `/api/device/config` reports MP3/16kHz defaults, that an unsupported input `opus/16000` profile receives HTTP 400 before upgrade under standard IAT, and that the normal text path still returns `tts_audio_chunk` metadata matching the selected output profile.

- [ ] **Step 2: Run the complete local verification gate**

Run:

```bash
cargo fmt --check
cargo test
npm run audio:check
npm run sdk:check
npm run voice:check
npm run ui:check
git diff --check
```

Expected: every command exits 0.

- [ ] **Step 3: Verify native codec packet compatibility with real Xfyun credentials**

Using the configured account, request each candidate profile separately and record pass/fail by provider, direction, codec, and rate. A profile passes only when bytes can be consumed directly by the matching standard decoder and packet boundaries remain one response audio field per device packet. Disable every failed or unverified combination in the deployed capability matrix; do not transcode or silently downgrade it.

- [ ] **Step 4: Measure the server relay overhead**

Send at least 500 packets for MP3 baseline and each enabled Speex/Opus profile. Compare the diagnostic events from receipt to upstream send and upstream receipt to client send. Both relay directions must show no multi-packet accumulation and P95 overhead no more than 2ms above the MP3 baseline.

- [ ] **Step 5: Deploy with the repository deployment workflow**

Before deployment, invoke the `safe-deploy-release` skill. Then run:

```bash
scripts/deploy-jd.sh
BASE_URL=https://www.niuwancheng.cn/mjy-voice-shop npm run cloud:smoke
curl -k -fsS https://www.niuwancheng.cn/mjy-voice-shop/api/health
curl -k -fsS https://www.niuwancheng.cn/mjy-voice-shop/api/device/config
```

Expected: systemd reports `mjy-voice-shop-rs.service` active, health returns `ok`, capability output matches the deployed provider matrix, and public smoke passes.

- [ ] **Step 6: Record verified profiles and deployment evidence**

Append the deployed commit, provider names, enabled input/output profiles, test command results, P95 relay measurements, service status, health response, and rollback reference to `docs/规划迭代记录.md`. Do not record credentials, signed URLs, audio bodies, or tokens.
