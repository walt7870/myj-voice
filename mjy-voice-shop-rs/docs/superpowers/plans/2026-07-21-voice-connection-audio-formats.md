# Voice Connection Audio Formats Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add connection-level `in_format` and `out_format` parameters to both voice WebSockets, supporting native MP3 and PCM16k input/output with MP3 defaults.

**Architecture:** A focused `AudioFormat` domain type owns query parsing and physical metadata. Each WebSocket resolves `VoiceConnectionFormats` once and passes it explicitly through ASR, conversation, and TTS tasks; Xfyun receives native `lame` or `raw` requests, with no server-side transcoding.

**Tech Stack:** Rust, Axum WebSocket, Tokio, serde, Xfyun IAT/TTS WebSocket APIs, vanilla JavaScript, Python and C++ SDK demos.

---

### Task 1: Type-safe audio format model

**Files:**
- Create: `src/domain/audio.rs`
- Modify: `src/domain/mod.rs`
- Test: `tests/protocol_tests.rs`

- [ ] **Step 1: Write failing format-model tests**

Add imports and assertions for exact lowercase parsing, MP3 defaults, PCM metadata, and unsupported values:

```rust
use mjy_voice_shop_rs::domain::audio::{AudioFormat, VoiceConnectionFormats};

#[test]
fn voice_connection_formats_default_to_mp3() {
    let formats = VoiceConnectionFormats::from_query(None, None).unwrap();
    assert_eq!(formats.input, AudioFormat::Mp3);
    assert_eq!(formats.output, AudioFormat::Mp3);
}

#[test]
fn voice_connection_formats_accept_exact_supported_values() {
    let formats = VoiceConnectionFormats::from_query(Some("pcm16k"), Some("mp3")).unwrap();
    assert_eq!(formats.input, AudioFormat::Pcm16k);
    assert_eq!(formats.output, AudioFormat::Mp3);
    assert_eq!(AudioFormat::Pcm16k.iat_sample_rate(), 16_000);
    assert_eq!(AudioFormat::Pcm16k.tts_sample_rate("super_smart"), 16_000);
    assert_eq!(AudioFormat::Mp3.tts_sample_rate("super_smart"), 24_000);
    assert_eq!(AudioFormat::Mp3.tts_sample_rate("standard"), 16_000);
    assert_eq!(AudioFormat::Pcm16k.channels(), 1);
    assert_eq!(AudioFormat::Pcm16k.bit_depth(), Some(16));
}

#[test]
fn voice_connection_formats_reject_aliases_and_case_variants() {
    for value in ["PCM16K", "pcm", "wav", "audio/mpeg"] {
        assert_eq!(
            VoiceConnectionFormats::from_query(Some(value), None)
                .unwrap_err()
                .code(),
            "unsupported_audio_format"
        );
    }
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run: `cargo test voice_connection_formats -- --nocapture`

Expected: FAIL because `domain::audio` does not exist.

- [ ] **Step 3: Implement the minimal format model**

Create `AudioFormat`, `VoiceConnectionFormats`, and `AudioFormatError`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFormat { Mp3, Pcm16k }

impl AudioFormat {
    pub fn parse(value: Option<&str>) -> Result<Self, AudioFormatError> {
        match value.unwrap_or("mp3") {
            "mp3" => Ok(Self::Mp3),
            "pcm16k" => Ok(Self::Pcm16k),
            value => Err(AudioFormatError::unsupported(value)),
        }
    }
    pub fn as_str(self) -> &'static str { match self { Self::Mp3 => "mp3", Self::Pcm16k => "pcm16k" } }
    pub fn xfyun_encoding(self) -> &'static str { match self { Self::Mp3 => "lame", Self::Pcm16k => "raw" } }
    pub fn iat_sample_rate(self) -> u32 { 16_000 }
    pub fn tts_sample_rate(self, provider: &str) -> u32 {
        match (self, provider.trim()) {
            (Self::Mp3, "standard" | "online") => 16_000,
            (Self::Mp3, _) => 24_000,
            (Self::Pcm16k, _) => 16_000,
        }
    }
    pub fn channels(self) -> u8 { 1 }
    pub fn bit_depth(self) -> Option<u8> { matches!(self, Self::Pcm16k).then_some(16) }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VoiceConnectionFormats { pub input: AudioFormat, pub output: AudioFormat }

impl VoiceConnectionFormats {
    pub fn from_query(input: Option<&str>, output: Option<&str>) -> Result<Self, AudioFormatError> {
        Ok(Self { input: AudioFormat::parse(input)?, output: AudioFormat::parse(output)? })
    }
}
```

