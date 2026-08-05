use std::{process::Command, str::FromStr, time::Duration};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use mjy_voice_shop_rs::{
    config::AppConfig,
    db,
    xfyun::{
        iat::recognize_pcm,
        llm::{stream_chat_chunks, ChatMessage},
        tts::synthesize_mp3_chunks,
    },
};
use serde_json::{json, Value};
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use tokio::{sync::mpsc, time::timeout};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Error as WsError, Message},
};
use url::Url;
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    if let Ok(base_url) = std::env::var("BASE_URL") {
        return run_public_smoke(&base_url).await;
    }

    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://mjy_voice_shop.db".to_string());
    let options = SqliteConnectOptions::from_str(&database_url)?.create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await?;
    db::init(&pool).await?;
    let mut config = db::get_config(&pool).await?;
    config.mock_providers = false;

    let mut results = Vec::new();
    results.push(run_case("IAT 中英听写", test_iat(&config)).await);
    results.push(run_case("大模型推理", test_llm(&config)).await);
    results.push(run_case("超拟人合成", test_tts(&config, "super_smart")).await);
    results.push(run_case("在线语音合成", test_standard_tts(&config)).await);

    println!("云服务单能力测试结果");
    println!("====================");
    for result in &results {
        println!(
            "[{}] {} - {}",
            if result.ok { "OK" } else { "FAIL" },
            result.name,
            result.detail
        );
    }
    ensure_all_cases_pass(&results)
}

struct SmokeResult {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn ensure_all_cases_pass(results: &[SmokeResult]) -> Result<()> {
    let failed = results.iter().filter(|result| !result.ok).count();
    anyhow::ensure!(failed == 0, "{failed} cloud smoke case(s) failed");
    Ok(())
}

async fn run_public_smoke(base_url: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .context("build public smoke HTTP client")?;
    let config = fetch_public_device_config(&client, base_url).await?;
    let output_profile = validate_public_audio_config(&config)?;
    validate_public_config_no_credentials(&config)?;

    let results = vec![
        run_case("公网健康检查", test_public_health(&client, base_url)).await,
        run_case(
            "公网匿名管理接口门禁",
            test_public_internal_admin_gate(&client, base_url),
        )
        .await,
        run_case(
            "公网演示设备鉴权拒绝",
            test_public_demo_device_auth_rejected(&client, base_url),
        )
        .await,
        run_case("公网配置默认档位", async {
            Ok("default input/output mp3/16000; opus input unavailable".to_string())
        })
        .await,
        run_case(
            "公网 Opus input 握手前拒绝",
            test_public_unsupported_opus_input(base_url),
        )
        .await,
        run_case(
            "公网 Chat WebSocket 文本语音链路",
            test_public_text_voice(base_url, &output_profile.0, output_profile.1),
        )
        .await,
    ];

    println!("公网语音链路测试结果");
    println!("====================");
    for result in &results {
        println!(
            "[{}] {} - {}",
            if result.ok { "OK" } else { "FAIL" },
            result.name,
            result.detail
        );
    }
    ensure_all_cases_pass(&results)
}

async fn test_public_health(client: &reqwest::Client, base_url: &str) -> Result<String> {
    let response = client
        .get(api_url(base_url, "/api/health"))
        .send()
        .await
        .context("GET public health")?;
    anyhow::ensure!(
        response.status().is_success(),
        "public health returned HTTP {}",
        response.status()
    );
    Ok(format!("HTTP {}", response.status()))
}

async fn test_public_internal_admin_gate(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<String> {
    let response = client
        .get(api_url(base_url, "/api/admin/config"))
        .send()
        .await
        .context("GET anonymous public admin config")?;
    validate_internal_admin_status(response.status())?;
    Ok("anonymous GET /api/admin/config -> HTTP 401".to_string())
}

fn validate_internal_admin_status(status: reqwest::StatusCode) -> Result<()> {
    anyhow::ensure!(
        status == reqwest::StatusCode::UNAUTHORIZED,
        "anonymous public admin config returned HTTP {status} instead of 401"
    );
    Ok(())
}

async fn fetch_public_device_config(client: &reqwest::Client, base_url: &str) -> Result<Value> {
    client
        .get(api_url(base_url, "/api/device/config"))
        .send()
        .await
        .context("GET public device config")?
        .error_for_status()
        .context("public device config status")?
        .json()
        .await
        .context("decode public device config")
}

async fn test_public_demo_device_auth_rejected(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<String> {
    let response = client
        .post(api_url(base_url, "/api/device/auth"))
        .json(&json!({"device_id": "DOLL-0001", "device_secret": "demo-secret"}))
        .send()
        .await
        .context("POST public demo device auth")?;
    validate_public_demo_auth_status(response.status())?;
    Ok(format!("HTTP {}", response.status()))
}

fn validate_public_demo_auth_status(status: reqwest::StatusCode) -> Result<()> {
    anyhow::ensure!(
        matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ),
        "public demo device auth returned HTTP {status} instead of 401/403"
    );
    Ok(())
}

fn validate_public_audio_config(config: &Value) -> Result<(String, u32)> {
    for direction in ["input", "output"] {
        let default = config
            .pointer(&format!("/audio_profiles/{direction}/default"))
            .context("missing audio profile default")?;
        anyhow::ensure!(
            default["format"] == "mp3",
            "{direction} default format is not mp3"
        );
        anyhow::ensure!(
            default["sample_rate"] == 16_000,
            "{direction} default sample rate is not 16000"
        );
    }
    let opus_input = config
        .pointer("/audio_profiles/input/supported")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|profile| profile["format"] == "opus");
    anyhow::ensure!(
        !opus_input,
        "current provider unexpectedly advertises opus input"
    );
    Ok(("mp3".to_string(), 16_000))
}

fn validate_public_config_no_credentials(config: &Value) -> Result<()> {
    const DEVICE_ID_PLACEHOLDER: &str = "<configured-device-id>";
    const DEVICE_SECRET_PLACEHOLDER: &str = "<provisioned-device-secret>";

    let device_id = config
        .pointer("/auth/request/device_id")
        .and_then(Value::as_str)
        .context("public device config is missing auth.request.device_id placeholder")?;
    anyhow::ensure!(
        device_id == DEVICE_ID_PLACEHOLDER,
        "public device config exposes a non-placeholder device_id"
    );
    let device_secret = config
        .pointer("/auth/request/device_secret")
        .and_then(Value::as_str)
        .context("public device config is missing auth.request.device_secret placeholder")?;
    anyhow::ensure!(
        device_secret == DEVICE_SECRET_PLACEHOLDER,
        "public device config exposes a non-placeholder device_secret"
    );

    fn contains_seeded_credential(value: &Value) -> bool {
        match value {
            Value::String(value) => value.contains("DOLL-0001") || value.contains("demo-secret"),
            Value::Array(values) => values.iter().any(contains_seeded_credential),
            Value::Object(values) => values.values().any(contains_seeded_credential),
            _ => false,
        }
    }

    anyhow::ensure!(
        !contains_seeded_credential(config),
        "public device config exposes seeded device credential values"
    );
    Ok(())
}

fn api_url(base_url: &str, path: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), path)
}

fn build_public_chat_voice_ws_url(
    base_url: &str,
    input_format: &str,
    input_rate: u32,
    output_format: &str,
    output_rate: u32,
) -> Result<Url> {
    let mut url = Url::parse(base_url).context("parse public base URL")?;
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|_| anyhow::anyhow!("set wss scheme"))?,
        "http" => url
            .set_scheme("ws")
            .map_err(|_| anyhow::anyhow!("set ws scheme"))?,
        scheme => anyhow::bail!("unsupported public base URL scheme: {scheme}"),
    }
    let path = format!("{}/api/chat/voice", url.path().trim_end_matches('/'));
    url.set_path(&path);
    url.set_query(None);
    url.query_pairs_mut()
        .append_pair("in_format", input_format)
        .append_pair("in_rate", &input_rate.to_string())
        .append_pair("out_format", output_format)
        .append_pair("out_rate", &output_rate.to_string());
    Ok(url)
}

async fn test_public_unsupported_opus_input(base_url: &str) -> Result<String> {
    let url = build_public_chat_voice_ws_url(base_url, "opus", 16_000, "mp3", 16_000)?;
    match connect_async(url.as_str()).await {
        Err(WsError::Http(response)) => {
            anyhow::ensure!(
                response.status().as_u16() == 400,
                "unsupported opus input returned HTTP {} instead of 400",
                response.status()
            );
            Ok("HTTP 400 before WebSocket upgrade".to_string())
        }
        Err(error) => Err(error).context("unsupported opus input handshake failed unexpectedly"),
        Ok(_) => anyhow::bail!("unsupported opus input unexpectedly upgraded to WebSocket"),
    }
}