Export the module from `src/domain/mod.rs`.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test voice_connection_formats -- --nocapture`

Expected: 3 tests PASS.

- [ ] **Step 5: Commit the format model**

```bash
git add src/domain/audio.rs src/domain/mod.rs tests/protocol_tests.rs
git commit -m "功能：增加语音连接音频格式模型"
```

### Task 2: Native Xfyun input and output payloads

**Files:**
- Modify: `src/xfyun/iat.rs`
- Modify: `src/xfyun/tts.rs`
- Test: `tests/protocol_tests.rs`

- [ ] **Step 1: Write failing IAT/TTS payload tests**

Add tests that call new format-aware builders:

```rust
#[test]
fn builds_iat_payload_for_mp3_and_pcm16k() {
    let mp3 = build_iat_frame_for_format("app", IatFrameKind::First, &[1], AudioFormat::Mp3).unwrap();
    let pcm = build_iat_frame_for_format("app", IatFrameKind::First, &[1, 2], AudioFormat::Pcm16k).unwrap();
    assert_eq!(mp3["payload"]["audio"]["encoding"], "lame");
    assert_eq!(pcm["payload"]["audio"]["encoding"], "raw");
    assert_eq!(pcm["payload"]["audio"]["sample_rate"], 16000);
}

#[test]
fn builds_super_smart_tts_payload_for_mp3_and_pcm16k() {
    let mp3 = build_tts_payload_for_format("app", "voice", "你好", AudioFormat::Mp3);
    let pcm = build_tts_payload_for_format("app", "voice", "你好", AudioFormat::Pcm16k);
    assert_eq!(mp3["parameter"]["tts"]["audio"]["encoding"], "lame");
    assert_eq!(pcm["parameter"]["tts"]["audio"]["encoding"], "raw");
    assert_eq!(pcm["parameter"]["tts"]["audio"]["sample_rate"], 16000);
}

#[test]
fn builds_standard_tts_payload_for_mp3_and_pcm16k() {
    let mp3 = build_standard_tts_payload_for_format("app", "voice", "你好", AudioFormat::Mp3);
    let pcm = build_standard_tts_payload_for_format("app", "voice", "你好", AudioFormat::Pcm16k);
    assert_eq!(mp3["business"]["aue"], "lame");
    assert_eq!(pcm["business"]["aue"], "raw");
    assert_eq!(pcm["business"]["auf"], "audio/L16;rate=16000");
}
```

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test payload_for_ -- --nocapture`

Expected: FAIL because the format-aware builders do not exist.

- [ ] **Step 3: Add format-aware builders and stream arguments**

Add `AudioFormat` arguments to the new builders, `recognize_audio`, `stream_iat`, `stream_audio_chunks`, and `stream_super_smart_tts_text_frames`. Keep existing test helpers as thin wrappers only where old protocol tests require them. Native requests must use:

```rust
"encoding": format.xfyun_encoding(),
"sample_rate": format.iat_sample_rate(),
"channels": format.channels(),
"bit_depth": format.bit_depth().unwrap_or(16)
```

For TTS use `format.tts_sample_rate(config.tts_provider.as_str())`. Standard TTS uses `aue = format.xfyun_encoding()` and `auf = "audio/L16;rate=16000"`; super-smart MP3 retains 24000Hz while PCM16k requests 16000Hz.

- [ ] **Step 4: Run payload tests and full protocol tests**

Run: `cargo test --test protocol_tests -- --nocapture`

Expected: all protocol tests PASS.

- [ ] **Step 5: Commit native provider support**

```bash
git add src/xfyun/iat.rs src/xfyun/tts.rs tests/protocol_tests.rs
git commit -m "功能：支持讯飞原生 MP3 与 PCM16k 音频"
```

### Task 3: Resolve and propagate WebSocket connection formats

**Files:**
- Modify: `src/web/mod.rs`
- Test: `tests/app_tests.rs`

- [ ] **Step 1: Write failing API and event tests**

Extend device config expectations and event metadata:

```rust
assert_eq!(body["voice_ws"]["query"], json!(["device_id", "token", "in_format", "out_format"]));
assert_eq!(body["audio_formats"]["input"]["default"], "mp3");
assert_eq!(body["audio_formats"]["output"]["default"], "mp3");
assert_eq!(body["audio_formats"]["input"]["supported"], json!(["mp3", "pcm16k"]));

let event = StreamEvent::tts_audio_chunk("AQI=".into(), 0, true, AudioFormat::Pcm16k);
assert_eq!(event.payload["format"], "pcm16k");
assert_eq!(event.payload["sample_rate"], 16000);
assert_eq!(event.payload["channels"], 1);
assert_eq!(event.payload["bit_depth"], 16);
```

Add unit coverage for `resolve_voice_formats` returning MP3 defaults and `unsupported_audio_format` for invalid values.

- [ ] **Step 2: Run focused tests and verify RED**

Run:

```bash
cargo test --test app_tests device_config_describes_voice_stream_protocol_for_sdks -- --nocapture
cargo test tts_audio_chunk -- --nocapture
```

Expected: FAIL because query parameters, config capability data, and event metadata are missing.

- [ ] **Step 3: Parse formats before WebSocket upgrade**

Use a shared query model:

```rust
#[derive(Debug, Deserialize)]
struct VoiceWsQuery {
    device_id: Option<String>,
    token: Option<String>,
    in_format: Option<String>,
    out_format: Option<String>,
}
```

Both `chat_ws` and `device_voice_ws` call `VoiceConnectionFormats::from_query`. On failure return:

```rust
(StatusCode::BAD_REQUEST, Json(json!({
    "error": "unsupported_audio_format",
    "message": error.to_string()
}))).into_response()
```

- [ ] **Step 4: Thread formats through the complete voice path**

Change signatures so formats are explicit:

```rust
handle_ws(socket, state, device_id, formats)
run_live_asr_to_channel(..., formats)
run_turn_to_channel(..., formats.output)
run_turn(..., output_format, emit)
run_llm_with_streaming_super_smart_tts(..., output_format, ...)
send_tts_sentence_events(..., output_format, ...)
```

Use `formats.input` for streamed and segmented IAT. Use `formats.output` for every mock and real TTS event. `/api/chat/text` passes `AudioFormat::Mp3` because it has no voice connection.

PCM input must reject odd byte counts before IAT. PCM-only duration/RMS diagnostics run only for `Pcm16k`; MP3 diagnostics report bytes and format without fabricating PCM duration.

- [ ] **Step 5: Run application tests and verify GREEN**

Run: `cargo test --test app_tests -- --nocapture`

Expected: all application tests PASS.

- [ ] **Step 6: Commit WebSocket propagation**

```bash
git add src/web/mod.rs tests/app_tests.rs
git commit -m "功能：按连接控制语音输入输出格式"
```

### Task 4: Browser and SDK callers

**Files:**
- Modify: `static/app.js`
- Modify: `SDKs/python/device_client.py`
- Modify: `SDKs/python/run_text_demo.sh`
- Modify: `SDKs/python/run_pcm_demo.sh`
- Modify: `SDKs/cpp/device_client.cpp`
- Modify: `SDKs/cpp/run_text_demo.sh`
- Modify: `SDKs/cpp/run_pcm_demo.sh`
- Modify: `scripts/voice-integrity-check.mjs`
- Modify: `scripts/sdk-acceptance.mjs`