async fn test_public_text_voice(
    base_url: &str,
    output_format: &str,
    output_rate: u32,
) -> Result<String> {
    let url = build_public_chat_voice_ws_url(base_url, "mp3", 16_000, output_format, output_rate)?;
    let (mut socket, _) = connect_async(url.as_str())
        .await
        .context("upgrade public chat voice WebSocket")?;
    socket
        .send(Message::Text(
            json!({
                "type": "text",
                "conversation_id": format!("cloud-smoke-{}", Uuid::new_v4()),
                "text": "公网验收，请简短回复收到"
            })
            .to_string()
            .into(),
        ))
        .await
        .context("send public text event")?;

    let mut audio_chunks = 0usize;
    while let Some(message) = socket.next().await {
        let Message::Text(raw) = message.context("read public voice WebSocket")? else {
            continue;
        };
        let event: Value = serde_json::from_str(&raw).context("decode public voice event")?;
        match event.get("event_type").and_then(Value::as_str) {
            Some("tts_audio_chunk") => {
                let payload = &event["payload"];
                anyhow::ensure!(
                    payload["format"] == output_format,
                    "TTS format metadata mismatch"
                );
                anyhow::ensure!(
                    payload["sample_rate"] == output_rate,
                    "TTS rate metadata mismatch"
                );
                anyhow::ensure!(payload["channels"] == 1, "TTS channel metadata mismatch");
                anyhow::ensure!(
                    payload["audio"]
                        .as_str()
                        .is_some_and(|audio| !audio.is_empty()),
                    "TTS audio chunk is empty"
                );
                audio_chunks += 1;
            }
            Some("error") => anyhow::bail!(
                "public voice returned error kind {}",
                event
                    .pointer("/payload/kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
            ),
            Some("voice_done") => {
                anyhow::ensure!(audio_chunks > 0, "voice_done arrived before any TTS audio");
                return Ok(format!(
                    "profile={output_format}/{output_rate}/mono, chunks={audio_chunks}, voice_done"
                ));
            }
            _ => {}
        }
    }
    anyhow::bail!("public voice WebSocket closed before voice_done")
}

async fn run_case<F>(name: &'static str, future: F) -> SmokeResult
where
    F: std::future::Future<Output = Result<String>>,
{
    match timeout(Duration::from_secs(60), future).await {
        Ok(Ok(detail)) => SmokeResult {
            name,
            ok: true,
            detail,
        },
        Ok(Err(error)) => SmokeResult {
            name,
            ok: false,
            detail: error.to_string(),
        },
        Err(_) => SmokeResult {
            name,
            ok: false,
            detail: "timeout after 60s".to_string(),
        },
    }
}

async fn test_iat(config: &AppConfig) -> Result<String> {
    let pcm = generate_iat_pcm().context("generate iat pcm sample")?;
    let text = recognize_pcm(config, &pcm).await?;
    anyhow::ensure!(
        text.chars()
            .any(|ch| ch.is_ascii_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&ch)),
        "IAT returned non-semantic text: {text}"
    );
    Ok(format!("recognized: {text}"))
}

async fn test_llm(config: &AppConfig) -> Result<String> {
    let (tx, mut rx) = mpsc::channel(32);
    let messages = vec![
        ChatMessage::system("你是语音购物助手，回复一句短话。"),
        ChatMessage::user("测试连通性，请回复收到。"),
    ];
    tokio::spawn(stream_chat_chunks(config.clone(), messages, tx));

    let mut content = String::new();
    while let Some(chunk) = rx.recv().await {
        let chunk = chunk?;
        content.push_str(&chunk.content);
        if chunk.is_final {
            break;
        }
    }
    anyhow::ensure!(!content.trim().is_empty(), "LLM returned empty content");
    Ok(format!("reply: {}", content.trim()))
}

async fn test_tts(config: &AppConfig, provider: &str) -> Result<String> {
    let mut config = config.clone();
    config.tts_provider = provider.to_string();
    let chunks = synthesize_mp3_chunks(&config, "好的，语音合成测试。").await?;
    let bytes: usize = chunks.iter().map(Vec::len).sum();
    anyhow::ensure!(bytes > 0, "TTS returned zero bytes");
    Ok(format!("audio chunks: {}, bytes: {bytes}", chunks.len()))
}

async fn test_standard_tts(config: &AppConfig) -> Result<String> {
    let primary = test_tts(config, "standard").await;
    if primary.is_ok() {
        return primary;
    }
    let primary_error = primary.err().unwrap().to_string();
    let mut fallback_config = config.clone();
    fallback_config.tts_provider = "standard".to_string();
    fallback_config.tts_standard_endpoint = "wss://ws-api.xfyun.cn/v2/tts".to_string();
    match synthesize_mp3_chunks(&fallback_config, "好的，语音合成测试。").await {
        Ok(chunks) => {
            let bytes: usize = chunks.iter().map(Vec::len).sum();
            Ok(format!(
                "fallback ws-api ok, audio chunks: {}, bytes: {bytes}; primary failed: {primary_error}",
                chunks.len()
            ))
        }
        Err(fallback_error) => anyhow::bail!(
            "primary({}): {}; fallback(wss://ws-api.xfyun.cn/v2/tts): {}",
            config.tts_standard_endpoint,
            primary_error,
            fallback_error
        ),
    }
}

fn generate_iat_pcm() -> Result<Vec<u8>> {
    let id = Uuid::new_v4();
    let aiff = std::env::temp_dir().join(format!("mjy_iat_{id}.aiff"));
    let pcm = std::env::temp_dir().join(format!("mjy_iat_{id}.pcm"));

    let say_with_tingting = Command::new("say")
        .args(["-v", "Tingting", "-o"])
        .arg(&aiff)
        .arg("买两瓶可乐和一瓶水")
        .status();
    let say_status = match say_with_tingting {
        Ok(status) if status.success() => status,
        _ => Command::new("say")
            .arg("-o")
            .arg(&aiff)
            .arg("买两瓶可乐和一瓶水")
            .status()
            .context("run say")?,
    };
    anyhow::ensure!(say_status.success(), "say command failed");

    let ffmpeg = if std::path::Path::new("/opt/homebrew/bin/ffmpeg").exists() {
        "/opt/homebrew/bin/ffmpeg"
    } else {
        "ffmpeg"
    };
    let ffmpeg_status = Command::new(ffmpeg)
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(&aiff)
        .args(["-ac", "1", "-ar", "16000", "-f", "s16le"])
        .arg(&pcm)
        .status()
        .context("run ffmpeg")?;
    anyhow::ensure!(ffmpeg_status.success(), "ffmpeg convert pcm failed");

    let bytes = std::fs::read(&pcm).context("read generated pcm")?;
    let _ = std::fs::remove_file(aiff);
    let _ = std::fs::remove_file(pcm);
    anyhow::ensure!(!bytes.is_empty(), "generated pcm is empty");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn complete_public_device_config() -> Value {
        json!({
            "audio_profiles": {
                "input": {
                    "default": {"format": "mp3", "sample_rate": 16000},
                    "supported": [{"format": "mp3", "sample_rates": [16000]}]
                },
                "output": {
                    "default": {"format": "mp3", "sample_rate": 16000},
                    "supported": [{"format": "mp3", "sample_rates": [8000, 16000, 24000]}]
                },
                "query": ["in_format", "in_rate", "out_format", "out_rate"],
                "pcm": {"bit_depth": 16, "channels": 1, "endianness": "little"},
                "packetized": {
                    "opus": {"frame_duration_ms": 20, "one_packet_per_chunk": true},
                    "speex": {"frame_duration_ms": 20, "one_packet_per_chunk": true}
                }
            },
            "auth": {
                "type": "device_token",
                "auth_url": "/api/device/auth",
                "request": {
                    "device_id": "<configured-device-id>",
                    "device_secret": "<provisioned-device-secret>"
                },
                "note": "device credentials are provisioned out of band; seeded demo credentials are local-only"
            },
            "voice_ws": {
                "path": "/api/device/voice",
                "query": ["device_id", "token", "in_format", "in_rate", "out_format", "out_rate"],
                "client_events": ["text", "audio_stream_start", "audio_stream_chunk", "audio_stream_end", "audio_segment"],
                "server_events": ["asr_partial", "asr_final", "llm_delta", "reply_sentence", "tts_audio_chunk", "order_draft", "order_created", "voice_done", "error"]
            },
            "heartbeat_interval_ms": 15000
        })
    }

    #[test]
    fn public_config_allows_documented_placeholders_and_rejects_credential_values() {
        let valid = complete_public_device_config();
        assert!(validate_public_config_no_credentials(&valid).is_ok());

        let mut actual_secret = valid.clone();
        actual_secret["auth"]["request"]["device_secret"] = json!("actual-device-secret");
        assert!(validate_public_config_no_credentials(&actual_secret).is_err());

        let mut seeded_id = valid.clone();
        seeded_id["auth"]["request"]["device_id"] = json!("DOLL-0001");
        assert!(validate_public_config_no_credentials(&seeded_id).is_err());

        let mut nested_seeded_secret = valid;
        nested_seeded_secret["diagnostic"] = json!({"last_value": "demo-secret"});
        assert!(validate_public_config_no_credentials(&nested_seeded_secret).is_err());
    }

    #[test]
    fn public_config_requires_mp3_16k_defaults_and_no_opus_input() {
        let valid = complete_public_device_config();
        assert!(validate_public_audio_config(&valid).is_ok());
        assert!(validate_public_config_no_credentials(&valid).is_ok());

        let leaked = json!({"auth": {"device_secret": "demo-secret"}});
        assert!(validate_public_config_no_credentials(&leaked).is_err());

        let wrong_default = json!({
            "audio_profiles": {
                "input": {"default": {"format": "pcm", "sample_rate": 16000}, "supported": []},
                "output": {"default": {"format": "mp3", "sample_rate": 16000}, "supported": []}
            }
        });
        assert!(validate_public_audio_config(&wrong_default).is_err());

        let opus_input = json!({
            "audio_profiles": {
                "input": {
                    "default": {"format": "mp3", "sample_rate": 16000},
                    "supported": [{"format": "opus", "sample_rates": [16000]}]
                },
                "output": {"default": {"format": "mp3", "sample_rate": 16000}, "supported": []}
            }
        });
        assert!(validate_public_audio_config(&opus_input).is_err());
    }

    #[test]
    fn public_ws_url_preserves_base_path_and_all_four_audio_parameters() {
        let url = build_public_chat_voice_ws_url(
            "https://example.test/myj-voice-shop/",
            "mp3",
            16_000,
            "mp3",
            8_000,
        )
        .unwrap();
        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.path(), "/myj-voice-shop/api/chat/voice");
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query.get("in_format").map(|v| v.as_ref()), Some("mp3"));
        assert_eq!(query.get("in_rate").map(|v| v.as_ref()), Some("16000"));
        assert_eq!(query.get("out_format").map(|v| v.as_ref()), Some("mp3"));
        assert_eq!(query.get("out_rate").map(|v| v.as_ref()), Some("8000"));
    }

    #[test]
    fn failed_smoke_case_makes_the_suite_fail() {
        let results = vec![
            SmokeResult {
                name: "ok",
                ok: true,
                detail: "ok".to_string(),
            },
            SmokeResult {
                name: "bad",
                ok: false,
                detail: "failed".to_string(),
            },
        ];
        assert!(ensure_all_cases_pass(&results).is_err());
    }

    #[test]
    fn anonymous_internal_admin_probe_accepts_login_required() {
        assert!(validate_internal_admin_status(reqwest::StatusCode::UNAUTHORIZED).is_ok());
        assert!(validate_internal_admin_status(reqwest::StatusCode::FORBIDDEN).is_err());
        assert!(validate_internal_admin_status(reqwest::StatusCode::OK).is_err());
        assert!(validate_internal_admin_status(reqwest::StatusCode::NOT_FOUND).is_err());
    }

    #[test]
    fn public_demo_device_auth_must_be_rejected() {
        assert!(validate_public_demo_auth_status(reqwest::StatusCode::UNAUTHORIZED).is_ok());
        assert!(validate_public_demo_auth_status(reqwest::StatusCode::FORBIDDEN).is_ok());
        assert!(validate_public_demo_auth_status(reqwest::StatusCode::OK).is_err());
    }
}