- [ ] **Step 1: Add failing source-integrity assertions**

Extend voice and SDK acceptance checks to require:

```javascript
assertCase("体验页显式声明音频格式", appSource.includes("in_format=pcm16k") && appSource.includes("out_format=mp3"));
assertCase("Python SDK 透传格式", pythonSource.includes('"in_format"') && pythonSource.includes('"out_format"'));
assertCase("C++ SDK 透传格式", cppSource.includes("in_format=") && cppSource.includes("out_format="));
```

- [ ] **Step 2: Start the service, run checks, and verify RED**

Run:

```bash
scripts/start-dev.sh
npm run voice:check
npm run sdk:check
```

Expected: FAIL on missing connection format parameters.

- [ ] **Step 3: Update the browser connection**

Build `/api/chat/voice` with explicit query parameters:

```javascript
state.ws = new WebSocket(wsUrl("/api/chat/voice?in_format=pcm16k&out_format=mp3"));
```

The browser continues to consume MP3 only; no PCM player is added.

- [ ] **Step 4: Update Python SDK and scripts**

Add `--in-format`, `--out-format`, validate against `{"mp3", "pcm16k"}`, include both in `ws_url`, and choose output/player behavior from `tts_audio_chunk.payload.format`. PCM playback uses `ffplay -f s16le -ar 16000 -ac 1 -i pipe:0`; MP3 retains `mpg123`/`ffplay`.

PCM demo scripts pass `--in-format pcm16k --out-format mp3`. Text demo scripts pass `--out-format mp3` and retain default MP3 input.

- [ ] **Step 5: Update C++ SDK and scripts**

Extend `Args` with:

```cpp
std::string in_format = "mp3";
std::string out_format = "mp3";
```

Parse `--in-format` and `--out-format`, append both URL-encoded query values during WebSocket handshake, and select save/play behavior from the event `format`. PCM demo scripts explicitly pass `pcm16k` input.

- [ ] **Step 6: Run browser and SDK checks and verify GREEN**

Run: `npm run voice:check` and `npm run sdk:check`

Expected: both commands PASS.

- [ ] **Step 7: Commit caller updates**

```bash
git add static/app.js SDKs scripts/voice-integrity-check.mjs scripts/sdk-acceptance.mjs
git commit -m "功能：客户端声明语音输入输出格式"
```

### Task 5: Documentation and full verification

**Files:**
- Modify: `docs/接口接入说明.md`
- Modify: `docs/规划迭代记录.md`
- Modify: `SDKs/README.md`
- Modify: `SDKs/python/README.md`
- Modify: `SDKs/cpp/README.md`

- [ ] **Step 1: Update protocol documentation**

Document `in_format`, `out_format`, MP3 defaults, exact `pcm16k` physical format, JSON/base64 framing, device config capability discovery, invalid-format HTTP 400 behavior, and example URLs for all four format combinations.

- [ ] **Step 2: Run formatting and full regression**

Run:

```bash
cargo fmt --all -- --check
cargo test
npm run voice:check
npm run sdk:check
git diff --check
```

Expected: all commands PASS with no formatting or whitespace errors.

- [ ] **Step 3: Run real-provider smoke when credentials are available**

Run the existing cloud smoke entrypoint after extending it to exercise MP3 and PCM16k output payloads:

```bash
npm run cloud:smoke
```

Expected: IAT and both TTS providers accept their native `lame` and `raw` requests. If credentials or network are unavailable, report the check as blocked rather than passed.

- [ ] **Step 4: Inspect final diff and commit documentation**

```bash
git diff -- src/xfyun src/web/mod.rs src/domain static/app.js SDKs scripts tests docs
git add docs/接口接入说明.md docs/规划迭代记录.md SDKs/README.md SDKs/python/README.md SDKs/cpp/README.md
git commit -m "文档：补充语音连接格式接入说明"
```
