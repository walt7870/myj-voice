use std::{
    collections::{HashSet, VecDeque},
    future::Future,
    net::IpAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::{
        connect_info::ConnectInfo,
        ws::{Message, WebSocket, WebSocketUpgrade},
        FromRequestParts, Path, Query, Request as AxumRequest, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware,
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::{
    sync::{
        broadcast, mpsc, oneshot, watch, Mutex, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore,
    },
    task::JoinHandle,
};
use tower_http::{services::ServeDir, set_header::SetResponseHeaderLayer};
use uuid::Uuid;

use crate::xfyun::audio::{
    iat_supports, supported_iat_profiles, supported_tts_profiles, tts_supports, IatProvider,
    TtsProvider,
};
use crate::{
    config::AppConfig,
    db,
    domain::{
        audio::{
            AudioFormat, AudioProfile, AudioProfileError, AudioSampleRate, VoiceConnectionAudio,
        },
        device_auth::verify_device_token,
        device_auth::{issue_device_token, secret_hash},
        matching::{match_products, Product, ProductMatch},
        order::{create_mock_order, order_error, OrderMcpClient},
    },
    xfyun::{
        auth::{build_signed_ws_url, current_rfc1123_date},
        iat::{
            build_iat_frame_for_profile, merge_iat_text, parse_iat_text_for_provider,
            recognize_audio, validate_input_packet, validate_input_packet_for_provider,
            AudioPacketError, IatFrameKind, IatUpstreamError,
        },
        llm::{split_complete_sentences, stream_chat_chunks, ChatMessage},
        tts::{
            start_audio_profile_chunks, start_super_smart_tts_text_frames_for_profile,
            TimedTtsAudioChunk, TtsAudioProfileError, TtsTextFrame, TtsUpstreamError,
        },
    },
};
use tokio_tungstenite::{connect_async, tungstenite::Message as UpstreamMessage};

mod admin;
#[allow(dead_code)]
mod turn_interrupt;

use turn_interrupt::{InterruptStatus, TurnInterruptRegistry};

pub const MAX_DECODED_AUDIO_BYTES: usize = 64 * 1024;
pub const MAX_AUDIO_BASE64_BYTES: usize = MAX_DECODED_AUDIO_BYTES.div_ceil(3) * 4;
const MAX_VOICE_WS_MESSAGE_BYTES: usize = 128 * 1024;
const WS_CONTROL_QUEUE_CAPACITY: usize = 64;
const WS_TURN_CAPACITY: usize = 64;
const WS_ANALYSIS_QUEUE_CAPACITY: usize = 64;
pub const LIVE_IAT_SESSION_TIMEOUT: Duration = Duration::from_secs(20);
const LOCAL_DEMO_DEVICE_ID: &str = "DOLL-0001";

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub server_secret: Arc<String>,
    pub admin_config: crate::admin_auth::AdminConfig,
    pub diagnostics: broadcast::Sender<DiagnosticEvent>,
}

pub fn router(state: AppState) -> Router {
    let internal_routes = Router::new()
        .merge(admin::routes())
        .route("/api/admin/config", get(get_config).put(update_config))
        .route("/api/admin/products", get(list_products).post(save_product))
        .route("/api/admin/products/{id}", put(save_product_with_id))
        .route("/api/admin/conversations", get(list_conversations))
        .route(
            "/api/admin/conversations/{id}",
            get(get_conversation_detail),
        )
        .route("/api/diagnostics/latency", get(diagnostics_ws))
        .route(
            "/api/debug/miniprogram-c/interfaces",
            get(miniprogram_c_interfaces),
        )
        .route(
            "/api/debug/miniprogram-c/call",
            post(miniprogram_c_debug_call),
        )
        .route(
            "/mock/app-catering/api/app/saleorder/get-user-sale-orders",
            get(mock_miniprogram_order_list),
        )
        .route(
            "/mock/app-catering/api/app/saleorder/get-user-sale-order-detail",
            get(mock_miniprogram_order_detail),
        )
        .route(
            "/mock/app-catering/api/app/saleorder/create-order",
            post(mock_miniprogram_create_order),
        )
        .route(
            "/mock/app-catering/api/app/saleorder/cancel-sale-order",
            post(mock_miniprogram_cancel_order),
        )
        .route(
            "/mock/app-catering/api/app/saleorder/pay-order",
            post(mock_miniprogram_pay_order),
        )
        .route(
            "/mock/app-catering/api/app/saleorder/apply-refund",
            post(mock_miniprogram_apply_refund),
        );

    Router::new()
        .route("/api/health", get(health))
        .route("/api/public/config", get(get_config))
        .merge(internal_routes)
        .route("/api/conversations/new", post(new_conversation))
        .route("/api/chat/text", post(chat_text))
        .route("/api/chat/voice", get(chat_ws))
        .route("/api/device/auth", post(device_auth))
        .route("/api/device/status", post(device_status))
        .route("/api/device/config", get(device_config))
        .route("/api/device/voice", get(device_voice_ws))
        .route("/api/order/confirm", post(confirm_order))
        .route("/api/orders/list", post(list_orders))
        .route("/api/orders/detail", post(get_order_detail))
        .route("/api/orders/refund", post(refund_order))
        .fallback_service(ServeDir::new("static").append_index_html_on_directories(true))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin::require_admin_access,
        ))
        .with_state(state)
}

fn is_internal_management_path(path: &str) -> bool {
    path == "/api/admin"
        || path.starts_with("/api/admin/")
        || path == "/api/debug"
        || path.starts_with("/api/debug/")
        || path == "/mock"
        || path.starts_with("/mock/")
        || path == "/api/order"
        || path.starts_with("/api/order/")
        || path == "/api/orders"
        || path.starts_with("/api/orders/")
        || path == "/api/diagnostics"
        || path.starts_with("/api/diagnostics/")
}

fn is_internal_management_request(peer_ip: Option<IpAddr>, x_real_ip: Option<&str>) -> bool {
    if !peer_ip.is_some_and(|ip| ip.is_loopback()) {
        return false;
    }
    x_real_ip.is_none_or(|value| {
        value
            .trim()
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    })
}

fn is_trusted_internal_source(
    connect_info: Option<ConnectInfo<std::net::SocketAddr>>,
    headers: &HeaderMap,
) -> bool {
    let peer_ip = connect_info.map(|ConnectInfo(address)| address.ip());
    let mut x_real_ips = headers.get_all("x-real-ip").iter();
    let Some(first) = x_real_ips.next() else {
        return is_internal_management_request(peer_ip, None);
    };
    let Ok(first) = first.to_str() else {
        return false;
    };
    is_internal_management_request(peer_ip, Some(first))
        && x_real_ips.all(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.trim().parse::<IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback())
        })
}

#[cfg(test)]
mod internal_access_tests {
    use super::{is_internal_management_path, is_internal_management_request};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn requires_loopback_peer_and_loopback_x_real_ip_when_present() {
        let local_v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let local_v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        let public = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 42));

        assert!(is_internal_management_request(Some(local_v4), None));
        assert!(is_internal_management_request(
            Some(local_v4),
            Some("127.0.0.1")
        ));
        assert!(is_internal_management_request(Some(local_v6), Some("::1")));
        assert!(!is_internal_management_request(None, None));
        assert!(!is_internal_management_request(Some(public), None));
        assert!(!is_internal_management_request(
            Some(public),
            Some("127.0.0.1")
        ));
        assert!(!is_internal_management_request(
            Some(local_v4),
            Some("203.0.113.42")
        ));
        assert!(!is_internal_management_request(
            Some(local_v4),
            Some("127.0.0.1, 203.0.113.42")
        ));
    }

    #[test]
    fn protects_entire_management_debug_and_mock_namespaces() {
        for path in [
            "/api/admin/config",
            "/api/admin/future",
            "/api/debug/future",
            "/mock/future",
            "/api/order/confirm",
            "/api/order/future",
            "/api/orders/list",
            "/api/orders/future",
            "/api/diagnostics/latency",
            "/api/diagnostics/future",
        ] {
            assert!(is_internal_management_path(path), "{path}");
        }
        for path in ["/admin.html", "/api/health", "/api/device/config"] {
            assert!(!is_internal_management_path(path), "{path}");
        }
    }
}

async fn health() -> Json<Value> {
    Json(json!({"status":"ok","service":"mjy-voice-shop-rs"}))
}

async fn get_config(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let config = db::get_config(&state.pool).await?;
    Ok(Json(json!(config.to_public())))
}

async fn update_config(
    State(state): State<AppState>,
    Json(mut input): Json<AppConfig>,
) -> Result<Json<Value>, ApiError> {
    let old = db::get_config(&state.pool).await?;
    if input.api_key.trim().is_empty() {
        input.api_key = old.api_key;
    }
    if input.api_secret.trim().is_empty() {
        input.api_secret = old.api_secret;
    }
    if input.order_mcp_token.trim().is_empty() {
        input.order_mcp_token = old.order_mcp_token;
    }
    db::save_config(&state.pool, &input).await?;
    Ok(Json(json!(input.to_public())))
}

async fn list_products(State(state): State<AppState>) -> Result<Json<Vec<Product>>, ApiError> {
    Ok(Json(db::list_products(&state.pool).await?))
}

async fn save_product(
    State(state): State<AppState>,
    Json(product): Json<Product>,
) -> Result<Json<Product>, ApiError> {
    db::upsert_product(&state.pool, &product).await?;
    Ok(Json(product))
}

async fn save_product_with_id(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut product): Json<Product>,
) -> Result<Json<Product>, ApiError> {
    product.id = id;
    db::upsert_product(&state.pool, &product).await?;
    Ok(Json(product))
}

async fn new_conversation(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let conversation_id = Uuid::new_v4().to_string();
    db::ensure_conversation_owned(
        &state.pool,
        &conversation_id,
        &db::ConversationOwner::Browser,
    )
    .await?;
    Ok(Json(json!({
        "conversation_id": conversation_id,
        "created_at": Utc::now().to_rfc3339()
    })))
}

#[derive(Debug, Deserialize)]
struct ConversationPageQuery {
    page: Option<i64>,
    page_size: Option<i64>,
}

async fn list_conversations(
    State(state): State<AppState>,
    Query(query): Query<ConversationPageQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!(
        db::list_conversations_page(
            &state.pool,
            query.page.unwrap_or(1),
            query.page_size.unwrap_or(10)
        )
        .await?
    )))
}

async fn get_conversation_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let messages = db::list_conversation_messages(&state.pool, &id).await?;
    let events = db::list_conversation_events(&state.pool, &id).await?;
    let orders = db::list_mock_order_payloads_by_conversation(&state.pool, &id).await?;
    Ok(Json(json!({
        "conversation_id": id,
        "messages": messages,
        "events": events,
        "orders": orders
    })))
}

#[derive(Debug, Deserialize)]
struct ChatTextRequest {
    text: String,
    conversation_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatTextResponse {
    conversation_id: String,
    turn_id: String,
    events: Vec<StreamEvent>,
}

async fn chat_text(
    State(state): State<AppState>,
    Json(req): Json<ChatTextRequest>,
) -> Result<Json<ChatTextResponse>, ApiError> {
    let config = db::get_config(&state.pool).await?;
    let iat_provider = IatProvider::parse(&config.iat_provider)?;
    let tts_provider = TtsProvider::parse(&config.tts_provider)?;
    let audio = resolve_voice_audio(None, None, None, None, iat_provider, tts_provider)?;
    let audio_context = VoiceAudioContext {
        audio,
        iat_provider,
        tts_provider,
        config,
    };
    let conversation_id = req
        .conversation_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let turn_id = Uuid::new_v4().to_string();
    let mut events = Vec::new();
    run_turn(
        &state,
        &conversation_id,
        &turn_id,
        &db::ConversationOwner::Browser,
        &req.text,
        None,
        audio_context,
        |event| {
            events.push(event);
            async {}
        },
    )
    .await?;
    Ok(Json(ChatTextResponse {
        conversation_id,
        turn_id,
        events,
    }))
}

#[derive(Debug, Default, Deserialize)]
struct VoiceWsQuery {
    device_id: Option<String>,
    token: Option<String>,
    in_format: Option<String>,
    in_rate: Option<String>,
    out_format: Option<String>,
    out_rate: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum VoiceAudioError {
    InvalidProfile(AudioProfileError),
    UnsupportedProfile {
        direction: &'static str,
        profile: AudioProfile,
        provider: &'static str,
    },
}

impl VoiceAudioError {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidProfile(error) => error.code(),
            Self::UnsupportedProfile { .. } => "unsupported_audio_profile",
        }
    }
}

impl std::fmt::Display for VoiceAudioError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfile(error) => error.fmt(formatter),
            Self::UnsupportedProfile {
                direction,
                profile,
                provider,
            } => write!(
                formatter,
                "unsupported {direction} audio profile for provider {provider}: format={}, rate={}",
                profile.format.as_str(),
                profile.sample_rate.hz()
            ),
        }
    }
}

impl std::error::Error for VoiceAudioError {}

impl From<AudioProfileError> for VoiceAudioError {
    fn from(error: AudioProfileError) -> Self {
        Self::InvalidProfile(error)
    }
}

fn resolve_voice_audio(
    input_format: Option<&str>,
    input_rate: Option<&str>,
    output_format: Option<&str>,
    output_rate: Option<&str>,
    iat_provider: IatProvider,
    tts_provider: TtsProvider,
) -> Result<VoiceConnectionAudio, VoiceAudioError> {
    let audio =
        VoiceConnectionAudio::from_query(input_format, input_rate, output_format, output_rate)?;
    if !iat_supports(iat_provider, audio.input) {
        return Err(VoiceAudioError::UnsupportedProfile {
            direction: "input",
            profile: audio.input,
            provider: iat_provider_name(iat_provider),
        });
    }
    if !tts_supports(tts_provider, audio.output) {
        return Err(VoiceAudioError::UnsupportedProfile {
            direction: "output",
            profile: audio.output,
            provider: tts_provider_name(tts_provider),
        });
    }
    Ok(audio)
}

fn iat_provider_name(provider: IatProvider) -> &'static str {
    match provider {
        IatProvider::SuperSmart => "super_smart",
        IatProvider::Standard => "standard",
    }
}

fn tts_provider_name(provider: TtsProvider) -> &'static str {
    match provider {
        TtsProvider::SuperSmart => "super_smart",
        TtsProvider::Standard => "standard",
    }
}

fn grouped_audio_profiles(profiles: &[AudioProfile]) -> Vec<Value> {
    let mut grouped: Vec<(AudioFormat, Vec<u32>)> = Vec::new();
    for profile in profiles {
        if let Some((_, rates)) = grouped
            .iter_mut()
            .find(|(format, _)| *format == profile.format)
        {
            if !rates.contains(&profile.sample_rate.hz()) {
                rates.push(profile.sample_rate.hz());
            }
        } else {
            grouped.push((profile.format, vec![profile.sample_rate.hz()]));
        }
    }
    grouped
        .into_iter()
        .map(|(format, sample_rates)| {
            json!({"format": format.as_str(), "sample_rates": sample_rates})
        })
        .collect()
}

pub fn decode_audio_packet(
    audio: Option<&str>,
    profile: AudioProfile,
) -> Result<Vec<u8>, AudioPacketError> {
    let encoded = audio.ok_or_else(|| AudioPacketError::invalid("audio packet is missing"))?;
    if encoded.len() > MAX_AUDIO_BASE64_BYTES {
        return Err(AudioPacketError::invalid(format!(
            "encoded audio packet is {} bytes; maximum is {MAX_AUDIO_BASE64_BYTES}",
            encoded.len()
        )));
    }
    let decoded = STANDARD
        .decode(encoded.as_bytes())
        .map_err(|_| AudioPacketError::invalid("audio packet is not valid base64"))?;
    validate_input_packet(profile, &decoded)?;
    Ok(decoded)
}

pub fn decode_live_audio_packet(
    context: Option<(AudioProfile, IatProvider)>,
    audio: Option<&str>,
) -> Result<Vec<u8>, AudioPacketError> {
    let (profile, provider) = context.ok_or_else(|| {
        AudioPacketError::invalid("audio_stream_chunk requires an active audio stream")
    })?;
    let decoded = decode_audio_packet(audio, profile)?;
    validate_input_packet_for_provider(profile, &decoded, provider)?;
    Ok(decoded)
}

fn decode_segment_audio_packet(
    audio: Option<&str>,
    profile: AudioProfile,
) -> Result<Vec<u8>, AudioPacketError> {
    if matches!(profile.format, AudioFormat::Opus | AudioFormat::Speex) {
        return Err(AudioPacketError::invalid(
            "packetized audio requires audio_stream_start/chunk/end with one packet per chunk",
        ));
    }
    decode_audio_packet(audio, profile)
}

fn audio_packet_error_event(error: &AudioPacketError) -> StreamEvent {
    StreamEvent::error(error.code(), &error.to_string())
}

fn voice_audio_error_response(error: VoiceAudioError) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": error.code(), "message": error.to_string()})),
    )
        .into_response()
}

fn config_error_response(error: impl std::fmt::Display) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "config_error", "message": error.to_string()})),
    )
        .into_response()
}

#[derive(Clone)]
struct VoiceAudioContext {
    audio: VoiceConnectionAudio,
    iat_provider: IatProvider,
    tts_provider: TtsProvider,
    config: AppConfig,
}

async fn negotiate_voice_audio(
    state: &AppState,
    query: &VoiceWsQuery,
) -> Result<VoiceAudioContext, axum::response::Response> {
    let config = db::get_config(&state.pool)
        .await
        .map_err(config_error_response)?;
    let iat_provider = IatProvider::parse(&config.iat_provider).map_err(config_error_response)?;
    let tts_provider = TtsProvider::parse(&config.tts_provider).map_err(config_error_response)?;
    let audio = resolve_voice_audio(
        query.in_format.as_deref(),
        query.in_rate.as_deref(),
        query.out_format.as_deref(),
        query.out_rate.as_deref(),
        iat_provider,
        tts_provider,
    )
    .map_err(voice_audio_error_response)?;
    Ok(VoiceAudioContext {
        audio,
        iat_provider,
        tts_provider,
        config,
    })
}

async fn chat_ws(
    State(state): State<AppState>,
    Query(query): Query<VoiceWsQuery>,
    request: AxumRequest,
) -> impl IntoResponse {
    let negotiated = match negotiate_voice_audio(&state, &query).await {
        Ok(negotiated) => negotiated,
        Err(response) => return response,
    };
    let (mut parts, _) = request.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let ws = ws.max_message_size(MAX_VOICE_WS_MESSAGE_BYTES);
    ws.on_upgrade(move |socket| {
        handle_ws(socket, state, db::ConversationOwner::Browser, negotiated)
    })
    .into_response()
}

async fn diagnostics_ws(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let trace_id = query.get("trace_id").cloned();
    ws.on_upgrade(move |mut socket| async move {
        let mut rx = state.diagnostics.subscribe();
        while let Ok(event) = rx.recv().await {
            if let Some(trace_id) = trace_id.as_deref() {
                if event.trace_id.as_deref() != Some(trace_id) {
                    continue;
                }
            }
            if socket
                .send(Message::Text(serde_json::to_string(&event).unwrap().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    })
}

async fn device_voice_ws(
    State(state): State<AppState>,
    Query(query): Query<VoiceWsQuery>,
    request: AxumRequest,
) -> impl IntoResponse {
    let device_id = query
        .device_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let connect_info = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .copied();
    if device_id == LOCAL_DEMO_DEVICE_ID
        && !is_trusted_internal_source(connect_info, request.headers())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "设备 WebSocket token 无效"})),
        )
            .into_response();
    }
    let Some(token) = query.token.as_deref() else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "设备 WebSocket 缺少 token"})),
        )
            .into_response();
    };
    match verify_device_token(token, &state.server_secret, Utc::now().timestamp()) {
        Ok(claims) if claims.device_id == device_id => {}
        _ => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "设备 WebSocket token 无效"})),
            )
                .into_response();
        }
    }
    match db::device_is_enabled(&state.pool, &device_id).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "设备已停用或不存在"})),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!(%error, %device_id, "failed to verify device status");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "设备状态校验失败"})),
            )
                .into_response();
        }
    }
    let negotiated = match negotiate_voice_audio(&state, &query).await {
        Ok(negotiated) => negotiated,
        Err(response) => return response,
    };
    let (mut parts, _) = request.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    let ws = ws.max_message_size(MAX_VOICE_WS_MESSAGE_BYTES);
    ws.on_upgrade(move |socket| {
        handle_ws(
            socket,
            state,
            db::ConversationOwner::Device(device_id),
            negotiated,
        )
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct WsInput {
    #[serde(alias = "type")]
    event_type: String,
    conversation_id: Option<String>,
    turn_id: Option<String>,
    source: Option<String>,
    text: Option<String>,
    audio: Option<String>,
    trace_id: Option<String>,
    client_sent_ms: Option<i64>,
}

struct AnalysisTicket {
    receiver: oneshot::Receiver<OwnedMutexGuard<()>>,
    slot: OwnedSemaphorePermit,
}

struct AnalysisGuard {
    _turn: OwnedMutexGuard<()>,
    _slot: OwnedSemaphorePermit,
}

struct PreparedAnalysisTurn {
    _guard: AnalysisGuard,
    round_text: String,
}

#[derive(Clone)]
struct TurnCapacityHold {
    _permit: Arc<OwnedSemaphorePermit>,
}

struct TurnCapacityPermit {
    hold: TurnCapacityHold,
}

impl TurnCapacityPermit {
    fn hold(&self) -> TurnCapacityHold {
        self.hold.clone()
    }
}

impl AnalysisTicket {
    async fn acquire(self) -> Result<AnalysisGuard, oneshot::error::RecvError> {
        let turn = self.receiver.await?;
        Ok(AnalysisGuard {
            _turn: turn,
            _slot: self.slot,
        })
    }
}

#[derive(Debug)]
struct AnalysisQueueFull;

#[derive(Debug)]
struct TurnCapacityFull;

#[derive(Default)]
struct InterruptedOutputFilter {
    turns: HashSet<String>,
    recently_finished: VecDeque<String>,
}

impl InterruptedOutputFilter {
    fn interrupt(&mut self, turn_id: String) {
        if !self.recently_finished.contains(&turn_id) {
            self.turns.insert(turn_id);
        }
    }

    fn finish(&mut self, turn_id: &str) {
        self.turns.remove(turn_id);
        self.recently_finished.push_back(turn_id.to_string());
        while self.recently_finished.len() > WS_CONTROL_QUEUE_CAPACITY {
            self.recently_finished.pop_front();
        }
    }

    fn suppresses(&self, turn_id: &str, event_type: &str) -> bool {
        self.turns.contains(turn_id)
            && matches!(
                event_type,
                "llm_delta" | "reply_sentence" | "tts_audio_chunk" | "voice_done"
            )
    }
}

#[derive(Clone)]
struct WsTurnCoordinator {
    interrupts: TurnInterruptRegistry,
    analysis_tickets: mpsc::Sender<oneshot::Sender<OwnedMutexGuard<()>>>,
    analysis_slots: Arc<Semaphore>,
    turn_capacity: Arc<Semaphore>,
}

impl WsTurnCoordinator {
    fn new() -> Self {
        Self::new_with_capacities(WS_TURN_CAPACITY, WS_ANALYSIS_QUEUE_CAPACITY)
    }

    fn new_with_capacities(turn_capacity: usize, analysis_capacity: usize) -> Self {
        let mutex = Arc::new(Mutex::new(()));
        let (analysis_tickets, mut tickets) =
            mpsc::channel::<oneshot::Sender<OwnedMutexGuard<()>>>(analysis_capacity);
        tokio::spawn(async move {
            while let Some(ticket) = tickets.recv().await {
                let guard = mutex.clone().lock_owned().await;
                let _ = ticket.send(guard);
            }
        });
        Self {
            interrupts: TurnInterruptRegistry::default(),
            analysis_tickets,
            analysis_slots: Arc::new(Semaphore::new(analysis_capacity)),
            turn_capacity: Arc::new(Semaphore::new(turn_capacity)),
        }
    }

    fn allocate(&self, conversation_id: Option<String>) -> (String, String) {
        (
            conversation_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
            Uuid::new_v4().to_string(),
        )
    }

    fn try_reserve_analysis(&self) -> Result<AnalysisTicket, AnalysisQueueFull> {
        let slot = self
            .analysis_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AnalysisQueueFull)?;
        let (ticket, receiver) = oneshot::channel();
        self.analysis_tickets
            .try_send(ticket)
            .map_err(|_| AnalysisQueueFull)?;
        Ok(AnalysisTicket { receiver, slot })
    }

    fn try_reserve_turn_capacity(&self) -> Result<TurnCapacityPermit, TurnCapacityFull> {
        let permit = self
            .turn_capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| TurnCapacityFull)?;
        Ok(TurnCapacityPermit {
            hold: TurnCapacityHold {
                _permit: Arc::new(permit),
            },
        })
    }
}

impl Default for WsTurnCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
struct TurnTrace {
    trace_id: Option<String>,
    base_ms: i64,
}

enum LiveAsrFrame {
    Audio {
        audio: Vec<u8>,
        relay_started: Instant,
    },
    End,
}

struct LiveAsrSession {
    sender: Option<mpsc::Sender<LiveAsrFrame>>,
    task: Option<JoinHandle<()>>,
    profile: AudioProfile,
    provider: IatProvider,
}

impl LiveAsrSession {
    fn new(
        sender: mpsc::Sender<LiveAsrFrame>,
        task: JoinHandle<()>,
        profile: AudioProfile,
        provider: IatProvider,
    ) -> Self {
        Self {
            sender: Some(sender),
            task: Some(task),
            profile,
            provider,
        }
    }

    fn is_accepting_audio(&self) -> bool {
        self.sender.is_some() && !self.is_finished()
    }

    fn is_finished(&self) -> bool {
        self.task.as_ref().map_or(true, JoinHandle::is_finished)
    }

    async fn finish_input(&mut self) -> Result<(), AudioPacketError> {
        let sender = self.sender.take().ok_or_else(|| {
            AudioPacketError::invalid("audio stream has already ended or is not active")
        })?;
        sender
            .send(LiveAsrFrame::End)
            .await
            .map_err(|_| AudioPacketError::invalid("audio stream is no longer accepting packets"))
    }

    async fn stop_and_join(&mut self) -> anyhow::Result<()> {
        self.sender.take();
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.abort();
        match task.await {
            Ok(()) => Ok(()),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(anyhow::anyhow!("live IAT task failed: {error}")),
        }
    }

    async fn reap_finished(&mut self) -> anyhow::Result<bool> {
        if !self.is_finished() {
            return Ok(false);
        }
        self.sender.take();
        let Some(task) = self.task.take() else {
            return Ok(true);
        };
        task.await
            .map_err(|error| anyhow::anyhow!("live IAT task failed: {error}"))?;
        Ok(true)
    }
}

impl Drop for LiveAsrSession {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn stop_live_asr_session(slot: &mut Option<LiveAsrSession>) -> anyhow::Result<()> {
    if let Some(mut session) = slot.take() {
        session.stop_and_join().await?;
    }
    Ok(())
}

async fn reap_finished_live_asr_session(slot: &mut Option<LiveAsrSession>) -> anyhow::Result<bool> {
    if !slot.as_ref().is_some_and(LiveAsrSession::is_finished) {
        return Ok(false);
    }
    let Some(mut session) = slot.take() else {
        return Ok(false);
    };
    session.reap_finished().await
}

struct RecognizedTurn {
    state: AppState,
    tx: mpsc::Sender<StreamEvent>,
    conversation_id: String,
    turn_id: String,
    coordinator: WsTurnCoordinator,
    turn_capacity: TurnCapacityPermit,
    owner: db::ConversationOwner,
    text: String,
    trace: Option<TurnTrace>,
    audio_context: VoiceAudioContext,
}

impl RecognizedTurn {
    fn new(
        state: AppState,
        tx: mpsc::Sender<StreamEvent>,
        conversation_id: String,
        turn_id: String,
        coordinator: WsTurnCoordinator,
        turn_capacity: TurnCapacityPermit,
        owner: db::ConversationOwner,
        text: String,
        trace: Option<TurnTrace>,
        audio_context: VoiceAudioContext,
    ) -> Option<Self> {
        if text.trim().is_empty() {
            return None;
        }
        Some(Self {
            state,
            tx,
            conversation_id,
            turn_id,
            coordinator,
            turn_capacity,
            owner,
            text,
            trace,
            audio_context,
        })
    }

    fn handoff(self) {
        // This task owns the business turn after ASR final handoff. It is deliberately
        // detached from LiveAsrSession, so stopping a later IAT session cannot cancel it.
        let analysis_ticket = match self.coordinator.try_reserve_analysis() {
            Ok(ticket) => ticket,
            Err(_) => {
                let _ = self.tx.try_send(
                    StreamEvent::error(
                        "analysis_queue_full",
                        "too many recognized turns are waiting for analysis",
                    )
                    .with_context(&self.conversation_id, &self.turn_id),
                );
                return;
            }
        };
        drop(self.handoff_with(|turn| async move {
            run_turn_to_channel(
                turn.state,
                turn.tx,
                turn.conversation_id,
                turn.turn_id,
                turn.coordinator,
                turn.turn_capacity,
                analysis_ticket,
                turn.owner,
                turn.text,
                turn.trace,
                turn.audio_context,
            )
            .await;
        }));
    }

    fn handoff_with<F, Fut>(self, run: F) -> JoinHandle<()>
    where
        F: FnOnce(Self) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        spawn_downstream_turn(run(self))
    }
}

fn spawn_downstream_turn<F>(future: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(future)
}

struct LiveIatWriterState {
    app_id: String,
    profile: AudioProfile,
    provider: IatProvider,
    started: bool,
    audio_bytes: usize,
}

impl LiveIatWriterState {
    fn new(app_id: String, profile: AudioProfile, provider: IatProvider) -> Self {
        Self {
            app_id,
            profile,
            provider,
            started: false,
            audio_bytes: 0,
        }
    }

    fn from_audio_context(context: &VoiceAudioContext) -> Self {
        Self::new(
            context.config.app_id.clone(),
            context.audio.input,
            context.iat_provider,
        )
    }

    fn handle_frame(
        &mut self,
        frame: LiveAsrFrame,
    ) -> anyhow::Result<(Value, bool, Option<Instant>)> {
        match frame {
            LiveAsrFrame::Audio {
                audio,
                relay_started,
            } => {
                let kind = if self.started {
                    IatFrameKind::Continue
                } else {
                    IatFrameKind::First
                };
                let payload = build_iat_frame_for_profile(
                    &self.app_id,
                    kind,
                    &audio,
                    self.profile,
                    self.provider,
                )?;
                self.started = true;
                self.audio_bytes += audio.len();
                Ok((payload, false, Some(relay_started)))
            }
            LiveAsrFrame::End if !self.started => {
                Err(AudioPacketError::invalid("audio stream ended before any audio packet").into())
            }
            LiveAsrFrame::End => {
                let payload = build_iat_frame_for_profile(
                    &self.app_id,
                    IatFrameKind::Last,
                    &[],
                    self.profile,
                    self.provider,
                )?;
                Ok((payload, true, None))
            }
        }
    }

    fn channel_closed(&self) -> anyhow::Result<usize> {
        Err(AudioPacketError::invalid("audio stream input closed before audio_stream_end").into())
    }
}

async fn couple_live_iat_io<W, R>(writer: W, reader: R) -> anyhow::Result<(usize, String)>
where
    W: Future<Output = anyhow::Result<usize>>,
    R: Future<Output = anyhow::Result<String>>,
{
    Ok(tokio::try_join!(writer, reader)?)
}

async fn handle_ws(
    socket: WebSocket,
    state: AppState,
    owner: db::ConversationOwner,
    audio_context: VoiceAudioContext,
) {
    let device_id = match &owner {
        db::ConversationOwner::Browser => "debug-browser".to_string(),
        db::ConversationOwner::Device(device_id) => device_id.clone(),
    };
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(64);
    let (control_tx, mut control_rx) = mpsc::channel::<StreamEvent>(WS_CONTROL_QUEUE_CAPACITY);
    let mut live_asr: Option<LiveAsrSession> = None;
    let coordinator = WsTurnCoordinator::default();
    let writer_state = state.clone();
    let writer = tokio::spawn(async move {
        let mut interrupted = InterruptedOutputFilter::default();
        while let Some(event) = next_ws_output_event(&mut control_rx, &mut rx).await {
            let Some(event) = filter_ws_output_event(&mut interrupted, event) else {
                continue;
            };
            if send_client_event(&writer_state, event, |message| sender.send(message))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(Ok(message)) = receiver.next().await {
        match message {
            Message::Text(raw) => {
                let parsed: Result<WsInput, _> = serde_json::from_str(&raw);
                let Ok(input) = parsed else {
                    let _ = tx
                        .send(StreamEvent::error("bad_request", "无法解析 WebSocket 消息"))
                        .await;
                    continue;
                };
                if let Err(error) = reap_finished_live_asr_session(&mut live_asr).await {
                    let _ = tx
                        .send(StreamEvent::error("asr_failed", &error.to_string()))
                        .await;
                }
                if input.event_type == "tts_interrupt" {
                    if !handle_device_button_interrupt(&control_tx, &coordinator.interrupts, &input)
                        .await
                    {
                        break;
                    }
                    continue;
                }
                if input.event_type == "audio_stream_start" {
                    let Ok(turn_capacity) = coordinator.try_reserve_turn_capacity() else {
                        let _ = try_enqueue_ws_control(
                            &control_tx,
                            StreamEvent::error(
                                "turn_capacity_full",
                                "too many voice turns are in flight",
                            ),
                        );
                        break;
                    };
                    if let Err(error) = stop_live_asr_session(&mut live_asr).await {
                        let _ = tx
                            .send(StreamEvent::error("asr_failed", &error.to_string()))
                            .await;
                    }
                    let input_profile = audio_context.audio.input;
                    let server_received_ms = Utc::now().timestamp_millis();
                    let trace = TurnTrace {
                        trace_id: input.trace_id.clone(),
                        base_ms: input.client_sent_ms.unwrap_or(server_received_ms),
                    };
                    let (asr_tx, asr_rx) = mpsc::channel(64);
                    let (conversation_id, turn_id) =
                        coordinator.allocate(input.conversation_id.clone());
                    emit_diagnostic(
                        &state,
                        Some(&conversation_id),
                        None,
                        Some(&trace),
                        "audio_received",
                        json!({"mode": "streaming"}),
                    );
                    let task = tokio::spawn(run_live_asr_to_channel(
                        state.clone(),
                        tx.clone(),
                        conversation_id,
                        turn_id,
                        coordinator.clone(),
                        turn_capacity,
                        owner.clone(),
                        trace,
                        asr_rx,
                        audio_context.clone(),
                    ));
                    live_asr = Some(LiveAsrSession::new(
                        asr_tx,
                        task,
                        input_profile,
                        audio_context.iat_provider,
                    ));
                    continue;
                }
                if input.event_type == "audio_stream_chunk" {
                    let relay_started = Instant::now();
                    let context = live_asr
                        .as_ref()
                        .filter(|session| session.is_accepting_audio())
                        .map(|session| (session.profile, session.provider));
                    let decoded = match decode_live_audio_packet(context, input.audio.as_deref()) {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            let _ = tx.send(audio_packet_error_event(&error)).await;
                            continue;
                        }
                    };
                    let asr_tx = match live_asr
                        .as_ref()
                        .and_then(|session| session.sender.as_ref())
                        .cloned()
                    {
                        Some(sender) => sender,
                        None => {
                            let error = AudioPacketError::invalid(
                                "audio_stream_chunk requires an active audio stream",
                            );
                            let _ = tx.send(audio_packet_error_event(&error)).await;
                            continue;
                        }
                    };
                    if asr_tx
                        .send(LiveAsrFrame::Audio {
                            audio: decoded,
                            relay_started,
                        })
                        .await
                        .is_err()
                    {
                        if let Err(error) = stop_live_asr_session(&mut live_asr).await {
                            let _ = tx
                                .send(StreamEvent::error("asr_failed", &error.to_string()))
                                .await;
                        }
                        let error = AudioPacketError::invalid(
                            "audio stream is no longer accepting packets",
                        );
                        let _ = tx.send(audio_packet_error_event(&error)).await;
                    }
                    continue;
                }
                if input.event_type == "audio_stream_end" {
                    let result = match live_asr.as_mut() {
                        Some(session) => session.finish_input().await,
                        None => Err(AudioPacketError::invalid(
                            "audio_stream_end requires an active audio stream",
                        )),
                    };
                    if let Err(error) = result {
                        let _ = tx.send(audio_packet_error_event(&error)).await;
                    }
                    continue;
                }
                if input.event_type == "audio_segment" {
                    let Ok(turn_capacity) = coordinator.try_reserve_turn_capacity() else {
                        let _ = try_enqueue_ws_control(
                            &control_tx,
                            StreamEvent::error(
                                "turn_capacity_full",
                                "too many voice turns are in flight",
                            ),
                        );
                        break;
                    };
                    let server_received_ms = Utc::now().timestamp_millis();
                    let trace = TurnTrace {
                        trace_id: input.trace_id.clone(),
                        base_ms: input.client_sent_ms.unwrap_or(server_received_ms),
                    };
                    let (conversation_id, turn_id) = coordinator.allocate(input.conversation_id);
                    drop(spawn_downstream_turn(run_audio_segment_to_channel(
                        state.clone(),
                        tx.clone(),
                        conversation_id,
                        turn_id,
                        coordinator.clone(),
                        turn_capacity,
                        owner.clone(),
                        input.audio,
                        trace,
                        audio_context.clone(),
                        device_id.clone(),
                    )));
                    continue;
                }
                if input.event_type == "interrupt_audio_segment" {
                    let server_received_ms = Utc::now().timestamp_millis();
                    let trace = TurnTrace {
                        trace_id: input.trace_id.clone(),
                        base_ms: input.client_sent_ms.unwrap_or(server_received_ms),
                    };
                    handle_interrupt_audio_segment(
                        &state,
                        &tx,
                        input.conversation_id,
                        input.audio,
                        trace,
                        audio_context.clone(),
                    )
                    .await;
                    continue;
                }
                if input.event_type == "audio_end" {
                    let _ = tx
                        .send(StreamEvent::error(
                            "bad_request",
                            "audio_end 已废弃，请发送 audio_segment",
                        ))
                        .await;
                    continue;
                }
                if let Some(text) = input.text {
                    if text.trim().is_empty() {
                        continue;
                    }
                    let Ok(turn_capacity) = coordinator.try_reserve_turn_capacity() else {
                        let _ = try_enqueue_ws_control(
                            &control_tx,
                            StreamEvent::error(
                                "turn_capacity_full",
                                "too many voice turns are in flight",
                            ),
                        );
                        break;
                    };
                    let server_received_ms = Utc::now().timestamp_millis();
                    let trace = input.trace_id.clone().map(|trace_id| TurnTrace {
                        trace_id: Some(trace_id),
                        base_ms: input.client_sent_ms.unwrap_or(server_received_ms),
                    });
                    let (conversation_id, turn_id) = coordinator.allocate(input.conversation_id);
                    if let Some(turn) = RecognizedTurn::new(
                        state.clone(),
                        tx.clone(),
                        conversation_id,
                        turn_id,
                        coordinator.clone(),
                        turn_capacity,
                        owner.clone(),
                        text,
                        trace,
                        audio_context.clone(),
                    ) {
                        turn.handoff();
                    }
                }
            }
            Message::Binary(_) => {
                let _ = tx
                    .send(StreamEvent::new(
                        "asr_partial",
                        json!({"text":"正在接收设备音频","device_id":device_id}),
                    ))
                    .await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    if let Err(error) = stop_live_asr_session(&mut live_asr).await {
        tracing::error!(error = %error, "failed to stop live IAT session");
    }
    drop(tx);
    drop(control_tx);
    let _ = writer.await;
}

async fn next_ws_output_event(
    control_rx: &mut mpsc::Receiver<StreamEvent>,
    reply_rx: &mut mpsc::Receiver<StreamEvent>,
) -> Option<StreamEvent> {
    tokio::select! {
        biased;
        Some(event) = control_rx.recv() => Some(event),
        Some(event) = reply_rx.recv() => Some(event),
        else => None,
    }
}

fn filter_ws_output_event(
    interrupted: &mut InterruptedOutputFilter,
    event: StreamEvent,
) -> Option<StreamEvent> {
    if event.event_type == "_turn_output_finished" {
        if let Some(turn_id) = event.turn_id.as_deref() {
            interrupted.finish(turn_id);
        }
        return None;
    }
    if event.event_type == "tts_interrupted"
        && matches!(
            event.payload.get("status").and_then(Value::as_str),
            Some("interrupted" | "already_interrupted" | "already_finished")
        )
    {
        if let Some(turn_id) = event.turn_id.clone() {
            interrupted.interrupt(turn_id);
        }
    }
    if event
        .turn_id
        .as_deref()
        .is_some_and(|turn_id| interrupted.suppresses(turn_id, event.event_type.as_str()))
    {
        return None;
    }
    Some(event)
}

fn try_enqueue_ws_control(tx: &mpsc::Sender<StreamEvent>, event: StreamEvent) -> bool {
    tx.try_send(event).is_ok()
}

#[allow(clippy::too_many_arguments)]
async fn run_audio_segment_to_channel(
    state: AppState,
    tx: mpsc::Sender<StreamEvent>,
    conversation_id: String,
    turn_id: String,
    coordinator: WsTurnCoordinator,
    turn_capacity: TurnCapacityPermit,
    owner: db::ConversationOwner,
    audio: Option<String>,
    trace: TurnTrace,
    audio_context: VoiceAudioContext,
    device_id: String,
) {
    let decoded = match decode_segment_audio_packet(audio.as_deref(), audio_context.audio.input) {
        Ok(decoded) => decoded,
        Err(error) => {
            let _ = tx.send(audio_packet_error_event(&error)).await;
            return;
        }
    };
    let audio_stats = (audio_context.audio.input.format == AudioFormat::Pcm)
        .then(|| pcm_stats(&decoded, audio_context.audio.input.sample_rate));
    emit_diagnostic(
        &state,
        Some(&conversation_id),
        None,
        Some(&trace),
        "audio_received",
        json!({
            "format": audio_context.audio.input.format.as_str(),
            "sample_rate": audio_context.audio.input.sample_rate.hz(),
            "bytes": decoded.len(),
            "audio_duration_ms": audio_stats.as_ref().map(|stats| stats.duration_ms),
            "rms": audio_stats.as_ref().map(|stats| stats.rms),
            "peak": audio_stats.as_ref().map(|stats| stats.peak)
        }),
    );
    let _ = tx
        .send(StreamEvent::new(
            "asr_partial",
            json!({"text":"已收到一句话音频，正在识别","device_id":device_id}),
        ))
        .await;
    let config = &audio_context.config;
    let text = if config.mock_providers {
        "我要买两瓶可乐和一瓶水".to_string()
    } else {
        match recognize_audio(
            config,
            &decoded,
            audio_context.audio.input,
            audio_context.iat_provider,
        )
        .await
        {
            Ok(text) => {
                tracing::info!(
                    target: "mjy_voice_shop_rs::asr",
                    text_len = text.chars().count(),
                    trace_id = ?trace.trace_id,
                    provider = iat_provider_name(audio_context.iat_provider),
                    format = audio_context.audio.input.format.as_str(),
                    sample_rate = audio_context.audio.input.sample_rate.hz(),
                    "iat recognition completed"
                );
                emit_diagnostic(
                    &state,
                    Some(&conversation_id),
                    None,
                    Some(&trace),
                    "asr_done",
                    json!({"text_len": text.chars().count()}),
                );
                text
            }
            Err(error) => {
                let raw_message = error.to_string();
                if audio_stats
                    .as_ref()
                    .is_some_and(|stats| should_suppress_empty_asr(stats.duration_ms, &raw_message))
                {
                    let _ = tx
                        .send(StreamEvent::new(
                            "asr_ignored",
                            json!({
                                "reason": "short_empty",
                                "duration_ms": audio_stats.as_ref().map(|stats| stats.duration_ms)
                            }),
                        ))
                        .await;
                    return;
                }
                let error_code = classify_iat_error(&error);
                if !emit_upstream_audio_rejection_evidence(
                    &state,
                    Some(&conversation_id),
                    Some(&trace),
                    &error,
                    AudioUpstreamDirection::Iat,
                    iat_provider_name(audio_context.iat_provider),
                    audio_context.audio.input,
                ) {
                    tracing::warn!(
                        target: "mjy_voice_shop_rs::asr",
                        code = error_code,
                        trace_id = ?trace.trace_id,
                        provider = iat_provider_name(audio_context.iat_provider),
                        format = audio_context.audio.input.format.as_str(),
                        sample_rate = audio_context.audio.input.sample_rate.hz(),
                        "iat recognition failed"
                    );
                }
                let _ = tx
                    .send(StreamEvent::error(
                        error_code,
                        &friendly_error_message(error_code, &raw_message),
                    ))
                    .await;
                return;
            }
        }
    };
    if let Some(turn) = RecognizedTurn::new(
        state,
        tx,
        conversation_id,
        turn_id,
        coordinator,
        turn_capacity,
        owner,
        text,
        Some(trace),
        audio_context,
    ) {
        turn.handoff();
    }
}

async fn handle_interrupt_audio_segment(
    state: &AppState,
    tx: &mpsc::Sender<StreamEvent>,
    conversation_id: Option<String>,
    audio: Option<String>,
    trace: TurnTrace,
    audio_context: VoiceAudioContext,
) {
    let decoded = match decode_segment_audio_packet(audio.as_deref(), audio_context.audio.input) {
        Ok(decoded) => decoded,
        Err(error) => {
            let _ = tx.send(audio_packet_error_event(&error)).await;
            return;
        }
    };
    let config = &audio_context.config;
    let recognized = if config.mock_providers {
        config.tts_interrupt_word.clone()
    } else {
        match recognize_audio(
            config,
            &decoded,
            audio_context.audio.input,
            audio_context.iat_provider,
        )
        .await
        {
            Ok(text) => text,
            Err(error) => {
                let code = classify_iat_error(&error);
                emit_upstream_audio_rejection_evidence(
                    state,
                    conversation_id.as_deref(),
                    Some(&trace),
                    &error,
                    AudioUpstreamDirection::Iat,
                    iat_provider_name(audio_context.iat_provider),
                    audio_context.audio.input,
                );
                if code != "asr_failed" {
                    let _ = tx.send(StreamEvent::error(code, &error.to_string())).await;
                    return;
                }
                let _ = tx
                    .send(StreamEvent::new(
                        "tts_interrupt_ignored",
                        json!({"reason": friendly_error_message("asr_failed", &error.to_string())}),
                    ))
                    .await;
                return;
            }
        }
    };
    let event_type = if is_interrupt_word_match(&recognized, &config.tts_interrupt_word) {
        "tts_interrupt_detected"
    } else {
        "tts_interrupt_ignored"
    };
    let _ = tx
        .send(StreamEvent::new(
            event_type,
            json!({"text": recognized, "word": config.tts_interrupt_word}),
        ))
        .await;
}

async fn handle_device_button_interrupt(
    tx: &mpsc::Sender<StreamEvent>,
    interrupts: &TurnInterruptRegistry,
    input: &WsInput,
) -> bool {
    let (Some(conversation_id), Some(turn_id), Some("button")) = (
        input.conversation_id.as_deref(),
        input.turn_id.as_deref(),
        input.source.as_deref(),
    ) else {
        return try_enqueue_ws_control(
            tx,
            StreamEvent::error(
                "bad_request",
                "tts_interrupt requires conversation_id, turn_id, and source=button",
            ),
        );
    };

    let status = interrupts.interrupt(conversation_id, turn_id).await;
    let status = match status {
        InterruptStatus::Interrupted => "interrupted",
        InterruptStatus::AlreadyInterrupted => "already_interrupted",
        InterruptStatus::AlreadyFinished => "already_finished",
        InterruptStatus::ConversationMismatch | InterruptStatus::UnknownTurn => {
            return try_enqueue_ws_control(
                tx,
                StreamEvent::error("bad_request", "unknown turn or conversation mismatch")
                    .with_context(conversation_id, turn_id),
            );
        }
    };
    try_enqueue_ws_control(
        tx,
        StreamEvent::new(
            "tts_interrupted",
            json!({"source":"button", "status":status}),
        )
        .with_context(conversation_id, turn_id),
    )
}

pub fn is_interrupt_word_match(text: &str, interrupt_word: &str) -> bool {
    let expected = normalize_interrupt_text(interrupt_word);
    !expected.is_empty() && normalize_interrupt_text(text) == expected
}

fn normalize_interrupt_text(text: &str) -> String {
    text.chars()
        .filter(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    '，' | '。' | '！' | '？' | ',' | '.' | '!' | '?' | ';' | '；' | ':' | '：'
                )
        })
        .collect()
}

async fn run_live_asr_to_channel(
    state: AppState,
    tx: mpsc::Sender<StreamEvent>,
    conversation_id: String,
    turn_id: String,
    coordinator: WsTurnCoordinator,
    turn_capacity: TurnCapacityPermit,
    owner: db::ConversationOwner,
    trace: TurnTrace,
    mut audio_rx: mpsc::Receiver<LiveAsrFrame>,
    audio_context: VoiceAudioContext,
) {
    let config = &audio_context.config;
    if config.mock_providers {
        if let Some(turn) = RecognizedTurn::new(
            state,
            tx,
            conversation_id,
            turn_id,
            coordinator,
            turn_capacity,
            owner,
            "我要买两瓶可乐和一瓶水".to_string(),
            Some(trace),
            audio_context,
        ) {
            turn.handoff();
        }
        return;
    }

    let uplink_state = state.clone();
    let uplink_conversation_id = conversation_id.clone();
    let uplink_trace = trace.clone();
    let upstream_session = async {
        let first_frame = audio_rx.recv().await.ok_or_else(|| {
            AudioPacketError::invalid("audio stream input closed before the first audio packet")
        })?;
        let mut writer_state = LiveIatWriterState::from_audio_context(&audio_context);
        let (first_payload, _, first_relay_started) = writer_state.handle_frame(first_frame)?;

        let signed_url = build_signed_ws_url(
            &config.iat_endpoint,
            &config.api_key,
            &config.api_secret,
            &current_rfc1123_date(),
        )?;
        let (socket, _) = connect_async(signed_url).await?;
        let (mut upstream_tx, mut upstream_rx) = socket.split();
        let writer = async move {
            upstream_tx
                .send(UpstreamMessage::Text(first_payload.to_string().into()))
                .await?;
            if let Some(relay_started) = first_relay_started {
                emit_audio_relay_diagnostic(
                    &uplink_state,
                    Some(&uplink_conversation_id),
                    None,
                    Some(&uplink_trace),
                    "voice_audio_uplink_relay_duration",
                    audio_context.audio.input,
                    iat_provider_name(audio_context.iat_provider),
                    relay_started,
                );
            }
            while let Some(frame) = audio_rx.recv().await {
                let (payload, is_end, relay_started) = writer_state.handle_frame(frame)?;
                upstream_tx
                    .send(UpstreamMessage::Text(payload.to_string().into()))
                    .await?;
                if let Some(relay_started) = relay_started {
                    emit_audio_relay_diagnostic(
                        &uplink_state,
                        Some(&uplink_conversation_id),
                        None,
                        Some(&uplink_trace),
                        "voice_audio_uplink_relay_duration",
                        audio_context.audio.input,
                        iat_provider_name(audio_context.iat_provider),
                        relay_started,
                    );
                }
                if is_end {
                    let duration_ms =
                        (audio_context.audio.input.format == AudioFormat::Pcm).then(|| {
                            pcm_duration_ms_from_bytes(
                                writer_state.audio_bytes,
                                audio_context.audio.input.sample_rate,
                            )
                        });
                    emit_diagnostic(
                        &uplink_state,
                        Some(&uplink_conversation_id),
                        None,
                        Some(&uplink_trace),
                        "audio_input_done",
                        json!({
                            "audio_duration_ms": duration_ms,
                            "bytes": writer_state.audio_bytes
                        }),
                    );
                    return anyhow::Result::<usize>::Ok(writer_state.audio_bytes);
                }
            }
            writer_state.channel_closed()
        };

        let reader = async {
            let mut recognized = String::new();
            while let Some(message) = upstream_rx.next().await {
                let message = message?;
                let UpstreamMessage::Text(raw) = message else {
                    continue;
                };
                let value: Value = serde_json::from_str(&raw)?;
                let parsed = parse_iat_text_for_provider(&value, audio_context.iat_provider)?;
                if !parsed.text.trim().is_empty() {
                    recognized = merge_iat_text(&recognized, &parsed.text);
                    if !parsed.is_final {
                        let _ = tx
                            .send(StreamEvent::new(
                                "asr_partial",
                                json!({"text": recognized, "mode": "streaming"}),
                            ))
                            .await;
                    }
                }
                if parsed.is_final {
                    break;
                }
            }
            anyhow::Result::<String>::Ok(recognized)
        };
        let (audio_bytes, recognized) = couple_live_iat_io(writer, reader).await?;
        anyhow::Result::<(String, usize)>::Ok((recognized, audio_bytes))
    };
    let result = match tokio::time::timeout(LIVE_IAT_SESSION_TIMEOUT, upstream_session).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "live IAT session timed out after 20 seconds"
        )),
    };

    match result {
        Ok((text, audio_bytes)) => {
            if text.trim().is_empty() {
                let duration_ms =
                    (audio_context.audio.input.format == AudioFormat::Pcm).then(|| {
                        pcm_duration_ms_from_bytes(
                            audio_bytes,
                            audio_context.audio.input.sample_rate,
                        )
                    });
                if duration_ms.is_some_and(|duration_ms| {
                    should_suppress_empty_asr(duration_ms, "IAT returned empty text")
                }) {
                    let duration_ms = duration_ms.unwrap_or_default();
                    let _ = tx
                        .send(StreamEvent::new(
                            "asr_ignored",
                            json!({"reason": "short_empty", "duration_ms": duration_ms}),
                        ))
                        .await;
                    return;
                }
                let _ = tx
                    .send(StreamEvent::error(
                        "asr_failed",
                        &friendly_error_message("asr_failed", "IAT returned empty text"),
                    ))
                    .await;
                return;
            }
            emit_diagnostic(
                &state,
                Some(&conversation_id),
                None,
                Some(&trace),
                "asr_done",
                json!({
                    "text_len": text.chars().count(),
                    "mode": "streaming",
                    "audio_duration_ms": (audio_context.audio.input.format == AudioFormat::Pcm)
                        .then(|| pcm_duration_ms_from_bytes(audio_bytes, audio_context.audio.input.sample_rate))
                }),
            );
            if let Some(turn) = RecognizedTurn::new(
                state,
                tx,
                conversation_id,
                turn_id,
                coordinator,
                turn_capacity,
                owner,
                text,
                Some(trace),
                audio_context,
            ) {
                turn.handoff();
            }
        }
        Err(error) => {
            let raw_message = error.to_string();
            let error_code = classify_iat_error(&error);
            if !emit_upstream_audio_rejection_evidence(
                &state,
                Some(&conversation_id),
                Some(&trace),
                &error,
                AudioUpstreamDirection::Iat,
                iat_provider_name(audio_context.iat_provider),
                audio_context.audio.input,
            ) {
                tracing::warn!(
                    target: "mjy_voice_shop_rs::asr",
                    code = error_code,
                    trace_id = ?trace.trace_id,
                    provider = iat_provider_name(audio_context.iat_provider),
                    format = audio_context.audio.input.format.as_str(),
                    sample_rate = audio_context.audio.input.sample_rate.hz(),
                    "streaming iat failed"
                );
            }
            let _ = tx
                .send(StreamEvent::error(
                    error_code,
                    &friendly_error_message(error_code, &raw_message),
                ))
                .await;
        }
    }
}

pub fn should_suppress_empty_asr(duration_ms: u64, raw_message: &str) -> bool {
    duration_ms < 900 && raw_message.contains("IAT returned empty text")
}

fn pcm_duration_ms_from_bytes(bytes: usize, sample_rate: AudioSampleRate) -> u64 {
    ((bytes as u64 / 2) * 1000) / u64::from(sample_rate.hz())
}

#[derive(Debug)]
struct PcmStats {
    duration_ms: u64,
    rms: f64,
    peak: f64,
}

fn pcm_stats(audio: &[u8], sample_rate: AudioSampleRate) -> PcmStats {
    let mut sum = 0.0;
    let mut peak = 0.0;
    let mut samples = 0usize;
    for chunk in audio.chunks_exact(2) {
        let value = i16::from_le_bytes([chunk[0], chunk[1]]) as f64 / i16::MAX as f64;
        sum += value * value;
        let abs = value.abs();
        if abs > peak {
            peak = abs;
        }
        samples += 1;
    }
    let rms = if samples == 0 {
        0.0
    } else {
        (sum / samples as f64).sqrt()
    };
    PcmStats {
        duration_ms: ((samples as f64 / f64::from(sample_rate.hz())) * 1000.0).round() as u64,
        rms,
        peak,
    }
}

async fn run_turn_to_channel(
    state: AppState,
    tx: mpsc::Sender<StreamEvent>,
    conversation_id: String,
    turn_id: String,
    coordinator: WsTurnCoordinator,
    turn_capacity: TurnCapacityPermit,
    analysis_ticket: AnalysisTicket,
    owner: db::ConversationOwner,
    text: String,
    trace: Option<TurnTrace>,
    audio_context: VoiceAudioContext,
) {
    let cancellation = match coordinator
        .interrupts
        .register(&conversation_id, &turn_id)
        .await
    {
        Ok(cancellation) => cancellation,
        Err(error) => {
            let _ = tx
                .send(
                    StreamEvent::error(
                        "bad_request",
                        &format!("turn interrupt registration failed: {error:?}"),
                    )
                    .with_context(&conversation_id, &turn_id),
                )
                .await;
            let _ = tx
                .send(
                    StreamEvent::new("_turn_output_finished", json!({}))
                        .with_context(&conversation_id, &turn_id),
                )
                .await;
            drop(turn_capacity);
            return;
        }
    };
    let turn_hold = turn_capacity.hold();
    let result = run_turn_with_interrupt(
        &state,
        &conversation_id,
        &turn_id,
        &owner,
        &text,
        trace,
        audio_context,
        cancellation,
        analysis_ticket,
        turn_hold,
        |event| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(event).await;
            }
        },
    )
    .await;
    coordinator
        .interrupts
        .finish(&conversation_id, &turn_id)
        .await;
    if let Err(error) = result {
        let _ = tx
            .send(
                StreamEvent::error("turn_failed", &error.message)
                    .with_context(&conversation_id, &turn_id),
            )
            .await;
    }
    let _ = tx
        .send(
            StreamEvent::new("_turn_output_finished", json!({}))
                .with_context(&conversation_id, &turn_id),
        )
        .await;
    drop(turn_capacity);
}

async fn run_turn<F, Fut>(
    state: &AppState,
    conversation_id: &str,
    turn_id: &str,
    owner: &db::ConversationOwner,
    user_text: &str,
    trace: Option<TurnTrace>,
    audio_context: VoiceAudioContext,
    emit: F,
) -> Result<(), ApiError>
where
    F: FnMut(StreamEvent) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (_cancellation_tx, cancellation) = watch::channel(false);
    let coordinator = WsTurnCoordinator::default();
    let _turn_capacity = coordinator
        .try_reserve_turn_capacity()
        .map_err(|_| anyhow::anyhow!("turn capacity is full"))?;
    let turn_hold = _turn_capacity.hold();
    let analysis_ticket = coordinator
        .try_reserve_analysis()
        .map_err(|_| anyhow::anyhow!("analysis queue is full"))?;
    run_turn_with_interrupt(
        state,
        conversation_id,
        turn_id,
        owner,
        user_text,
        trace,
        audio_context,
        cancellation,
        analysis_ticket,
        turn_hold,
        emit,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_with_interrupt<F, Fut>(
    state: &AppState,
    conversation_id: &str,
    turn_id: &str,
    owner: &db::ConversationOwner,
    user_text: &str,
    trace: Option<TurnTrace>,
    audio_context: VoiceAudioContext,
    mut cancellation: watch::Receiver<bool>,
    analysis_ticket: AnalysisTicket,
    turn_hold: TurnCapacityHold,
    mut emit: F,
) -> Result<(), ApiError>
where
    F: FnMut(StreamEvent) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let started = Utc::now().timestamp_millis();
    let prepared = prepare_analysis_turn(
        state,
        conversation_id,
        turn_id,
        owner,
        user_text,
        analysis_ticket,
    )
    .await?;
    emit_diagnostic(
        state,
        Some(conversation_id),
        Some(turn_id),
        trace.as_ref(),
        "turn_started",
        json!({"text_len": user_text.chars().count()}),
    );
    emit(StreamEvent::asr_final(user_text).with_context(conversation_id, turn_id)).await;

    let state_for_analysis = state.clone();
    let conversation_for_analysis = conversation_id.to_string();
    let turn_for_analysis = turn_id.to_string();
    let latest_text_for_analysis = user_text.to_string();
    let round_text_for_reply = prepared.round_text.clone();
    let analysis = tokio::spawn(async move {
        analyze_turn(
            &state_for_analysis,
            &conversation_for_analysis,
            &turn_for_analysis,
            &latest_text_for_analysis,
            &prepared.round_text,
        )
        .await
    });

    let reply_result: Result<(), ApiError> = async {
        // Business prompts/order settings remain hot-reloadable; audio provider settings come only
        // from the immutable connection snapshot in `audio_context`.
        let config = db::get_config(&state.pool).await?;
        let mut assistant_sentences = Vec::new();
        let direct_end_reply = direct_conversation_end_reply(user_text);
        if config.mock_providers || direct_end_reply.is_some() {
        let reply = direct_end_reply
            .map(ToString::to_string)
            .unwrap_or_else(|| mock_reply(&config, user_text));
        let mut sentence_buffer = String::new();
        for chunk in reply.chars().collect::<Vec<_>>().chunks(5) {
            if reply_cancelled(&cancellation) {
                break;
            }
            let delta = chunk.iter().collect::<String>();
            emit(
                StreamEvent::new("llm_delta", json!({"content": delta}))
                    .with_context(conversation_id, turn_id),
            )
            .await;
            tokio::task::yield_now().await;
            if reply_cancelled(&cancellation) {
                break;
            }
            for sentence in split_complete_sentences(&mut sentence_buffer, &delta) {
                assistant_sentences.push(sentence.clone());
                emit_reply_sentence(
                    state,
                    &mut emit,
                    conversation_id,
                    turn_id,
                    &sentence,
                    0,
                    &audio_context,
                    trace.as_ref(),
                    &mut cancellation,
                    &turn_hold,
                )
                .await?;
            }
        }
        if !reply_cancelled(&cancellation) && !sentence_buffer.trim().is_empty() {
            let sentence = sentence_buffer.trim().to_string();
            assistant_sentences.push(sentence.clone());
            emit_reply_sentence(
                state,
                &mut emit,
                conversation_id,
                turn_id,
                &sentence,
                0,
                &audio_context,
                trace.as_ref(),
                &mut cancellation,
                &turn_hold,
            )
            .await?;
        }
        } else {
        let mut messages = vec![ChatMessage::system(config.role_prompt.clone())];
        if let Some(prompt) =
            order_context_prompt(state, conversation_id, user_text, &round_text_for_reply).await?
        {
            messages.push(ChatMessage::system(prompt));
        }
        messages.push(ChatMessage::user(user_text.to_string()));
        let mut produced_reply;
        if uses_streaming_super_smart_tts(audio_context.tts_provider) {
            produced_reply = run_llm_with_streaming_super_smart_tts(
                state,
                &config,
                &mut emit,
                conversation_id,
                turn_id,
                messages,
                &audio_context,
                trace.as_ref(),
                &mut assistant_sentences,
                &mut cancellation,
                turn_hold.clone(),
            )
            .await?;
        } else {
            let (chat_tx, mut chat_rx) = mpsc::channel(32);
            let chat_task = tokio::spawn(stream_chat_chunks(config.clone(), messages, chat_tx));
            let (tts_tx, mut tts_rx) = mpsc::channel(64);
            let mut sentence_buffer = String::new();
            let mut seq = 0;
            let mut active_tts = 0usize;
            let mut llm_done = false;
            produced_reply = false;
            let mut saw_first_token = false;
            while !llm_done || active_tts > 0 {
                tokio::select! {
                    biased;
                    _ = cancellation.changed() => {
                        break;
                    }
                    maybe_chunk = chat_rx.recv(), if !llm_done => {
                        let Some(chunk) = maybe_chunk else {
                            llm_done = true;
                            continue;
                        };
                        let chunk = match chunk {
                            Ok(chunk) => chunk,
                            Err(error) => {
                                emit(
                                    StreamEvent::error("llm_failed", &error.to_string())
                                        .with_context(conversation_id, turn_id),
                                )
                                .await;
                                llm_done = true;
                                continue;
                            }
                        };
                        if chunk.content.is_empty() {
                            continue;
                        }
                        if !saw_first_token {
                            saw_first_token = true;
                            emit_diagnostic(
                                state,
                                Some(conversation_id),
                                Some(turn_id),
                                trace.as_ref(),
                                "llm_first_token",
                                json!({"chars": chunk.content.chars().count()}),
                            );
                        }
                        produced_reply = true;
                        emit(
                            StreamEvent::new("llm_delta", json!({"content": chunk.content}))
                                .with_context(conversation_id, turn_id),
                        )
                        .await;
                        for sentence in split_complete_sentences(&mut sentence_buffer, &chunk.content) {
                            assistant_sentences.push(sentence.clone());
                            emit_diagnostic(
                                state,
                                Some(conversation_id),
                                Some(turn_id),
                                trace.as_ref(),
                                "llm_sentence_ready",
                                json!({"seq": seq, "chars": sentence.chars().count()}),
                            );
                            queue_reply_sentence(
                                state,
                                &mut emit,
                                &tts_tx,
                                conversation_id,
                                turn_id,
                                &sentence,
                                seq,
                                audio_context.clone(),
                                trace.as_ref(),
                                cancellation.clone(),
                                turn_hold.clone(),
                            )
                            .await;
                            active_tts += 1;
                            seq += 1;
                        }
                    }
                    maybe_event = tts_rx.recv(), if active_tts > 0 => {
                        let Some(event) = maybe_event else {
                            active_tts = 0;
                            continue;
                        };
                        if event.event_type == "_tts_task_done" {
                            active_tts = active_tts.saturating_sub(1);
                        } else {
                            emit(event).await;
                        }
                    }
                }
            }
            if reply_cancelled(&cancellation) {
                chat_task.abort();
            }
            let _ = chat_task.await;
            if !reply_cancelled(&cancellation) && !sentence_buffer.trim().is_empty() {
                let sentence = sentence_buffer.trim().to_string();
                assistant_sentences.push(sentence.clone());
                emit_diagnostic(
                    state,
                    Some(conversation_id),
                    Some(turn_id),
                    trace.as_ref(),
                    "llm_sentence_ready",
                    json!({"seq": seq, "chars": sentence.chars().count()}),
                );
                queue_reply_sentence(
                    state,
                    &mut emit,
                    &tts_tx,
                    conversation_id,
                    turn_id,
                    &sentence,
                    seq,
                    audio_context.clone(),
                    trace.as_ref(),
                    cancellation.clone(),
                    turn_hold.clone(),
                )
                .await;
                active_tts += 1;
                while active_tts > 0 {
                    tokio::select! {
                        biased;
                        _ = cancellation.changed() => break,
                        maybe_event = tts_rx.recv() => {
                            let Some(event) = maybe_event else { break; };
                            if event.event_type == "_tts_task_done" {
                                active_tts = active_tts.saturating_sub(1);
                            } else {
                                emit(event).await;
                            }
                        }
                    }
                }
                produced_reply = true;
            }
        }
        if !reply_cancelled(&cancellation) && !produced_reply {
            let sentence = "我先帮你识别商品，命中结果会显示在右侧购物车。";
            assistant_sentences.push(sentence.to_string());
            emit_reply_sentence(
                state,
                &mut emit,
                conversation_id,
                turn_id,
                sentence,
                0,
                &audio_context,
                trace.as_ref(),
                &mut cancellation,
                &turn_hold,
            )
            .await?;
        }
        }
        if !reply_cancelled(&cancellation) && !assistant_sentences.is_empty() {
            db::append_conversation_message(
                &state.pool,
                conversation_id,
                turn_id,
                "assistant",
                &assistant_sentences.join("\n"),
            )
            .await?;
        }
        if !reply_cancelled(&cancellation) {
            emit(StreamEvent::new("voice_done", json!({})).with_context(conversation_id, turn_id))
                .await;
        }
        Ok(())
    }
    .await;

    let result =
        settle_turn_reply_and_analysis(analysis, reply_result, conversation_id, turn_id, &mut emit)
            .await;
    emit(
        StreamEvent::new(
            "latency_metrics",
            json!({"total_ms": Utc::now().timestamp_millis() - started}),
        )
        .with_context(conversation_id, turn_id),
    )
    .await;
    result
}

async fn settle_turn_reply_and_analysis<F, Fut>(
    analysis: tokio::task::JoinHandle<Result<Vec<StreamEvent>, ApiError>>,
    reply_result: Result<(), ApiError>,
    conversation_id: &str,
    turn_id: &str,
    emit: &mut F,
) -> Result<(), ApiError>
where
    F: FnMut(StreamEvent) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if let Ok(Ok(events)) = analysis.await {
        for event in events {
            emit(event.with_context(conversation_id, turn_id)).await;
        }
    }
    reply_result
}

async fn prepare_analysis_turn(
    state: &AppState,
    conversation_id: &str,
    turn_id: &str,
    owner: &db::ConversationOwner,
    user_text: &str,
    analysis_ticket: AnalysisTicket,
) -> Result<PreparedAnalysisTurn, ApiError> {
    let guard = analysis_ticket.acquire().await?;
    db::ensure_conversation_owned(&state.pool, conversation_id, owner).await?;
    db::append_conversation_message(&state.pool, conversation_id, turn_id, "user", user_text)
        .await?;
    let round_text = db::pending_order_user_text(&state.pool, conversation_id).await?;
    Ok(PreparedAnalysisTurn {
        _guard: guard,
        round_text,
    })
}

fn uses_streaming_super_smart_tts(provider: TtsProvider) -> bool {
    provider == TtsProvider::SuperSmart
}

fn reply_cancelled(cancellation: &watch::Receiver<bool>) -> bool {
    *cancellation.borrow()
}

fn track_tts_provider_lifetime(
    mut upstream: mpsc::Receiver<anyhow::Result<TimedTtsAudioChunk>>,
    provider_task: tokio::task::JoinHandle<()>,
    turn_hold: TurnCapacityHold,
) -> mpsc::Receiver<anyhow::Result<TimedTtsAudioChunk>> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let cancelled = loop {
            tokio::select! {
                biased;
                _ = tx.closed() => {
                    break true;
                }
                item = upstream.recv() => {
                    let Some(item) = item else { break false; };
                    if tx.send(item).await.is_err() {
                        break true;
                    }
                }
            }
        };
        drop(upstream);
        if cancelled {
            provider_task.abort();
        }
        let _ = provider_task.await;
        drop(turn_hold);
    });
    rx
}

async fn stream_tracked_audio_profile_chunks(
    config: AppConfig,
    text: String,
    profile: AudioProfile,
    provider: TtsProvider,
    turn_hold: TurnCapacityHold,
) -> mpsc::Receiver<anyhow::Result<TimedTtsAudioChunk>> {
    let (upstream, provider_task) =
        start_audio_profile_chunks(config, text, profile, provider).into_parts();
    track_tts_provider_lifetime(upstream, provider_task, turn_hold)
}

async fn stream_tracked_super_smart_tts_text_frames(
    config: AppConfig,
    text_rx: mpsc::Receiver<TtsTextFrame>,
    profile: AudioProfile,
    turn_hold: TurnCapacityHold,
) -> mpsc::Receiver<anyhow::Result<TimedTtsAudioChunk>> {
    let (upstream, provider_task) =
        start_super_smart_tts_text_frames_for_profile(config, text_rx, profile).into_parts();
    track_tts_provider_lifetime(upstream, provider_task, turn_hold)
}

async fn run_llm_with_streaming_super_smart_tts<F, Fut>(
    state: &AppState,
    config: &AppConfig,
    emit: &mut F,
    conversation_id: &str,
    turn_id: &str,
    messages: Vec<ChatMessage>,
    audio_context: &VoiceAudioContext,
    trace: Option<&TurnTrace>,
    assistant_sentences: &mut Vec<String>,
    cancellation: &mut watch::Receiver<bool>,
    turn_hold: TurnCapacityHold,
) -> Result<bool, ApiError>
where
    F: FnMut(StreamEvent) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (chat_tx, mut chat_rx) = mpsc::channel(32);
    let chat_task = tokio::spawn(stream_chat_chunks(config.clone(), messages, chat_tx));
    let (tts_text_tx, tts_text_rx) = mpsc::channel(32);
    let mut tts_audio_rx = tokio::select! {
        biased;
        _ = cancellation.changed() => {
            chat_task.abort();
            let _ = chat_task.await;
            return Ok(false);
        },
        receiver = stream_tracked_super_smart_tts_text_frames(
            audio_context.config.clone(),
            tts_text_rx,
            audio_context.audio.output,
            turn_hold,
        ) => receiver,
    };

    let mut assistant_text = String::new();
    let mut tts_text_buffer = String::new();
    let mut delayed_tts_text: Option<String> = None;
    let mut tts_text_seq = 0u32;
    let mut tts_started = false;
    let mut tts_input_closed = false;
    let mut tts_audio_done = false;
    let mut llm_done = false;
    let mut produced_reply = false;
    let mut saw_first_token = false;
    let mut saw_first_tts_chunk = false;
    let mut tts_chunk_count = 0usize;
    let mut tts_byte_count = 0usize;

    while !llm_done || !tts_input_closed || !tts_audio_done {
        if reply_cancelled(cancellation) {
            break;
        }
        if llm_done && !tts_input_closed {
            for fragment in drain_tts_text_fragments(&mut tts_text_buffer, true) {
                emit_diagnostic(
                    state,
                    Some(conversation_id),
                    Some(turn_id),
                    trace,
                    "llm_sentence_ready",
                    json!({"seq": tts_text_seq, "chars": fragment.chars().count(), "mode": "streaming_text"}),
                );
                emit(
                    StreamEvent::new("reply_sentence", json!({"text": fragment}))
                        .with_context(conversation_id, turn_id),
                )
                .await;
                if !tts_started {
                    emit_diagnostic(
                        state,
                        Some(conversation_id),
                        Some(turn_id),
                        trace,
                        "tts_start",
                        json!({"seq": 0, "mode": "streaming_text"}),
                    );
                    tts_started = true;
                    let _ = tts_text_tx
                        .send(TtsTextFrame {
                            text: fragment,
                            status: 0,
                            seq: tts_text_seq,
                        })
                        .await;
                    tts_text_seq += 1;
                    continue;
                }
                if let Some(previous) = delayed_tts_text.replace(fragment) {
                    let _ = tts_text_tx
                        .send(TtsTextFrame {
                            text: previous,
                            status: 1,
                            seq: tts_text_seq,
                        })
                        .await;
                    tts_text_seq += 1;
                }
            }
            if let Some(last) = delayed_tts_text.take() {
                let _ = tts_text_tx
                    .send(TtsTextFrame {
                        text: last,
                        status: 2,
                        seq: tts_text_seq,
                    })
                    .await;
                tts_started = true;
            } else if tts_started {
                let _ = tts_text_tx
                    .send(TtsTextFrame {
                        text: String::new(),
                        status: 2,
                        seq: tts_text_seq,
                    })
                    .await;
            } else if !tts_started {
                tts_audio_done = true;
            }
            tts_input_closed = true;
        }

        if llm_done && tts_input_closed && tts_audio_done {
            break;
        }

        tokio::select! {
            biased;
            _ = cancellation.changed() => break,
            maybe_chunk = chat_rx.recv(), if !llm_done => {
                let Some(chunk) = maybe_chunk else {
                    llm_done = true;
                    continue;
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        emit(
                            StreamEvent::error("llm_failed", &error.to_string())
                                .with_context(conversation_id, turn_id),
                        )
                        .await;
                        llm_done = true;
                        continue;
                    }
                };
                if chunk.content.is_empty() {
                    continue;
                }
                if !saw_first_token {
                    saw_first_token = true;
                    emit_diagnostic(
                        state,
                        Some(conversation_id),
                        Some(turn_id),
                        trace,
                        "llm_first_token",
                        json!({"chars": chunk.content.chars().count()}),
                    );
                }
                produced_reply = true;
                assistant_text.push_str(&chunk.content);
                tts_text_buffer.push_str(&chunk.content);
                emit(
                    StreamEvent::new("llm_delta", json!({"content": chunk.content}))
                        .with_context(conversation_id, turn_id),
                )
                .await;

                for fragment in drain_tts_text_fragments(&mut tts_text_buffer, false) {
                    emit_diagnostic(
                        state,
                        Some(conversation_id),
                        Some(turn_id),
                        trace,
                        "llm_sentence_ready",
                        json!({"seq": tts_text_seq, "chars": fragment.chars().count(), "mode": "streaming_text"}),
                    );
                    emit(
                        StreamEvent::new("reply_sentence", json!({"text": fragment}))
                            .with_context(conversation_id, turn_id),
                    )
                    .await;
                    if !tts_started {
                        emit_diagnostic(
                            state,
                            Some(conversation_id),
                            Some(turn_id),
                            trace,
                            "tts_start",
                            json!({"seq": 0, "mode": "streaming_text"}),
                        );
                        tts_started = true;
                        let _ = tts_text_tx
                            .send(TtsTextFrame {
                                text: fragment,
                                status: 0,
                                seq: tts_text_seq,
                            })
                            .await;
                        tts_text_seq += 1;
                        continue;
                    }
                    if let Some(previous) = delayed_tts_text.replace(fragment) {
                        let _ = tts_text_tx
                            .send(TtsTextFrame {
                                text: previous,
                                status: 1,
                                seq: tts_text_seq,
                            })
                            .await;
                        tts_text_seq += 1;
                    }
                }
                if chunk.is_final {
                    llm_done = true;
                }
            }
            maybe_audio = tts_audio_rx.recv(), if !tts_audio_done => {
                let Some(chunk) = maybe_audio else {
                    tts_audio_done = true;
                    continue;
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        let raw_message = error.to_string();
                        let code = classify_tts_error(&error);
                        emit_upstream_audio_rejection_evidence(
                            state,
                            Some(conversation_id),
                            trace,
                            &error,
                            AudioUpstreamDirection::Tts,
                            tts_provider_name(audio_context.tts_provider),
                            audio_context.audio.output,
                        );
                        emit(
                            StreamEvent::error(
                                code,
                                &friendly_error_message(code, &raw_message),
                            )
                            .with_context(conversation_id, turn_id),
                        )
                        .await;
                        tts_audio_done = true;
                        continue;
                    }
                };
                let relay_started = chunk.relay_started;
                if !saw_first_tts_chunk {
                    saw_first_tts_chunk = true;
                    emit_diagnostic(
                        state,
                        Some(conversation_id),
                        Some(turn_id),
                        trace,
                        "tts_first_chunk",
                        json!({"seq": 0, "bytes": chunk.audio.len(), "mode": "streaming_text"}),
                    );
                }
                tts_chunk_count += 1;
                tts_byte_count += chunk.audio.len();
                let is_last = chunk.is_last;
                emit(
                    StreamEvent::tts_audio_chunk(
                        STANDARD.encode(chunk.audio),
                        0,
                        is_last,
                        audio_context.audio.output,
                    )
                        .with_audio_relay_timing(
                            audio_context.audio.output,
                            audio_context.tts_provider,
                            relay_started,
                        )
                        .with_context(conversation_id, turn_id),
                )
                .await;
                if is_last {
                    emit_diagnostic(
                        state,
                        Some(conversation_id),
                        Some(turn_id),
                        trace,
                        "tts_done",
                        json!({"seq": 0, "chunks": tts_chunk_count, "bytes": tts_byte_count, "mode": "streaming_text"}),
                    );
                    tts_audio_done = true;
                }
            }
        }
    }

    let assistant_text = assistant_text.trim();
    if reply_cancelled(cancellation) {
        chat_task.abort();
    }
    let _ = chat_task.await;
    if !assistant_text.is_empty() {
        assistant_sentences.push(assistant_text.to_string());
    }
    Ok(produced_reply)
}

fn drain_tts_text_fragments(buffer: &mut String, force: bool) -> Vec<String> {
    let mut fragments = Vec::new();
    let mut last_boundary = 0usize;
    for (idx, ch) in buffer.char_indices() {
        let chars_before = buffer[..idx].chars().count();
        let is_strong = matches!(ch, '。' | '！' | '？' | '!' | '?' | '\n');
        let is_soft = matches!(ch, '，' | ',' | '；' | ';') && chars_before >= 8;
        if is_strong || is_soft {
            last_boundary = idx + ch.len_utf8();
        }
    }
    if last_boundary == 0 && buffer.chars().count() >= 18 {
        last_boundary = buffer
            .char_indices()
            .nth(17)
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(buffer.len());
    }
    if force && last_boundary == 0 && !buffer.trim().is_empty() {
        last_boundary = buffer.len();
    }
    if last_boundary == 0 {
        return fragments;
    }
    let drained = buffer.drain(..last_boundary).collect::<String>();
    let mut segment = String::new();
    for ch in drained.chars() {
        segment.push(ch);
        let segment_len = segment.chars().count();
        let is_strong = matches!(ch, '。' | '！' | '？' | '!' | '?' | '\n');
        let is_soft = matches!(ch, '，' | ',' | '；' | ';') && segment_len >= 9;
        if is_strong || is_soft {
            let text = segment.trim();
            if text.chars().count() >= 2 {
                fragments.push(text.to_string());
            }
            segment.clear();
        }
    }
    let text = segment.trim();
    if text.chars().count() >= 2 {
        fragments.push(text.to_string());
    }
    fragments
}

async fn emit_reply_sentence<F, Fut>(
    state: &AppState,
    emit: &mut F,
    conversation_id: &str,
    turn_id: &str,
    sentence: &str,
    seq: u32,
    audio_context: &VoiceAudioContext,
    trace: Option<&TurnTrace>,
    cancellation: &mut watch::Receiver<bool>,
    turn_hold: &TurnCapacityHold,
) -> Result<(), ApiError>
where
    F: FnMut(StreamEvent) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if reply_cancelled(cancellation) {
        return Ok(());
    }
    emit(
        StreamEvent::new("reply_sentence", json!({"text": sentence}))
            .with_context(conversation_id, turn_id),
    )
    .await;
    if reply_cancelled(cancellation) {
        return Ok(());
    }
    if audio_context.config.mock_providers {
        emit_diagnostic(
            state,
            Some(conversation_id),
            Some(turn_id),
            trace,
            "tts_done",
            json!({"seq": seq, "mock": true}),
        );
        emit(
            StreamEvent::tts_audio_chunk(
                mock_audio_chunk(sentence, audio_context.audio.output),
                seq,
                true,
                audio_context.audio.output,
            )
            .with_mock_audio()
            .with_context(conversation_id, turn_id),
        )
        .await;
        return Ok(());
    }
    emit_diagnostic(
        state,
        Some(conversation_id),
        Some(turn_id),
        trace,
        "tts_start",
        json!({"seq": seq, "chars": sentence.chars().count()}),
    );
    let mut chunk_rx = tokio::select! {
        biased;
        _ = cancellation.changed() => return Ok(()),
        receiver = stream_tracked_audio_profile_chunks(
            audio_context.config.clone(),
            sentence.to_string(),
            audio_context.audio.output,
            audio_context.tts_provider,
            turn_hold.clone(),
        ) => receiver,
    };
    let mut index = 0usize;
    let mut byte_count = 0usize;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.changed() => break,
            chunk = chunk_rx.recv() => chunk,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let raw_message = error.to_string();
                let code = classify_tts_error(&error);
                emit_upstream_audio_rejection_evidence(
                    state,
                    Some(conversation_id),
                    trace,
                    &error,
                    AudioUpstreamDirection::Tts,
                    tts_provider_name(audio_context.tts_provider),
                    audio_context.audio.output,
                );
                emit(
                    StreamEvent::error(code, &friendly_error_message(code, &raw_message))
                        .with_context(conversation_id, turn_id),
                )
                .await;
                return Ok(());
            }
        };
        let relay_started = chunk.relay_started;
        if index == 0 {
            emit_diagnostic(
                state,
                Some(conversation_id),
                Some(turn_id),
                trace,
                "tts_first_chunk",
                json!({"seq": seq, "bytes": chunk.audio.len()}),
            );
        }
        byte_count += chunk.audio.len();
        let is_last = chunk.is_last;
        emit(
            StreamEvent::tts_audio_chunk(
                STANDARD.encode(chunk.audio),
                seq,
                is_last,
                audio_context.audio.output,
            )
            .with_audio_relay_timing(
                audio_context.audio.output,
                audio_context.tts_provider,
                relay_started,
            )
            .with_context(conversation_id, turn_id),
        )
        .await;
        index += 1;
        if is_last {
            break;
        }
    }
    emit_diagnostic(
        state,
        Some(conversation_id),
        Some(turn_id),
        trace,
        "tts_done",
        json!({"seq": seq, "chunks": index, "bytes": byte_count}),
    );
    Ok(())
}

async fn queue_reply_sentence<F, Fut>(
    state: &AppState,
    emit: &mut F,
    tts_tx: &mpsc::Sender<StreamEvent>,
    conversation_id: &str,
    turn_id: &str,
    sentence: &str,
    seq: u32,
    audio_context: VoiceAudioContext,
    trace: Option<&TurnTrace>,
    cancellation: watch::Receiver<bool>,
    turn_hold: TurnCapacityHold,
) where
    F: FnMut(StreamEvent) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if reply_cancelled(&cancellation) {
        return;
    }
    emit(
        StreamEvent::new("reply_sentence", json!({"text": sentence}))
            .with_context(conversation_id, turn_id),
    )
    .await;
    emit_diagnostic(
        state,
        Some(conversation_id),
        Some(turn_id),
        trace,
        "tts_start",
        json!({"seq": seq, "chars": sentence.chars().count(), "parallel": true}),
    );
    tokio::spawn(send_tts_sentence_events(
        state.clone(),
        audio_context,
        tts_tx.clone(),
        conversation_id.to_string(),
        turn_id.to_string(),
        sentence.to_string(),
        seq,
        trace.cloned(),
        cancellation,
        turn_hold,
    ));
}

async fn send_tts_sentence_events(
    state: AppState,
    audio_context: VoiceAudioContext,
    tx: mpsc::Sender<StreamEvent>,
    conversation_id: String,
    turn_id: String,
    sentence: String,
    seq: u32,
    trace: Option<TurnTrace>,
    mut cancellation: watch::Receiver<bool>,
    turn_hold: TurnCapacityHold,
) {
    if reply_cancelled(&cancellation) {
        let _ = tx
            .send(
                StreamEvent::new("_tts_task_done", json!({"seq": seq}))
                    .with_context(&conversation_id, &turn_id),
            )
            .await;
        return;
    }
    if audio_context.config.mock_providers {
        emit_diagnostic(
            &state,
            Some(&conversation_id),
            Some(&turn_id),
            trace.as_ref(),
            "tts_done",
            json!({"seq": seq, "mock": true}),
        );
        let _ = tx
            .send(
                StreamEvent::tts_audio_chunk(
                    mock_audio_chunk(&sentence, audio_context.audio.output),
                    seq,
                    true,
                    audio_context.audio.output,
                )
                .with_mock_audio()
                .with_context(&conversation_id, &turn_id),
            )
            .await;
        let _ = tx
            .send(
                StreamEvent::new("_tts_task_done", json!({"seq": seq}))
                    .with_context(&conversation_id, &turn_id),
            )
            .await;
        return;
    }

    let mut chunk_rx = tokio::select! {
        biased;
        _ = cancellation.changed() => {
            let _ = tx.send(
                StreamEvent::new("_tts_task_done", json!({"seq": seq}))
                    .with_context(&conversation_id, &turn_id),
            ).await;
            return;
        }
        receiver = stream_tracked_audio_profile_chunks(
            audio_context.config.clone(),
            sentence,
            audio_context.audio.output,
            audio_context.tts_provider,
            turn_hold,
        ) => receiver,
    };
    let mut index = 0usize;
    let mut byte_count = 0usize;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancellation.changed() => break,
            chunk = chunk_rx.recv() => chunk,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let raw_message = error.to_string();
                let code = classify_tts_error(&error);
                emit_upstream_audio_rejection_evidence(
                    &state,
                    Some(&conversation_id),
                    trace.as_ref(),
                    &error,
                    AudioUpstreamDirection::Tts,
                    tts_provider_name(audio_context.tts_provider),
                    audio_context.audio.output,
                );
                let _ = tx
                    .send(
                        StreamEvent::error(code, &friendly_error_message(code, &raw_message))
                            .with_context(&conversation_id, &turn_id),
                    )
                    .await;
                let _ = tx
                    .send(
                        StreamEvent::new("_tts_task_done", json!({"seq": seq}))
                            .with_context(&conversation_id, &turn_id),
                    )
                    .await;
                return;
            }
        };
        let relay_started = chunk.relay_started;
        if index == 0 {
            emit_diagnostic(
                &state,
                Some(&conversation_id),
                Some(&turn_id),
                trace.as_ref(),
                "tts_first_chunk",
                json!({"seq": seq, "bytes": chunk.audio.len()}),
            );
        }
        byte_count += chunk.audio.len();
        let is_last = chunk.is_last;
        let _ = tx
            .send(
                StreamEvent::tts_audio_chunk(
                    STANDARD.encode(chunk.audio),
                    seq,
                    is_last,
                    audio_context.audio.output,
                )
                .with_audio_relay_timing(
                    audio_context.audio.output,
                    audio_context.tts_provider,
                    relay_started,
                )
                .with_context(&conversation_id, &turn_id),
            )
            .await;
        index += 1;
        if is_last {
            break;
        }
    }
    emit_diagnostic(
        &state,
        Some(&conversation_id),
        Some(&turn_id),
        trace.as_ref(),
        "tts_done",
        json!({"seq": seq, "chunks": index, "bytes": byte_count}),
    );
    let _ = tx
        .send(
            StreamEvent::new("_tts_task_done", json!({"seq": seq}))
                .with_context(&conversation_id, &turn_id),
        )
        .await;
}

async fn analyze_turn(
    state: &AppState,
    conversation_id: &str,
    turn_id: &str,
    latest_user_text: &str,
    round_user_text: &str,
) -> Result<Vec<StreamEvent>, ApiError> {
    let products = db::list_products(&state.pool).await?;
    let matches = match_products(round_user_text, &products);
    let active_order = latest_active_conversation_order(state, conversation_id).await?;
    let should_refund_order =
        active_order.is_some() && is_explicit_order_refund_intent(latest_user_text);
    let is_order_confirmation = is_order_confirmation_intent(latest_user_text);
    let should_end_conversation = !should_refund_order
        && !is_order_confirmation
        && is_conversation_end_intent(latest_user_text);
    let should_submit_order = !should_refund_order
        && !should_end_conversation
        && is_order_confirmation
        && !matches.is_empty();
    let mut events = vec![StreamEvent::new("analysis_started", json!({}))];
    let intent = if should_refund_order {
        "refund_order"
    } else if should_end_conversation {
        "end_conversation"
    } else if should_submit_order {
        "confirm_order"
    } else if matches.is_empty() {
        "chat"
    } else {
        "buy"
    };
    events.push(StreamEvent::new(
        "intent_analysis",
        json!({"intent": intent, "text": latest_user_text, "confidence": if matches.is_empty() {0.3} else {0.86}}),
    ));
    events.push(StreamEvent::new(
        "product_matches",
        json!({"items": matches}),
    ));
    if !matches.is_empty() && !should_refund_order && !should_end_conversation {
        events.push(StreamEvent::new(
            "order_draft",
            json!({
                "conversation_id": conversation_id,
                "turn_id": turn_id,
                "items": matches,
                "status": if should_submit_order { "submitting" } else { "awaiting_confirmation" },
                "display_name": "待下发订单"
            }),
        ));
    }
    if should_refund_order {
        if let Some(order) = active_order {
            events.push(StreamEvent::new(
                "order_refund_started",
                json!({
                    "conversation_id": conversation_id,
                    "turn_id": turn_id,
                    "saleOrderId": order.order_id.clone(),
                    "reason": latest_user_text
                }),
            ));
            let result = refund_submitted_order(
                state,
                conversation_id,
                &default_device_id(),
                Value::Null,
                &order.order_id,
                Some(latest_user_text.to_string()),
                Some(turn_id.to_string()),
            )
            .await;
            if result.get("ok").and_then(Value::as_bool) == Some(false) {
                let message = result
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("退单接口调用失败");
                events.push(StreamEvent::error("order_refund_failed", message));
            } else {
                events.push(StreamEvent::new("order_refunded", result));
                events.push(StreamEvent::new(
                    "conversation_ended",
                    json!({
                        "conversation_id": conversation_id,
                        "turn_id": turn_id,
                        "reason": "order_refund_completed"
                    }),
                ));
            }
        }
    }
    if should_end_conversation {
        events.push(StreamEvent::new(
            "conversation_ended",
            json!({
                "conversation_id": conversation_id,
                "turn_id": turn_id,
                "reason": "user_end_intent"
            }),
        ));
    }
    if should_submit_order {
        events.push(StreamEvent::new(
            "order_submit_started",
            json!({"conversation_id": conversation_id, "turn_id": turn_id}),
        ));
        let result = submit_order(
            state,
            conversation_id,
            &default_device_id(),
            Value::Null,
            &matches,
        )
        .await;
        if result.get("ok").and_then(Value::as_bool) == Some(false) {
            let message = result
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("订单接口调用失败");
            events.push(StreamEvent::error("order_failed", message));
        } else {
            events.push(StreamEvent::new("order_created", result));
        }
    }
    events.push(StreamEvent::new("analysis_done", json!({})));
    for event in &events {
        db::log_event(
            &state.pool,
            conversation_id,
            turn_id,
            &event.event_type,
            &event.payload,
        )
        .await;
    }
    Ok(events)
}

fn is_order_confirmation_intent(text: &str) -> bool {
    let normalized = normalize_interrupt_text(text);
    if normalized.is_empty() {
        return false;
    }
    let denied = [
        "不要下单",
        "别下单",
        "不用下单",
        "不下单",
        "先不下单",
        "不要下发",
        "别下发",
        "不用下发",
        "不下发",
        "先不下发",
        "取消下单",
        "取消下发",
    ];
    if denied.iter().any(|phrase| normalized.contains(phrase)) {
        return false;
    }

    let short_confirmations = [
        "下单",
        "下发",
        "确认",
        "确定",
        "可以",
        "可以的",
        "好",
        "好的",
        "对",
        "对的",
        "是的",
        "没错",
        "行",
        "嗯",
        "嗯嗯",
    ];
    if short_confirmations.contains(&normalized.as_str()) {
        return true;
    }

    let positive = [
        "确认下单",
        "确定下单",
        "可以下单",
        "直接下单",
        "帮我下单",
        "提交订单",
        "下单吧",
        "就这些",
        "就买这些",
        "确认购买",
        "确定购买",
        "下发订单",
        "帮我下发",
        "直接下发",
        "确认下发",
        "确定下发",
        "可以下发",
    ];
    positive.iter().any(|word| normalized.contains(word))
        || (normalized.contains("确认") && normalized.contains("下单"))
        || (normalized.contains("确定") && normalized.contains("下单"))
}

fn is_conversation_end_intent(text: &str) -> bool {
    let normalized = normalize_interrupt_text(text);
    if normalized.is_empty() {
        return false;
    }
    if is_order_confirmation_intent(text) {
        return false;
    }
    let end_words = [
        "结束对话",
        "结束聊天",
        "停止对话",
        "停止聊天",
        "不用了",
        "不要了",
        "没事了",
        "先这样",
        "就这样",
        "再见",
        "拜拜",
        "退出",
        "关闭",
        "退下",
        "推下",
        "退一下吧",
        "推一下吧",
    ];
    end_words.iter().any(|word| normalized.contains(word))
        || (normalized.contains("结束") && normalized.contains("对话"))
        || (normalized.contains("结束") && normalized.contains("聊天"))
}

fn direct_conversation_end_reply(text: &str) -> Option<&'static str> {
    (is_conversation_end_intent(text) && !is_explicit_order_refund_intent(text))
        .then_some("好的主人，我退下了。")
}

fn is_explicit_order_refund_intent(text: &str) -> bool {
    let normalized = normalize_interrupt_text(text);
    if normalized.is_empty() {
        return false;
    }
    let denied_phrases = [
        "不要退单",
        "别退单",
        "不用退单",
        "不退单",
        "不要退款",
        "别退款",
        "不用退款",
        "不退款",
        "不要取消订单",
        "别取消订单",
        "不用取消订单",
        "不取消订单",
    ];
    if denied_phrases
        .iter()
        .any(|phrase| normalized.contains(phrase))
    {
        return false;
    }

    let bare_phrases = ["退单", "退款", "取消订单"];
    if bare_phrases.iter().any(|phrase| normalized == *phrase) {
        return true;
    }

    let explicit_phrases = [
        "我要退单",
        "我想退单",
        "我需要退单",
        "请帮我退单",
        "帮我退单",
        "给我退单",
        "我要申请退单",
        "请帮我申请退单",
        "帮我申请退单",
        "我要退款",
        "我想退款",
        "我需要退款",
        "请帮我退款",
        "帮我退款",
        "给我退款",
        "我要申请退款",
        "请帮我申请退款",
        "帮我申请退款",
        "我要取消订单",
        "我想取消订单",
        "我需要取消订单",
        "请帮我取消订单",
        "帮我取消订单",
        "给我取消订单",
        "我要申请取消订单",
        "请帮我申请取消订单",
        "帮我申请取消订单",
    ];
    explicit_phrases
        .iter()
        .any(|phrase| normalized.contains(phrase))
}

#[derive(Debug, Clone)]
struct ActiveConversationOrder {
    order_id: String,
    payload: Value,
}

async fn latest_active_conversation_order(
    state: &AppState,
    conversation_id: &str,
) -> Result<Option<ActiveConversationOrder>, ApiError> {
    let orders = db::list_mock_order_payloads_by_conversation(&state.pool, conversation_id).await?;
    for order in orders {
        if is_closed_order_payload(&order.payload) {
            continue;
        }
        let order_id =
            order_id_from_payload(&order.payload).unwrap_or_else(|| order.order_id.clone());
        if !order_id.trim().is_empty() {
            return Ok(Some(ActiveConversationOrder {
                order_id,
                payload: order.payload,
            }));
        }
    }

    let events = db::list_conversation_events(&state.pool, conversation_id).await?;
    for event in events.into_iter().rev() {
        if event.event_type == "order_refunded" {
            return Ok(None);
        }
        if event.event_type == "order_created" {
            if is_closed_order_payload(&event.payload) {
                return Ok(None);
            }
            if let Some(order_id) = order_id_from_payload(&event.payload) {
                return Ok(Some(ActiveConversationOrder {
                    order_id,
                    payload: event.payload,
                }));
            }
        }
    }
    Ok(None)
}

fn order_id_from_payload(payload: &Value) -> Option<String> {
    let candidates = [
        payload.get("saleOrderId"),
        payload.get("order_id"),
        payload.get("orderId"),
        payload.get("data").and_then(|data| data.get("saleOrderId")),
        payload.get("data").and_then(|data| data.get("order_id")),
        payload.get("data").and_then(|data| data.get("orderId")),
    ];
    candidates
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn is_closed_order_payload(payload: &Value) -> bool {
    let status = payload
        .get("status")
        .or_else(|| payload.get("displayStatus"))
        .or_else(|| payload.get("data").and_then(|data| data.get("status")))
        .or_else(|| {
            payload
                .get("data")
                .and_then(|data| data.get("displayStatus"))
        })
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        status.as_str(),
        "refunded" | "refund" | "cancelled" | "canceled" | "closed"
    ) || status.contains("已退")
        || status.contains("已取消")
}

async fn order_context_prompt(
    state: &AppState,
    conversation_id: &str,
    latest_user_text: &str,
    round_user_text: &str,
) -> Result<Option<String>, ApiError> {
    let active_order = latest_active_conversation_order(state, conversation_id).await?;
    if let Some(order) = active_order.as_ref() {
        let order_id = order_id_from_payload(&order.payload).unwrap_or(order.order_id.clone());
        if is_explicit_order_refund_intent(latest_user_text) {
            return Ok(Some(format!(
                "本轮订单已经下发，订单号：{}。用户正在明确要求退单、退款或取消订单。请只用一句简短话术告知已为用户处理退单/取消，本轮对话结束；不要再次询问是否下单。",
                order_id
            )));
        }
    }

    let products = db::list_products(&state.pool).await?;
    let matches = match_products(round_user_text, &products);
    if !matches.is_empty() {
        return Ok(Some(format!(
            "当前已经识别到待下发订单：{}。这是用户在本对话中新发起的订单，不要与之前已下发的订单合并。如果用户还没有明确确认下单，请用一句话播报这些商品和数量，并询问是否确认下发订单；不要承诺已经下单。",
            summarize_order_items(&matches)
        )));
    }
    if let Some(order) = active_order {
        let order_id = order_id_from_payload(&order.payload).unwrap_or(order.order_id);
        return Ok(Some(format!(
            "本对话中最近一笔订单已经下发，订单号：{}。不要重复确认或下发该订单；用户仍可以继续购买商品并创建一笔新订单。只有用户明确说出退单、退款、取消订单或相关完整请求时才按退单处理。",
            order_id
        )));
    }
    Ok(None)
}

fn summarize_order_items(items: &[ProductMatch]) -> String {
    items
        .iter()
        .map(|item| format!("{} x {}（{}）", item.name, item.quantity, item.spec))
        .collect::<Vec<_>>()
        .join("、")
}

fn mock_reply(config: &AppConfig, user_text: &str) -> String {
    let products = ["可乐", "水", "矿泉水", "牛奶"];
    if is_explicit_order_refund_intent(user_text) {
        "好的，已为您处理退单，本轮对话结束。".to_string()
    } else if is_order_confirmation_intent(user_text) {
        "好的，收到确认，正在下发订单。".to_string()
    } else if is_conversation_end_intent(user_text) {
        "好的，本轮交互结束。".to_string()
    } else if products.iter().any(|name| user_text.contains(name)) {
        "好的，我先帮你整理待下发订单，确认后我再下发。".to_string()
    } else {
        let _ = config;
        "我在呢。你可以直接说想买什么，比如买两瓶可乐和一瓶水。".to_string()
    }
}

fn mock_audio_chunk(text: &str, profile: AudioProfile) -> String {
    match profile.format {
        AudioFormat::Mp3 => STANDARD.encode(format!("MOCK_MP3_STREAM:{text}").as_bytes()),
        AudioFormat::Pcm16k | AudioFormat::Pcm => {
            let duration_ms = (text.chars().count().max(1) * 80).max(320);
            let byte_count = profile.sample_rate.hz() as usize * duration_ms / 1_000 * 2;
            STANDARD.encode(vec![0; byte_count])
        }
        AudioFormat::Opus | AudioFormat::Speex => {
            // Test-only marker; this is not a decodable Opus or Speex payload.
            STANDARD.encode(
                format!("MOCK_{}_UNENCODED_MARKER:{text}", profile.format.as_str()).as_bytes(),
            )
        }
    }
}

pub fn classify_iat_error(error: &anyhow::Error) -> &'static str {
    for cause in error.chain() {
        if let Some(packet_error) = cause.downcast_ref::<AudioPacketError>() {
            return packet_error.code();
        }
        if cause
            .downcast_ref::<IatUpstreamError>()
            .is_some_and(IatUpstreamError::is_audio_profile_rejection)
        {
            return "upstream_audio_profile_rejected";
        }
    }
    "asr_failed"
}

pub fn classify_tts_error(error: &anyhow::Error) -> &'static str {
    for cause in error.chain() {
        if cause.downcast_ref::<TtsAudioProfileError>().is_some() {
            return "unsupported_audio_profile";
        }
        if cause
            .downcast_ref::<TtsUpstreamError>()
            .is_some_and(TtsUpstreamError::is_audio_profile_rejection)
        {
            return "upstream_audio_profile_rejected";
        }
    }
    "tts_failed"
}

pub fn friendly_error_message(code: &str, raw_message: &str) -> String {
    if code == "asr_failed" && raw_message.contains("live IAT session timed out") {
        return "语音识别超时，请再说一遍".to_string();
    }
    if code == "asr_failed" && raw_message.contains("IAT returned empty text") {
        return "没有识别到有效语音，请再说一遍".to_string();
    }
    if code == "tts_failed" {
        return format!("语音合成暂不可用，文字回复已保留：{raw_message}");
    }
    raw_message.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub conversation_id: Option<String>,
    pub turn_id: Option<String>,
    pub trace_id: Option<String>,
    pub stage: String,
    pub timestamp_ms: i64,
    pub elapsed_ms: Option<i64>,
    pub detail: Value,
}

fn emit_diagnostic(
    state: &AppState,
    conversation_id: Option<&str>,
    turn_id: Option<&str>,
    trace: Option<&TurnTrace>,
    stage: &str,
    detail: Value,
) {
    let now = Utc::now().timestamp_millis();
    let event = DiagnosticEvent {
        conversation_id: conversation_id.map(str::to_string),
        turn_id: turn_id.map(str::to_string),
        trace_id: trace.and_then(|trace| trace.trace_id.clone()),
        stage: stage.to_string(),
        timestamp_ms: now,
        elapsed_ms: trace.map(|trace| now - trace.base_ms),
        detail,
    };
    let _ = state.diagnostics.send(event);
}

#[derive(Clone, Copy)]
enum AudioUpstreamDirection {
    Iat,
    Tts,
}

impl AudioUpstreamDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Iat => "iat",
            Self::Tts => "tts",
        }
    }
}

fn emit_upstream_audio_rejection_evidence(
    state: &AppState,
    conversation_id: Option<&str>,
    trace: Option<&TurnTrace>,
    error: &anyhow::Error,
    direction: AudioUpstreamDirection,
    provider: &str,
    profile: AudioProfile,
) -> bool {
    let upstream_code = error.chain().find_map(|cause| match direction {
        AudioUpstreamDirection::Iat => cause
            .downcast_ref::<IatUpstreamError>()
            .filter(|error| error.is_audio_profile_rejection())
            .map(|error| error.code),
        AudioUpstreamDirection::Tts => cause
            .downcast_ref::<TtsUpstreamError>()
            .filter(|error| error.is_audio_profile_rejection())
            .map(|error| error.code),
    });
    let Some(upstream_code) = upstream_code else {
        return false;
    };
    let direction = direction.as_str();
    let service_code = "upstream_audio_profile_rejected";
    tracing::warn!(
        target: "mjy_voice_shop_rs::audio_upstream",
        direction,
        provider,
        format = profile.format.as_str(),
        sample_rate = profile.sample_rate.hz(),
        service_code,
        upstream_code,
        conversation_id,
        trace_id = trace.and_then(|trace| trace.trace_id.as_deref()),
        "upstream audio profile rejected"
    );
    emit_diagnostic(
        state,
        conversation_id,
        None,
        trace,
        service_code,
        json!({
            "direction": direction,
            "provider": provider,
            "format": profile.format.as_str(),
            "sample_rate": profile.sample_rate.hz(),
            "service_code": service_code,
            "upstream_code": upstream_code
        }),
    );
    true
}

fn emit_audio_relay_diagnostic(
    state: &AppState,
    conversation_id: Option<&str>,
    turn_id: Option<&str>,
    trace: Option<&TurnTrace>,
    stage: &str,
    profile: AudioProfile,
    provider: &str,
    started: Instant,
) {
    emit_diagnostic(
        state,
        conversation_id,
        turn_id,
        trace,
        stage,
        json!({
            "format": profile.format.as_str(),
            "sample_rate": profile.sample_rate.hz(),
            "provider": provider,
            "duration_micros": started.elapsed().as_micros() as u64
        }),
    );
}

async fn send_client_event<F, Fut, E>(
    state: &AppState,
    event: StreamEvent,
    send: F,
) -> Result<(), E>
where
    F: FnOnce(Message) -> Fut,
    Fut: Future<Output = Result<(), E>>,
{
    let conversation_id = event.conversation_id.clone();
    let turn_id = event.turn_id.clone();
    let relay_metric = event.relay_metric_context.clone();
    let serialized = serde_json::to_string(&event).expect("stream event must serialize");
    send(Message::Text(serialized.into())).await?;
    if let Some(relay_metric) = relay_metric {
        emit_audio_relay_diagnostic(
            state,
            conversation_id.as_deref(),
            turn_id.as_deref(),
            None,
            match relay_metric.direction {
                AudioRelayDirection::Downlink => "voice_audio_downlink_relay_duration",
            },
            relay_metric.profile,
            tts_provider_name(relay_metric.provider),
            relay_metric.started,
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEvent {
    pub conversation_id: Option<String>,
    pub turn_id: Option<String>,
    pub event_type: String,
    pub timestamp_ms: i64,
    pub payload: Value,
    #[serde(skip)]
    relay_metric_context: Option<RelayMetricContext>,
}

#[derive(Debug, Clone)]
struct RelayMetricContext {
    profile: AudioProfile,
    provider: TtsProvider,
    direction: AudioRelayDirection,
    started: Instant,
}

#[derive(Debug, Clone, Copy)]
enum AudioRelayDirection {
    Downlink,
}

impl StreamEvent {
    pub fn new(event_type: &str, payload: Value) -> Self {
        Self {
            conversation_id: None,
            turn_id: None,
            event_type: event_type.to_string(),
            timestamp_ms: Utc::now().timestamp_millis(),
            payload,
            relay_metric_context: None,
        }
    }

    pub fn asr_final(text: &str) -> Self {
        Self::new("asr_final", json!({"text": text}))
    }

    pub fn tts_audio_chunk(
        audio_base64: String,
        seq: u32,
        is_last: bool,
        profile: AudioProfile,
    ) -> Self {
        let mut payload = json!({
            "audio": audio_base64,
            "seq": seq,
            "format": profile.format.as_str(),
            "sample_rate": profile.sample_rate.hz(),
            "channels": profile.channels(),
            "is_last": is_last
        });
        if let Some(bit_depth) = profile.bit_depth() {
            payload["bit_depth"] = json!(bit_depth);
        }
        Self::new("tts_audio_chunk", payload)
    }

    pub fn error(code: &str, message: &str) -> Self {
        Self::new("error", json!({"code": code, "message": message}))
    }

    fn with_audio_relay_timing(
        mut self,
        profile: AudioProfile,
        provider: TtsProvider,
        started: Instant,
    ) -> Self {
        self.relay_metric_context = Some(RelayMetricContext {
            profile,
            provider,
            direction: AudioRelayDirection::Downlink,
            started,
        });
        self
    }

    fn with_mock_audio(mut self) -> Self {
        self.payload["mock"] = json!(true);
        self
    }

    pub fn with_context(mut self, conversation_id: &str, turn_id: &str) -> Self {
        self.conversation_id = Some(conversation_id.to_string());
        self.turn_id = Some(turn_id.to_string());
        self
    }
}

#[derive(Debug, Deserialize)]
struct DeviceAuthRequest {
    device_id: String,
    device_secret: String,
}

async fn device_auth(
    State(state): State<AppState>,
    connect_info: ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<DeviceAuthRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.device_id == LOCAL_DEMO_DEVICE_ID
        && !is_trusted_internal_source(Some(connect_info), &headers)
    {
        return Err(ApiError::unauthorized("设备鉴权失败"));
    }
    let row = sqlx::query("SELECT secret_hash, enabled FROM devices WHERE device_id = ?")
        .bind(&req.device_id)
        .fetch_optional(&state.pool)
        .await?;
    let Some(row) = row else {
        return Err(ApiError::unauthorized("设备不存在"));
    };
    let stored_hash: String = sqlx::Row::get(&row, "secret_hash");
    let enabled: i64 = sqlx::Row::get(&row, "enabled");
    if enabled != 1 || stored_hash != secret_hash(&req.device_secret) {
        return Err(ApiError::unauthorized("设备鉴权失败"));
    }
    let exp = Utc::now().timestamp() + 3600;
    let token = issue_device_token(&req.device_id, &state.server_secret, exp)?;
    Ok(Json(json!({"token": token, "expires_at": exp})))
}

async fn device_status(Json(payload): Json<Value>) -> Json<Value> {
    Json(json!({"ok": true, "received": payload, "server_time": Utc::now().to_rfc3339()}))
}

async fn device_config(State(state): State<AppState>) -> axum::response::Response {
    let config = match db::get_config(&state.pool).await {
        Ok(config) => config,
        Err(error) => return config_error_response(error),
    };
    let iat_provider = match IatProvider::parse(&config.iat_provider) {
        Ok(provider) => provider,
        Err(error) => return config_error_response(error),
    };
    let tts_provider = match TtsProvider::parse(&config.tts_provider) {
        Ok(provider) => provider,
        Err(error) => return config_error_response(error),
    };
    Json(json!({
        "audio_profiles": {
            "input": {
                "default": {"format": "mp3", "sample_rate": 16000},
                "supported": grouped_audio_profiles(supported_iat_profiles(iat_provider))
            },
            "output": {
                "default": {"format": "mp3", "sample_rate": 16000},
                "supported": grouped_audio_profiles(supported_tts_profiles(tts_provider))
            },
            "query": ["in_format", "in_rate", "out_format", "out_rate"],
            "pcm": {
                "bit_depth": 16,
                "channels": 1,
                "endianness": "little"
            },
            "packetized": {
                "opus": {
                    "frame_duration_ms": 20,
                    "one_packet_per_chunk": true
                },
                "speex": {
                    "frame_duration_ms": 20,
                    "one_packet_per_chunk": true
                }
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
            "client_events": [
                "text",
                "audio_stream_start",
                "audio_stream_chunk",
                "audio_stream_end",
                "audio_segment",
                "tts_interrupt"
            ],
            "server_events": [
                "asr_partial",
                "asr_final",
                "llm_delta",
                "reply_sentence",
                "tts_audio_chunk",
                "order_draft",
                "order_created",
                "tts_interrupted",
                "conversation_ended",
                "voice_done",
                "error"
            ]
        },
        "heartbeat_interval_ms": 15000
    }))
    .into_response()
}

async fn miniprogram_c_interfaces() -> Json<Value> {
    Json(json!({
        "source": {
            "name": "Apifox BeCoCo",
            "note": "当前可访问沉淀中，小程序C端订单已确认两个读取接口；原分享页如需密码，需用本地沉淀校准。",
            "doc_path": "订单接口理解沉淀.md"
        },
        "required_headers": miniprogram_required_headers(),
        "interfaces": [
            {
                "id": "get-user-sale-orders",
                "name": "查询用户订单列表",
                "method": "GET",
                "path": "/app-catering/api/app/saleorder/get-user-sale-orders",
                "mock_path": "/mock/app-catering/api/app/saleorder/get-user-sale-orders",
                "description": "按小程序用户上下文查询订单分页列表，适合查最近订单、履约中订单。",
                "default_query": {
                    "srcChannel": "2",
                    "status": "102",
                    "pageIndex": "1",
                    "pageSize": "3",
                    "sorting": "creationTime desc"
                },
                "response_focus": ["data.items", "displayStatus", "isCancellable", "isRefundable", "goodses"]
            },
            {
                "id": "get-user-sale-order-detail",
                "name": "查询用户订单详情",
                "method": "GET",
                "path": "/app-catering/api/app/saleorder/get-user-sale-order-detail",
                "mock_path": "/mock/app-catering/api/app/saleorder/get-user-sale-order-detail",
                "description": "根据 saleOrderId 查询单笔订单详情，适合播报状态、商品、金额、提货码和售后能力。",
                "default_query": {
                    "srcChannel": "2",
                    "saleOrderId": "mock-sale-order-001"
                },
                "response_focus": ["data.displayStatus", "data.statusDesc", "data.pickGoodsType", "data.goodses", "data.pickCode"]
            },
            {
                "id": "create-order",
                "name": "创建订单",
                "method": "POST",
                "path": "/app-catering/api/app/saleorder/create-order",
                "mock_path": "/mock/app-catering/api/app/saleorder/create-order",
                "path_status": "待 Apifox 确认",
                "description": "参考订单详情字段预置下单 mock，用于先验证玩偶下单编排；真实路径和字段待 Apifox 补充后替换。",
                "default_query": {"srcChannel": "2"},
                "default_body": {
                    "storeId": "999006940",
                    "storeNo": "6634",
                    "pickGoodsType": 2,
                    "remark": "少冰",
                    "goodses": [
                        {"goodsId": "cola-500", "goodsName": "可口可乐", "spec": "500ml", "qty": 2, "salePrice": 3.5},
                        {"goodsId": "water-555", "goodsName": "怡宝矿泉水", "spec": "555ml", "qty": 1, "salePrice": 2.5}
                    ]
                },
                "response_focus": ["data.saleOrderId", "data.orderNo", "data.displayStatus", "data.payAmt"]
            },
            {
                "id": "cancel-sale-order",
                "name": "取消订单",
                "method": "POST",
                "path": "/app-catering/api/app/saleorder/cancel-sale-order",
                "mock_path": "/mock/app-catering/api/app/saleorder/cancel-sale-order",
                "path_status": "待 Apifox 确认",
                "description": "参考详情中的 isCancellable/statusDesc 预置取消 mock，用于验证取消订单交互分支。",
                "default_query": {"srcChannel": "2"},
                "default_body": {
                    "saleOrderId": "mock-sale-order-001",
                    "cancelReason": "用户语音确认取消"
                },
                "response_focus": ["data.saleOrderId", "data.displayStatus", "data.statusDesc"]
            },
            {
                "id": "pay-order",
                "name": "发起支付",
                "method": "POST",
                "path": "/app-catering/api/app/saleorder/pay-order",
                "mock_path": "/mock/app-catering/api/app/saleorder/pay-order",
                "path_status": "待 Apifox 确认",
                "description": "预置小程序支付参数 mock，用于验证订单创建后进入支付态的调试链路。",
                "default_query": {"srcChannel": "2"},
                "default_body": {
                    "saleOrderId": "mock-sale-order-001",
                    "payType": 1,
                    "openId": "mock-openid"
                },
                "response_focus": ["data.saleOrderId", "data.payStatus", "data.payment.prepayId"]
            },
            {
                "id": "apply-refund",
                "name": "申请退款",
                "method": "POST",
                "path": "/app-catering/api/app/saleorder/apply-refund",
                "mock_path": "/mock/app-catering/api/app/saleorder/apply-refund",
                "path_status": "待 Apifox 确认",
                "description": "参考详情中的 isRefundable/refundStatus/reviewStatus 预置退款申请 mock，用于验证售后入口。",
                "default_query": {"srcChannel": "2"},
                "default_body": {
                    "saleOrderId": "mock-sale-order-003",
                    "refundReason": "用户语音申请退款",
                    "refundAmt": 16.8
                },
                "response_focus": ["data.refundId", "data.refundStatus", "data.reviewStatus"]
            }
        ],
        "missing_interfaces": [
            {
                "name": "创建订单",
                "reason": "已先按预置 mock 形式补充调试入口；真实路径和字段待 Apifox 补充后替换。"
            },
            {
                "name": "取消订单",
                "reason": "已先按预置 mock 形式补充调试入口；真实取消接口需确认。"
            },
            {
                "name": "支付/退款申请",
                "reason": "已拆为发起支付和申请退款两个 mock 调试入口；真实接口待确认。"
            }
        ]
    }))
}

#[derive(Debug, Deserialize)]
struct MiniprogramDebugCall {
    interface_id: String,
    #[serde(default)]
    query: std::collections::HashMap<String, String>,
    #[serde(default)]
    body: Value,
    #[serde(default)]
    headers: std::collections::HashMap<String, String>,
}

async fn miniprogram_c_debug_call(
    Json(req): Json<MiniprogramDebugCall>,
) -> Result<Json<Value>, ApiError> {
    let response = match req.interface_id.as_str() {
        "get-user-sale-orders" => mock_order_list_payload(&req.query, &req.headers),
        "get-user-sale-order-detail" => mock_order_detail_payload(&req.query, &req.headers),
        "create-order" => mock_create_order_payload(&req.query, &req.headers, &req.body),
        "cancel-sale-order" => mock_cancel_order_payload(&req.query, &req.headers, &req.body),
        "pay-order" => mock_pay_order_payload(&req.query, &req.headers, &req.body),
        "apply-refund" => mock_apply_refund_payload(&req.query, &req.headers, &req.body),
        _ => {
            return Ok(Json(json!({
                "ok": false,
                "code": "UNKNOWN_INTERFACE",
                "message": "未知小程序C端接口",
                "interface_id": req.interface_id
            })));
        }
    };
    Ok(Json(json!({
        "ok": true,
        "interface_id": req.interface_id,
        "request": {
            "query": req.query,
            "body": req.body,
            "headers": req.headers
        },
        "response": response
    })))
}

async fn mock_miniprogram_order_list(
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Json<Value> {
    Json(mock_order_list_payload(
        &query,
        &headers_to_debug_map(&headers),
    ))
}

async fn mock_miniprogram_order_detail(
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Json<Value> {
    Json(mock_order_detail_payload(
        &query,
        &headers_to_debug_map(&headers),
    ))
}

async fn mock_miniprogram_create_order(
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    Json(mock_create_order_payload(
        &query,
        &headers_to_debug_map(&headers),
        &body,
    ))
}

async fn mock_miniprogram_cancel_order(
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    Json(mock_cancel_order_payload(
        &query,
        &headers_to_debug_map(&headers),
        &body,
    ))
}

async fn mock_miniprogram_pay_order(
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    Json(mock_pay_order_payload(
        &query,
        &headers_to_debug_map(&headers),
        &body,
    ))
}

async fn mock_miniprogram_apply_refund(
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    Json(mock_apply_refund_payload(
        &query,
        &headers_to_debug_map(&headers),
        &body,
    ))
}

fn miniprogram_required_headers() -> Vec<&'static str> {
    vec![
        "__app",
        "__appver",
        "__company",
        "__store",
        "__storeno",
        "__src_channel",
        "CompanyCode",
        "Authorization",
        "debug",
    ]
}

fn headers_to_debug_map(headers: &HeaderMap) -> std::collections::HashMap<String, String> {
    miniprogram_required_headers()
        .into_iter()
        .filter_map(|key| {
            headers
                .get(key)
                .and_then(|value| value.to_str().ok())
                .map(|value| (key.to_string(), value.to_string()))
        })
        .collect()
}

fn missing_miniprogram_headers(
    headers: &std::collections::HashMap<String, String>,
) -> Vec<&'static str> {
    miniprogram_required_headers()
        .into_iter()
        .filter(|key| {
            headers
                .get(*key)
                .or_else(|| headers.get(&key.to_ascii_lowercase()))
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
        })
        .collect()
}

fn mock_order_list_payload(
    query: &std::collections::HashMap<String, String>,
    headers: &std::collections::HashMap<String, String>,
) -> Value {
    let page_index = query
        .get("pageIndex")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let page_size = query
        .get("pageSize")
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(3)
        .clamp(1, 20);
    let mut items = vec![
        mock_order_summary(
            "mock-sale-order-001",
            "SO2026071000012345",
            "配送中",
            205,
            0,
            2,
        ),
        mock_order_summary(
            "mock-sale-order-002",
            "SO2026070900098765",
            "待取餐",
            0,
            204,
            1,
        ),
        mock_order_summary(
            "mock-sale-order-003",
            "SO2026070800088888",
            "已完成",
            207,
            207,
            3,
        ),
    ];
    if let Some(status) = query.get("status").map(String::as_str) {
        if status == "102" {
            items.truncate(2);
        } else if status == "103" {
            items = items
                .into_iter()
                .filter(|item| item["displayStatus"] == "已完成")
                .collect();
        } else if status == "104" {
            items.clear();
        }
    }
    if let Some(filter) = query.get("filter").filter(|value| !value.trim().is_empty()) {
        items.retain(|item| {
            item["orderNo"].as_str().unwrap_or("").contains(filter)
                || item["goodses"]
                    .as_array()
                    .unwrap_or(&Vec::new())
                    .iter()
                    .any(|goods| goods["goodsName"].as_str().unwrap_or("").contains(filter))
        });
    }
    let total_count = items.len() as i64;
    let page_count = ((total_count as f64) / (page_size as f64)).ceil().max(1.0) as i64;
    let start = ((page_index - 1) * page_size) as usize;
    let end = (start + page_size as usize).min(items.len());
    let paged_items = if start < items.len() {
        items[start..end].to_vec()
    } else {
        Vec::new()
    };
    json!({
        "code": 0,
        "msg": "mock",
        "data": {
            "pageIndex": page_index,
            "pageSize": page_size,
            "pageCount": page_count,
            "totalCount": total_count,
            "items": paged_items
        },
        "error": null,
        "_debug": {
            "mock": true,
            "interface": "get-user-sale-orders",
            "query": query,
            "missingHeaders": missing_miniprogram_headers(headers)
        }
    })
}

fn mock_order_detail_payload(
    query: &std::collections::HashMap<String, String>,
    headers: &std::collections::HashMap<String, String>,
) -> Value {
    let sale_order_id = query
        .get("saleOrderId")
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| "mock-sale-order-001".to_string());
    let order = match sale_order_id.as_str() {
        "mock-sale-order-002" => {
            mock_order_detail(&sale_order_id, "SO2026070900098765", "待取餐", 0, 204, 1)
        }
        "mock-sale-order-003" => {
            mock_order_detail(&sale_order_id, "SO2026070800088888", "已完成", 207, 207, 3)
        }
        _ => mock_order_detail(&sale_order_id, "SO2026071000012345", "配送中", 205, 0, 2),
    };
    json!({
        "code": 0,
        "msg": "mock",
        "data": order,
        "error": null,
        "_debug": {
            "mock": true,
            "interface": "get-user-sale-order-detail",
            "query": query,
            "missingHeaders": missing_miniprogram_headers(headers)
        }
    })
}

fn mock_create_order_payload(
    query: &std::collections::HashMap<String, String>,
    headers: &std::collections::HashMap<String, String>,
    body: &Value,
) -> Value {
    let goodses = body
        .get("goodses")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(mock_goodses);
    let total_qty: f64 = goodses
        .iter()
        .filter_map(|item| item.get("qty").and_then(Value::as_f64))
        .sum();
    let pay_amt: f64 = goodses
        .iter()
        .map(|item| {
            item.get("qty").and_then(Value::as_f64).unwrap_or(1.0)
                * item.get("salePrice").and_then(Value::as_f64).unwrap_or(0.0)
        })
        .sum();
    json!({
        "code": 0,
        "msg": "mock",
        "data": {
            "saleOrderId": "mock-created-order-001",
            "orderId": "mock-created-order-001",
            "orderNo": "SO2026071000099999",
            "displayStatus": "待支付",
            "orderStatusForAppletCenter": 101,
            "orderStatusForAppletDetail": 201,
            "storeId": body.get("storeId").cloned().unwrap_or_else(|| json!("999006940")),
            "storeNo": body.get("storeNo").cloned().unwrap_or_else(|| json!("6634")),
            "pickGoodsType": body.get("pickGoodsType").cloned().unwrap_or_else(|| json!(2)),
            "totalQty": total_qty,
            "totalAmt": pay_amt,
            "payAmt": pay_amt,
            "goodses": goodses,
            "creationTime": "2026-07-10 13:30:00",
            "expireTime": "2026-07-10 13:45:00"
        },
        "error": null,
        "_debug": {
            "mock": true,
            "mockOnly": true,
            "interface": "create-order",
            "pathStatus": "待 Apifox 确认",
            "query": query,
            "body": body,
            "missingHeaders": missing_miniprogram_headers(headers)
        }
    })
}

fn mock_cancel_order_payload(
    query: &std::collections::HashMap<String, String>,
    headers: &std::collections::HashMap<String, String>,
    body: &Value,
) -> Value {
    let sale_order_id = body
        .get("saleOrderId")
        .or_else(|| body.get("orderId"))
        .and_then(Value::as_str)
        .unwrap_or("mock-sale-order-001");
    json!({
        "code": 0,
        "msg": "mock",
        "data": {
            "saleOrderId": sale_order_id,
            "orderId": sale_order_id,
            "displayStatus": "已取消",
            "orderStatusForAppletCenter": 104,
            "orderStatusForAppletDetail": 208,
            "statusDesc": "用户取消",
            "cancelReason": body.get("cancelReason").cloned().unwrap_or_else(|| json!("用户取消")),
            "cancelTime": "2026-07-10 13:36:00",
            "isCancellable": false,
            "isRefundable": false
        },
        "error": null,
        "_debug": {
            "mock": true,
            "mockOnly": true,
            "interface": "cancel-sale-order",
            "pathStatus": "待 Apifox 确认",
            "query": query,
            "body": body,
            "missingHeaders": missing_miniprogram_headers(headers)
        }
    })
}

fn mock_pay_order_payload(
    query: &std::collections::HashMap<String, String>,
    headers: &std::collections::HashMap<String, String>,
    body: &Value,
) -> Value {
    let sale_order_id = body
        .get("saleOrderId")
        .and_then(Value::as_str)
        .unwrap_or("mock-sale-order-001");
    json!({
        "code": 0,
        "msg": "mock",
        "data": {
            "saleOrderId": sale_order_id,
            "payType": body.get("payType").cloned().unwrap_or_else(|| json!(1)),
            "payStatus": "WAIT_PAY",
            "payAmt": 16.80,
            "payment": {
                "appId": "mock-miniapp-id",
                "timeStamp": "1783661760",
                "nonceStr": "mock-nonce",
                "package": "prepay_id=mock-prepay-id",
                "signType": "RSA",
                "paySign": "mock-pay-sign",
                "prepayId": "mock-prepay-id"
            }
        },
        "error": null,
        "_debug": {
            "mock": true,
            "mockOnly": true,
            "interface": "pay-order",
            "pathStatus": "待 Apifox 确认",
            "query": query,
            "body": body,
            "missingHeaders": missing_miniprogram_headers(headers)
        }
    })
}

fn mock_apply_refund_payload(
    query: &std::collections::HashMap<String, String>,
    headers: &std::collections::HashMap<String, String>,
    body: &Value,
) -> Value {
    let sale_order_id = body
        .get("saleOrderId")
        .and_then(Value::as_str)
        .unwrap_or("mock-sale-order-003");
    json!({
        "code": 0,
        "msg": "mock",
        "data": {
            "saleOrderId": sale_order_id,
            "refundId": "mock-refund-001",
            "refundNo": "RF202607100001",
            "refundAmt": body.get("refundAmt").cloned().unwrap_or_else(|| json!(16.80)),
            "refundReason": body.get("refundReason").cloned().unwrap_or_else(|| json!("用户申请退款")),
            "refundStatus": 1,
            "reviewStatus": 2,
            "displayStatus": "退款审核中",
            "applyTime": "2026-07-10 13:40:00"
        },
        "error": null,
        "_debug": {
            "mock": true,
            "mockOnly": true,
            "interface": "apply-refund",
            "pathStatus": "待 Apifox 确认",
            "query": query,
            "body": body,
            "missingHeaders": missing_miniprogram_headers(headers)
        }
    })
}

fn mock_order_summary(
    sale_order_id: &str,
    order_no: &str,
    display_status: &str,
    delivery_status: i64,
    pick_self_status: i64,
    pick_goods_type: i64,
) -> Value {
    json!({
        "saleOrderId": sale_order_id,
        "orderId": sale_order_id,
        "orderNo": order_no,
        "no": order_no,
        "storeId": "999006940",
        "storeNo": "6634",
        "storeName": "美宜佳科技园店",
        "totalQty": 3,
        "totalAmt": 18.50,
        "payAmt": 16.80,
        "orderStatusForAppletCenter": if display_status == "已完成" { 103 } else { 102 },
        "orderStatusForAppletDetail": delivery_status,
        "orderStatusForAppletDetailPickSelf": pick_self_status,
        "displayStatus": display_status,
        "isCancellable": display_status == "配送中" || display_status == "待取餐",
        "isRefundable": display_status == "已完成",
        "pickGoodsType": pick_goods_type,
        "pickCode": if pick_goods_type == 1 { "A168" } else { "" },
        "creationTime": "2026-07-10 12:30:00",
        "completionTime": if display_status == "已完成" { "2026-07-08 13:05:00" } else { "" },
        "payType": 1,
        "goodses": mock_goodses()
    })
}

fn mock_order_detail(
    sale_order_id: &str,
    order_no: &str,
    display_status: &str,
    delivery_status: i64,
    pick_self_status: i64,
    pick_goods_type: i64,
) -> Value {
    let mut order = serde_json::Map::new();
    order.insert("saleOrderId".to_string(), json!(sale_order_id));
    order.insert("orderId".to_string(), json!(sale_order_id));
    order.insert("no".to_string(), json!(order_no));
    order.insert("srcChannel".to_string(), json!(2));
    order.insert(
        "orderStatusForAppletDetail".to_string(),
        json!(delivery_status),
    );
    order.insert(
        "orderStatusForAppletDetailPickSelf".to_string(),
        json!(pick_self_status),
    );
    order.insert("displayStatus".to_string(), json!(display_status));
    order.insert(
        "statusDesc".to_string(),
        json!(if display_status == "配送中" {
            "骑手正在前往门店取货"
        } else {
            "模拟订单状态"
        }),
    );
    order.insert("creationTime".to_string(), json!("2026-07-10 12:30:00"));
    order.insert("payTime".to_string(), json!("2026-07-10 12:31:00"));
    order.insert(
        "completionTime".to_string(),
        json!(if display_status == "已完成" {
            "2026-07-08 13:05:00"
        } else {
            ""
        }),
    );
    order.insert("bookingTime".to_string(), json!(""));
    order.insert("expectFinishTime".to_string(), json!("2026-07-10 13:10:00"));
    order.insert("storeId".to_string(), json!("999006940"));
    order.insert("storeNo".to_string(), json!("6634"));
    order.insert("storeName".to_string(), json!("美宜佳科技园店"));
    order.insert(
        "storeAddress".to_string(),
        json!("深圳市南山区科技园模拟路 88 号"),
    );
    order.insert("storeTel".to_string(), json!("0755-12345678"));
    order.insert("storeGeo".to_string(), json!("113.934,22.540"));
    order.insert("storeReceivedOrderBeforeCnt".to_string(), json!(2));
    order.insert("totalAmt".to_string(), json!(18.50));
    order.insert("discountAmt".to_string(), json!(1.70));
    order.insert("goodsAmt".to_string(), json!(18.50));
    order.insert("payAmt".to_string(), json!(16.80));
    order.insert(
        "deliveryStaticCost".to_string(),
        json!(if pick_goods_type == 2 { 3.0 } else { 0.0 }),
    );
    order.insert("address_LinkMan".to_string(), json!("李先生"));
    order.insert("address_LinkTel".to_string(), json!("13800138000"));
    order.insert(
        "address_Address".to_string(),
        json!("深圳市南山区某小区 8 栋"),
    );
    order.insert("address_LinkMan_Secret".to_string(), json!("李*生"));
    order.insert("address_LinkTel_Secret".to_string(), json!("138****8000"));
    order.insert(
        "deliveryType".to_string(),
        json!(if pick_goods_type == 2 { 2 } else { 0 }),
    );
    order.insert("pickGoodsType".to_string(), json!(pick_goods_type));
    order.insert(
        "pickCode".to_string(),
        json!(if pick_goods_type == 1 { "A168" } else { "" }),
    );
    order.insert("takeQrCode".to_string(), json!("mock-qrcode-token"));
    order.insert("remark".to_string(), json!("少冰"));
    order.insert(
        "isCancellable".to_string(),
        json!(display_status == "配送中" || display_status == "待取餐"),
    );
    order.insert(
        "isRefundable".to_string(),
        json!(display_status == "已完成"),
    );
    order.insert("refundStatus".to_string(), json!(0));
    order.insert("reviewStatus".to_string(), json!(0));
    order.insert("goodses".to_string(), json!(mock_goodses()));
    order.insert("goodsGroups".to_string(), json!([]));
    order.insert(
        "discounts".to_string(),
        json!([
            {"name": "会员优惠", "amount": 1.70}
        ]),
    );
    order.insert("activityName".to_string(), json!("夏日饮品优惠"));
    Value::Object(order)
}

fn mock_goodses() -> Vec<Value> {
    vec![
        json!({
            "goodsId": "cola-500",
            "goodsName": "可口可乐",
            "goodsType": 1,
            "spec": "500ml",
            "qty": 2,
            "salePrice": 3.5,
            "payAmt": 7.0
        }),
        json!({
            "goodsId": "water-555",
            "goodsName": "怡宝矿泉水",
            "goodsType": 1,
            "spec": "555ml",
            "qty": 1,
            "salePrice": 2.5,
            "payAmt": 2.5
        }),
    ]
}

#[derive(Debug, Deserialize)]
struct ConfirmOrderRequest {
    conversation_id: String,
    #[serde(default = "default_device_id")]
    device_id: String,
    #[serde(default)]
    context: Value,
    items: Vec<ProductMatch>,
}

async fn confirm_order(
    State(state): State<AppState>,
    Json(req): Json<ConfirmOrderRequest>,
) -> Result<Json<Value>, ApiError> {
    db::ensure_conversation(&state.pool, &req.conversation_id).await?;
    let turn_id = Uuid::new_v4().to_string();
    db::log_event(
        &state.pool,
        &req.conversation_id,
        &turn_id,
        "order_submit_started",
        &json!({
            "conversation_id": &req.conversation_id,
            "turn_id": &turn_id,
            "source": "manual_confirm",
            "items": &req.items
        }),
    )
    .await;
    let result = submit_order(
        &state,
        &req.conversation_id,
        &req.device_id,
        req.context,
        &req.items,
    )
    .await;
    let event_type = if result.get("ok").and_then(Value::as_bool) == Some(false) {
        "order_failed"
    } else {
        "order_created"
    };
    db::log_event(
        &state.pool,
        &req.conversation_id,
        &turn_id,
        event_type,
        &result,
    )
    .await;
    Ok(Json(result))
}

async fn submit_order(
    state: &AppState,
    conversation_id: &str,
    device_id: &str,
    context: Value,
    items: &[ProductMatch],
) -> Value {
    let config = match db::get_config(&state.pool).await {
        Ok(config) => config,
        Err(error) => {
            return json!({
                "ok": false,
                "code": "ORDER_CONFIG_ERROR",
                "message": format!("订单配置读取失败：{error}")
            });
        }
    };
    if !config.order_mcp_enabled {
        let context = order_context_without_mcp(&config, context);
        let tool_name = order_mcp_tool_name(&config, "create_order", "createOrder");
        db::log_event(
            &state.pool,
            conversation_id,
            "order-api",
            "order_create_call",
            &json!({
                "tool": tool_name,
                "device_id": device_id,
                "context": context.clone(),
                "items": items,
                "mcp_enabled": false
            }),
        )
        .await;
        db::log_event(
            &state.pool,
            conversation_id,
            "order-api",
            "order_create_fallback",
            &json!({
                "reason": {
                    "code": "ORDER_MCP_DISABLED",
                    "message": "订单 MCP 未启用"
                },
                "fallback": "local_mock_order"
            }),
        )
        .await;
        return create_local_mock_order(state, conversation_id, context, &items).await;
    }
    let base_client =
        OrderMcpClient::new(config.order_mcp_url.clone(), config.order_mcp_token.clone());
    let context = resolve_order_context(&base_client, &config, device_id, context).await;
    let client = OrderMcpClient::new_with_context(
        config.order_mcp_url.clone(),
        config.order_mcp_token.clone(),
        &context,
    );
    let authorize_tool = order_mcp_tool_name(&config, "authorize_member", "authorizeMember");
    db::log_event(
        &state.pool,
        conversation_id,
        "order-api",
        "order_mcp_authorize_call",
        &json!({"tool": authorize_tool}),
    )
    .await;
    let authorize_result = client.call_tool(&authorize_tool, json!({})).await;
    db::log_event(
        &state.pool,
        conversation_id,
        "order-api",
        "order_mcp_authorize_result",
        &json!({
            "tool": authorize_tool,
            "success": mcp_tool_succeeded(&authorize_result),
            "code": authorize_result.get("code"),
            "message": authorize_result.get("message").or_else(|| authorize_result.get("msg"))
        }),
    )
    .await;
    if !mcp_tool_succeeded(&authorize_result) {
        db::log_event(
            &state.pool,
            conversation_id,
            "order-api",
            "order_create_failed",
            &json!({
                "stage": "authorize_member",
                "reason": authorize_result,
                "fallback": "disabled_when_mcp_enabled"
            }),
        )
        .await;
        return authorize_result;
    }
    let items = match enrich_mcp_product_matches(&client, &config, &context, conversation_id, items)
        .await
    {
        Ok(items) => items,
        Err(error) => {
            db::log_event(
                &state.pool,
                conversation_id,
                "order-api",
                "order_product_resolution_failed",
                &error,
            )
            .await;
            return error;
        }
    };
    let preview_tool = order_mcp_tool_name(&config, "preview_order", "previewOrder");
    let create_arguments = build_create_order_arguments(&context, &items);
    let preview_arguments = build_preview_order_arguments(&create_arguments);
    db::log_event(
        &state.pool,
        conversation_id,
        "order-api",
        "order_mcp_preview_call",
        &json!({"tool": preview_tool, "arguments": preview_arguments}),
    )
    .await;
    let preview_result = client.call_tool(&preview_tool, preview_arguments).await;
    db::log_event(
        &state.pool,
        conversation_id,
        "order-api",
        "order_mcp_preview_result",
        &json!({
            "tool": preview_tool,
            "success": mcp_tool_succeeded(&preview_result),
            "code": preview_result.get("code"),
            "message": preview_result.get("message").or_else(|| preview_result.get("msg")),
            "aboutTime": preview_result.pointer("/data/aboutTime"),
            "discountPrice": preview_result.pointer("/data/discountPrice")
        }),
    )
    .await;
    if !mcp_tool_succeeded(&preview_result) {
        db::log_event(
            &state.pool,
            conversation_id,
            "order-api",
            "order_create_failed",
            &json!({
                "stage": "preview_order",
                "reason": preview_result,
                "fallback": "disabled_when_mcp_enabled"
            }),
        )
        .await;
        return preview_result;
    }
    let tool_name = order_mcp_tool_name(&config, "create_order", "createOrder");
    db::log_event(
        &state.pool,
        conversation_id,
        "order-api",
        "order_create_call",
        &json!({
            "tool": tool_name,
            "device_id": device_id,
            "context": context.clone(),
            "items": items,
            "arguments": create_arguments
        }),
    )
    .await;
    let result = client.call_tool(&tool_name, create_arguments).await;
    if should_use_local_order_fallback(&result) {
        db::log_event(
            &state.pool,
            conversation_id,
            "order-api",
            "order_create_failed",
            &json!({
                "reason": result,
                "fallback": "disabled_when_mcp_enabled"
            }),
        )
        .await;
        return result;
    }
    persist_order_mcp_result(state, conversation_id, context, &items, result).await
}

#[derive(Debug, Deserialize)]
struct OrderListRequest {
    #[serde(default = "default_device_id")]
    device_id: String,
    #[serde(default)]
    context: Value,
    #[serde(default)]
    filters: Value,
}

async fn list_orders(
    State(state): State<AppState>,
    Json(req): Json<OrderListRequest>,
) -> Result<Json<Value>, ApiError> {
    let config = db::get_config(&state.pool).await?;
    if !config.order_mcp_enabled {
        let orders = db::list_mock_order_payloads(&state.pool).await?;
        return Ok(Json(json!({
            "ok": true,
            "mock": true,
            "mcp_enabled": false,
            "orders": orders
        })));
    }
    let base_client =
        OrderMcpClient::new(config.order_mcp_url.clone(), config.order_mcp_token.clone());
    let context = resolve_order_context(&base_client, &config, &req.device_id, req.context).await;
    let client = OrderMcpClient::new_with_context(
        config.order_mcp_url.clone(),
        config.order_mcp_token.clone(),
        &context,
    );
    let tool_name = order_mcp_tool_name(&config, "list_orders", "listUserOrders");
    let result = client
        .call_tool(
            &tool_name,
            json!({"context": context, "filters": req.filters}),
        )
        .await;
    if should_use_local_order_fallback(&result) {
        let orders = db::list_mock_order_payloads(&state.pool).await?;
        return Ok(Json(json!({"ok": true, "mock": true, "orders": orders})));
    }
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
struct OrderDetailRequest {
    #[serde(default = "default_device_id")]
    device_id: String,
    #[serde(default)]
    context: Value,
    #[serde(alias = "saleOrderId", alias = "order_id")]
    sale_order_id: String,
    #[serde(default, alias = "correlationId")]
    correlation_id: Option<String>,
}

async fn get_order_detail(
    State(state): State<AppState>,
    Json(req): Json<OrderDetailRequest>,
) -> Result<Json<Value>, ApiError> {
    let config = db::get_config(&state.pool).await?;
    if !config.order_mcp_enabled {
        return Ok(Json(
            db::get_mock_order_payload(&state.pool, &req.sale_order_id)
                .await?
                .unwrap_or_else(|| order_error("ORDER_NOT_FOUND", "未找到本地订单")),
        ));
    }
    let base_client =
        OrderMcpClient::new(config.order_mcp_url.clone(), config.order_mcp_token.clone());
    let context = resolve_order_context(&base_client, &config, &req.device_id, req.context).await;
    let client = OrderMcpClient::new_with_context(
        config.order_mcp_url.clone(),
        config.order_mcp_token.clone(),
        &context,
    );
    let tool_name = order_mcp_tool_name(&config, "get_order_detail", "getUserOrderDetail");
    let arguments = if tool_name == "queryOrderDetailInfo" {
        json!({"orderId": req.sale_order_id})
    } else {
        json!({
            "context": context,
            "orderId": req.sale_order_id,
            "saleOrderId": req.sale_order_id,
            "correlationId": req.correlation_id
        })
    };
    let result = client.call_tool(&tool_name, arguments).await;
    if should_use_local_order_fallback(&result) {
        return Ok(Json(
            db::get_mock_order_payload(&state.pool, &req.sale_order_id)
                .await?
                .unwrap_or_else(|| order_error("ORDER_NOT_FOUND", "未找到本地订单")),
        ));
    }
    Ok(Json(result))
}

#[derive(Debug, Deserialize)]
struct RefundOrderRequest {
    #[serde(default = "default_device_id")]
    device_id: String,
    #[serde(default)]
    context: Value,
    #[serde(alias = "saleOrderId", alias = "order_id")]
    sale_order_id: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default, alias = "correlationId")]
    correlation_id: Option<String>,
}

async fn refund_order(
    State(state): State<AppState>,
    Json(req): Json<RefundOrderRequest>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        refund_submitted_order(
            &state,
            "manual-refund",
            &req.device_id,
            req.context,
            &req.sale_order_id,
            req.reason,
            req.correlation_id,
        )
        .await,
    ))
}

async fn refund_submitted_order(
    state: &AppState,
    conversation_id: &str,
    device_id: &str,
    context: Value,
    sale_order_id: &str,
    reason: Option<String>,
    correlation_id: Option<String>,
) -> Value {
    let config = match db::get_config(&state.pool).await {
        Ok(config) => config,
        Err(error) => {
            return order_error("ORDER_CONFIG_ERROR", &format!("订单配置读取失败：{error}"));
        }
    };
    if !config.order_mcp_enabled {
        db::log_event(
            &state.pool,
            conversation_id,
            "order-api",
            "order_refund_fallback",
            &json!({
                "reason": {
                    "code": "ORDER_MCP_DISABLED",
                    "message": "订单 MCP 未启用"
                },
                "fallback": "local_mock_refund"
            }),
        )
        .await;
        return match refund_local_mock_order(state, sale_order_id, reason).await {
            Ok(value) => value,
            Err(error) => order_error("ORDER_REFUND_FAILED", &error.message),
        };
    }
    let base_client =
        OrderMcpClient::new(config.order_mcp_url.clone(), config.order_mcp_token.clone());
    let context = resolve_order_context(&base_client, &config, device_id, context).await;
    let client = OrderMcpClient::new_with_context(
        config.order_mcp_url.clone(),
        config.order_mcp_token.clone(),
        &context,
    );
    let tool_name = order_mcp_tool_name(&config, "refund_order", "refundOrder");
    db::log_event(
        &state.pool,
        conversation_id,
        "order-api",
        "order_refund_call",
        &json!({
            "tool": tool_name,
            "device_id": device_id,
            "context": context.clone(),
            "saleOrderId": sale_order_id,
            "reason": reason.clone(),
            "correlationId": correlation_id.clone()
        }),
    )
    .await;
    let result = client
        .call_tool(
            &tool_name,
            json!({
                "context": context,
                "saleOrderId": sale_order_id,
                "reason": reason.clone(),
                "correlationId": correlation_id.clone()
            }),
        )
        .await;
    if should_use_local_order_fallback(&result) {
        db::log_event(
            &state.pool,
            conversation_id,
            "order-api",
            "order_refund_failed",
            &json!({
                "reason": result,
                "fallback": "disabled_when_mcp_enabled"
            }),
        )
        .await;
        return result;
    }
    result
}

async fn resolve_order_context(
    client: &OrderMcpClient,
    config: &AppConfig,
    device_id: &str,
    context: Value,
) -> Value {
    if context
        .as_object()
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return context;
    }
    if config
        .order_context
        .as_object()
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return config.order_context.clone();
    }
    let tool_name = order_mcp_tool_name(config, "resolve_context", "resolveUserContext");
    let resolved = client
        .call_tool(&tool_name, json!({"deviceId": device_id}))
        .await;
    if should_use_local_order_fallback(&resolved) {
        return crate::config::default_order_context();
    }
    resolved.get("context").cloned().unwrap_or(resolved)
}

fn order_context_without_mcp(config: &AppConfig, context: Value) -> Value {
    if context
        .as_object()
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return context;
    }
    if config
        .order_context
        .as_object()
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return config.order_context.clone();
    }
    crate::config::default_order_context()
}

fn order_mcp_tool_name(config: &AppConfig, key: &str, fallback: &str) -> String {
    config
        .order_mcp_tools
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn build_create_order_arguments(context: &Value, items: &[ProductMatch]) -> Value {
    let dept_id = context
        .get("deptId")
        .and_then(value_as_i64)
        .or_else(|| context.get("storeId").and_then(value_as_i64))
        .unwrap_or(999006940);
    let longitude = context
        .get("longitude")
        .and_then(Value::as_f64)
        .unwrap_or(113.9419);
    let latitude = context
        .get("latitude")
        .and_then(Value::as_f64)
        .unwrap_or(22.5431);
    let delivery = context
        .get("delivery")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("pick");
    let mut arguments = json!({
        "deptId": dept_id,
        "productList": items.iter().map(product_match_to_mcp_line).collect::<Vec<_>>(),
        "longitude": longitude,
        "latitude": latitude,
        "delivery": delivery,
        "couponCodeList": []
    });
    if let Some(address_id) = context.get("addressId").and_then(value_as_i64) {
        arguments["addressId"] = json!(address_id);
    }
    arguments
}

fn build_preview_order_arguments(create_arguments: &Value) -> Value {
    let mut arguments = json!({
        "deptId": create_arguments.get("deptId"),
        "productList": create_arguments.get("productList"),
        "delivery": create_arguments.get("delivery")
    });
    if let Some(address_id) = create_arguments.get("addressId") {
        arguments["addressId"] = address_id.clone();
    }
    arguments
}

fn product_match_to_mcp_line(item: &ProductMatch) -> Value {
    if let (Some(product_id), Some(sku_code)) = (item.mcp_product_id, item.mcp_sku_code.as_deref())
    {
        return json!({
            "productId": product_id,
            "skuCode": sku_code,
            "amount": item.quantity
        });
    }
    if let (Some(parent_goods_gid), Some(parent_goods_no), Some(goods_gid), Some(goods_no)) = (
        item.parent_goods_gid,
        item.parent_goods_no.as_deref(),
        item.goods_gid,
        item.goods_no.as_deref(),
    ) {
        return json!({
            "parentGoodsGid": parent_goods_gid,
            "parentGoodsNo": parent_goods_no,
            "goodsGid": goods_gid,
            "goodsNo": goods_no,
            "amount": item.quantity
        });
    }
    let (product_id, sku_code) = match item.product_id.as_str() {
        "cola-500" => (16513, "SP11392-500ML"),
        "water-555" => (20002, "SP20002-555ML"),
        "milk-250" => (30003, "SP30003-250ML"),
        _ if item.name.contains("可乐") => (16513, "SP11392-500ML"),
        _ if item.name.contains("水") || item.name.contains("怡宝") => (20002, "SP20002-555ML"),
        _ if item.name.contains("奶") => (30003, "SP30003-250ML"),
        _ => (16513, "SP11392-500ML"),
    };
    json!({
        "productId": product_id,
        "skuCode": sku_code,
        "amount": item.quantity
    })
}

async fn enrich_mcp_product_matches(
    client: &OrderMcpClient,
    config: &AppConfig,
    context: &Value,
    conversation_id: &str,
    items: &[ProductMatch],
) -> Result<Vec<ProductMatch>, Value> {
    let tool_name = order_mcp_tool_name(config, "search_product", "searchProductForMcp");
    let dept_id = context
        .get("deptId")
        .and_then(value_as_i64)
        .or_else(|| context.get("storeId").and_then(value_as_i64))
        .unwrap_or_default();
    let delivery = context
        .get("delivery")
        .and_then(Value::as_str)
        .unwrap_or("pick");
    let mut enriched = Vec::with_capacity(items.len());
    for item in items {
        let result = client
            .call_tool(
                &tool_name,
                json!({
                    "deptId": dept_id,
                    "query": item.name,
                    "delivery": delivery,
                    "chatId": conversation_id
                }),
            )
            .await;
        if !mcp_tool_succeeded(&result) {
            return Err(order_error(
                "ORDER_PRODUCT_RESOLUTION_FAILED",
                result
                    .get("message")
                    .or_else(|| result.get("msg"))
                    .and_then(Value::as_str)
                    .unwrap_or("商品搜索失败"),
            ));
        }
        let candidates = result
            .get("data")
            .and_then(Value::as_array)
            .or_else(|| result.pointer("/data/items").and_then(Value::as_array))
            .cloned()
            .unwrap_or_default();
        let Some(candidate) = candidates.first() else {
            // Older customer MCP deployments may expose the order tools but
            // not the product-search tool yet. Preserve the existing product
            // mapping in that compatibility case; the subsequent preview or
            // create call remains the source of truth for success/failure.
            if extract_mcp_order_id(&result).is_some() {
                enriched.push(item.clone());
                continue;
            }
            return Err(order_error(
                "ORDER_PRODUCT_NOT_FOUND",
                &format!("未找到商品：{}", item.name),
            ));
        };
        let product_id = candidate
            .get("productId")
            .and_then(value_as_i64)
            .ok_or_else(|| order_error("ORDER_PRODUCT_BAD_RESPONSE", "商品结果缺少 productId"))?;
        let sku_code = candidate
            .get("skuCode")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| order_error("ORDER_PRODUCT_BAD_RESPONSE", "商品结果缺少 skuCode"))?;
        let mut item = item.clone();
        item.mcp_product_id = Some(product_id);
        item.mcp_sku_code = Some(sku_code.to_string());
        item.unit_price = candidate
            .get("estimatePrice")
            .or_else(|| candidate.get("initialPrice"))
            .and_then(Value::as_f64)
            .unwrap_or(item.unit_price);
        enriched.push(item);
    }
    Ok(enriched)
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
}

fn should_use_local_order_fallback(value: &Value) -> bool {
    value
        .get("code")
        .and_then(Value::as_str)
        .map(|code| code.starts_with("ORDER_MCP_") || code.ends_with("_NOT_CONFIGURED"))
        .unwrap_or(false)
}

fn mcp_tool_succeeded(value: &Value) -> bool {
    if value.get("ok").and_then(Value::as_bool) == Some(false)
        || value.get("success").and_then(Value::as_bool) == Some(false)
    {
        return false;
    }
    if let Some(code) = value.get("code") {
        if code.as_i64().is_some_and(|code| code != 0 && code != 200) {
            return false;
        }
        if code
            .as_str()
            .is_some_and(|code| code.starts_with("ORDER_MCP_") || code.ends_with("_NOT_CONFIGURED"))
        {
            return false;
        }
    }
    true
}

async fn persist_order_mcp_result(
    state: &AppState,
    conversation_id: &str,
    context: Value,
    items: &[ProductMatch],
    result: Value,
) -> Value {
    let Some(order_id) = extract_mcp_order_id(&result) else {
        return result;
    };
    let mut payload = result;
    payload["ok"] = json!(payload
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(true));
    payload["mock"] = json!(false);
    payload["saleOrderId"] = json!(order_id.clone());
    payload["order_id"] = json!(order_id.clone());
    payload["conversation_id"] = json!(conversation_id);
    payload["context"] = context;
    payload["items"] = serde_json::to_value(items).unwrap_or_else(|_| json!([]));
    if let Err(error) =
        db::save_mock_order_payload(&state.pool, &order_id, conversation_id, &payload).await
    {
        return order_error(
            "ORDER_MCP_SAVE_FAILED",
            &format!("MCP 订单保存失败：{error}"),
        );
    }
    db::log_event(
        &state.pool,
        conversation_id,
        "order-api",
        "order_persisted",
        &payload,
    )
    .await;
    payload
}

fn extract_mcp_order_id(value: &Value) -> Option<String> {
    for pointer in [
        "/data/orderId",
        "/data/id",
        "/orderId",
        "/id",
        "/saleOrderId",
        "/data/saleOrderId",
    ] {
        if let Some(candidate) = value.pointer(pointer) {
            let text = candidate
                .as_str()
                .map(ToString::to_string)
                .or_else(|| candidate.as_i64().map(|number| number.to_string()))
                .or_else(|| candidate.as_u64().map(|number| number.to_string()));
            if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
                return Some(text);
            }
        }
    }
    None
}

async fn create_local_mock_order(
    state: &AppState,
    conversation_id: &str,
    context: Value,
    items: &[ProductMatch],
) -> Value {
    let order = create_mock_order(conversation_id, items);
    let mut payload = serde_json::to_value(&order).unwrap_or_else(|_| json!({}));
    payload["ok"] = json!(true);
    payload["mock"] = json!(true);
    payload["saleOrderId"] = json!(order.order_id.clone());
    payload["status"] = json!("created");
    payload["context"] = context;
    if let Err(error) = db::save_mock_order_payload(
        &state.pool,
        &order.order_id,
        &order.conversation_id,
        &payload,
    )
    .await
    {
        return order_error(
            "ORDER_MOCK_SAVE_FAILED",
            &format!("本地订单保存失败：{error}"),
        );
    }
    db::log_event(
        &state.pool,
        conversation_id,
        "order-api",
        "order_persisted",
        &payload,
    )
    .await;
    payload
}

async fn refund_local_mock_order(
    state: &AppState,
    sale_order_id: &str,
    reason: Option<String>,
) -> Result<Value, ApiError> {
    let Some(mut payload) = db::get_mock_order_payload(&state.pool, sale_order_id).await? else {
        return Ok(order_error("ORDER_NOT_FOUND", "未找到本地订单"));
    };
    payload["ok"] = json!(true);
    payload["mock"] = json!(true);
    payload["status"] = json!("refunded");
    payload["refundReason"] = json!(reason.unwrap_or_else(|| "用户申请退单".to_string()));
    payload["refundedAt"] = json!(Utc::now().to_rfc3339());
    let conversation_id = payload
        .get("conversation_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    db::save_mock_order_payload(&state.pool, sale_order_id, &conversation_id, &payload).await?;
    Ok(payload)
}

fn default_device_id() -> String {
    "DOLL-0001".to_string()
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn unauthorized(message: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.to_string(),
        }
    }
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.into().to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

#[cfg(test)]
mod intent_tests {
    use super::{
        classify_iat_error, classify_tts_error, couple_live_iat_io, decode_segment_audio_packet,
        direct_conversation_end_reply, emit_audio_relay_diagnostic,
        emit_upstream_audio_rejection_evidence, is_explicit_order_refund_intent,
        is_order_confirmation_intent, mock_audio_chunk, negotiate_voice_audio,
        pcm_duration_ms_from_bytes, reap_finished_live_asr_session, resolve_voice_audio, run_turn,
        send_client_event, stop_live_asr_session, AppState, AudioUpstreamDirection, IatProvider,
        LiveAsrFrame, LiveAsrSession, LiveIatWriterState, Message, RecognizedTurn, StreamEvent,
        TurnCapacityPermit, UpstreamMessage, Value, VoiceAudioContext, VoiceWsQuery,
        WsTurnCoordinator,
    };
    use crate::domain::audio::{AudioFormat, AudioProfile, AudioSampleRate};
    use crate::xfyun::audio::TtsProvider;
    use crate::xfyun::iat::{parse_standard_iat_text, AudioPacketError};
    use crate::xfyun::tts::{
        parse_standard_tts_audio_frame, tts_encoding, StandardTtsPacketizer,
        TimedStandardTtsPacketizer, TimedTtsAudioChunk, TtsAudioChunk,
    };
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct ActiveGuard(Arc<AtomicUsize>);

    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn pending_live_session(active: Arc<AtomicUsize>) -> LiveAsrSession {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            active.fetch_add(1, Ordering::SeqCst);
            let _guard = ActiveGuard(active);
            std::future::pending::<()>().await;
        });
        LiveAsrSession::new(
            sender,
            task,
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
            IatProvider::Standard,
        )
    }

    fn test_app_state() -> AppState {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let (diagnostics, _) = tokio::sync::broadcast::channel(1);
        AppState {
            pool,
            server_secret: Arc::new("test-secret".to_string()),
            admin_config: crate::admin_auth::AdminConfig::new(
                "myjadmin",
                crate::admin_auth::hash_password("test-admin-password").unwrap(),
            )
            .unwrap(),
            diagnostics,
        }
    }

    async fn initialized_test_app_state() -> AppState {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::init(&pool).await.unwrap();
        let (diagnostics, _) = tokio::sync::broadcast::channel(16);
        AppState {
            pool,
            server_secret: Arc::new("test-secret".to_string()),
            admin_config: crate::admin_auth::AdminConfig::new(
                "myjadmin",
                crate::admin_auth::hash_password("test-admin-password").unwrap(),
            )
            .unwrap(),
            diagnostics,
        }
    }

    fn test_audio_context() -> VoiceAudioContext {
        let config = crate::config::AppConfig::default_from_env();
        VoiceAudioContext {
            audio: crate::domain::audio::VoiceConnectionAudio::from_query(None, None, None, None)
                .unwrap(),
            iat_provider: IatProvider::SuperSmart,
            tts_provider: TtsProvider::SuperSmart,
            config,
        }
    }

    async fn spawn_controlled_ws_provider(
        response: Option<serde_json::Value>,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _request = socket.next().await.unwrap().unwrap();
            if let Some(response) = response {
                socket
                    .send(UpstreamMessage::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
            let _ = entered_tx.send(());
            tokio::select! {
                _ = release_rx => {}
                _ = socket.next() => {}
            }
            let _ = socket.close(None).await;
            let _ = closed_tx.send(());
        });
        (
            format!("ws://{address}/provider"),
            entered_rx,
            release_tx,
            closed_rx,
            task,
        )
    }

    async fn wait_for_turn_capacity(coordinator: &WsTurnCoordinator) -> TurnCapacityPermit {
        loop {
            if let Ok(permit) = coordinator.try_reserve_turn_capacity() {
                return permit;
            }
            tokio::task::yield_now().await;
        }
    }

    fn forbidden_reply_event(event_type: &str) -> bool {
        matches!(
            event_type,
            "llm_delta" | "reply_sentence" | "tts_audio_chunk" | "voice_done"
        )
    }

    #[tokio::test]
    async fn upstream_audio_rejection_evidence_is_exact_and_contains_no_sensitive_fields() {
        let state = test_app_state();
        let mut diagnostics = state.diagnostics.subscribe();
        let profile = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000);
        let iat_error = parse_standard_iat_text(&serde_json::json!({
            "code": 10163,
            "message": "unsupported encoding token=secret audio=private"
        }))
        .unwrap_err();
        assert_eq!(
            classify_iat_error(&iat_error),
            "upstream_audio_profile_rejected"
        );

        assert!(emit_upstream_audio_rejection_evidence(
            &state,
            Some("conversation-1"),
            None,
            &iat_error,
            AudioUpstreamDirection::Iat,
            "standard",
            profile,
        ));

        let event = diagnostics.recv().await.unwrap();
        assert_eq!(event.stage, "upstream_audio_profile_rejected");
        assert_eq!(event.conversation_id.as_deref(), Some("conversation-1"));
        assert_eq!(
            event.detail,
            serde_json::json!({
                "direction": "iat",
                "provider": "standard",
                "format": "speex",
                "sample_rate": 8000,
                "service_code": "upstream_audio_profile_rejected",
                "upstream_code": 10163
            })
        );
        for forbidden in ["message", "audio", "url", "token", "secret"] {
            assert!(event.detail.get(forbidden).is_none(), "{forbidden}");
        }

        let tts_error = parse_standard_tts_audio_frame(&serde_json::json!({
            "code": 10006,
            "message": "invalid aue token=secret audio=private"
        }))
        .unwrap_err();
        assert_eq!(
            classify_tts_error(&tts_error),
            "upstream_audio_profile_rejected"
        );
        assert!(emit_upstream_audio_rejection_evidence(
            &state,
            Some("conversation-1"),
            None,
            &tts_error,
            AudioUpstreamDirection::Tts,
            "standard",
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
        ));
        let event = diagnostics.recv().await.unwrap();
        assert_eq!(
            event.detail,
            serde_json::json!({
                "direction": "tts",
                "provider": "standard",
                "format": "opus",
                "sample_rate": 16000,
                "service_code": "upstream_audio_profile_rejected",
                "upstream_code": 10006
            })
        );
        for forbidden in ["message", "audio", "url", "token", "secret"] {
            assert!(event.detail.get(forbidden).is_none(), "{forbidden}");
        }
    }

    #[test]
    fn explicit_refund_requests_are_accepted() {
        for text in [
            "退单",
            "退款",
            "取消订单",
            "我要退单",
            "我想退款，请处理",
            "请帮我退单",
            "帮我取消订单",
            "我要取消订单，谢谢",
        ] {
            assert!(
                is_explicit_order_refund_intent(text),
                "expected explicit refund intent: {text}"
            );
        }
    }

    #[test]
    fn ambiguous_dismissal_and_non_requests_are_rejected() {
        for text in [
            "退掉",
            "帮我退",
            "你可以退下了",
            "让玩偶退下",
            "我要让玩偶退下",
            "不要退单",
            "我不退款",
            "这个订单不要了",
            "怎么申请退款",
        ] {
            assert!(
                !is_explicit_order_refund_intent(text),
                "expected non-refund intent: {text}"
            );
        }
    }

    #[test]
    fn conversation_end_reply_is_deterministic_and_never_claims_a_refund() {
        assert_eq!(
            direct_conversation_end_reply("退一下吧。退一下吧"),
            Some("好的主人，我退下了。")
        );
        assert_eq!(direct_conversation_end_reply("我要退单"), None);
        assert_eq!(direct_conversation_end_reply("买一瓶水"), None);
    }

    #[test]
    fn natural_order_confirmations_are_accepted() {
        for text in [
            "下单。",
            "帮我下发订单。",
            "下发。",
            "对的。",
            "是的",
            "好的",
            "没错",
            "可以",
        ] {
            assert!(
                is_order_confirmation_intent(text),
                "expected order confirmation intent: {text}"
            );
        }
    }

    #[test]
    fn negated_order_confirmations_are_rejected() {
        for text in ["不要下单", "别下发", "先不下单", "取消下发订单"] {
            assert!(
                !is_order_confirmation_intent(text),
                "expected non-confirmation intent: {text}"
            );
        }
    }

    #[test]
    fn voice_audio_defaults_to_mp3_16k_for_both_directions() {
        let audio = resolve_voice_audio(
            None,
            None,
            None,
            None,
            IatProvider::Standard,
            TtsProvider::Standard,
        )
        .expect("default audio profiles");

        assert_eq!(audio.input.format, AudioFormat::Mp3);
        assert_eq!(audio.input.sample_rate, AudioSampleRate::Hz16000);
        assert_eq!(audio.output.format, AudioFormat::Mp3);
        assert_eq!(audio.output.sample_rate, AudioSampleRate::Hz16000);
    }

    #[test]
    fn voice_audio_resolves_independent_direction_profiles() {
        let audio = resolve_voice_audio(
            Some("speex"),
            Some("8000"),
            Some("opus"),
            Some("16000"),
            IatProvider::Standard,
            TtsProvider::Standard,
        )
        .unwrap();

        assert_eq!(audio.input.sample_rate.hz(), 8000);
        assert_eq!(audio.output.format, AudioFormat::Opus);
    }

    #[test]
    fn voice_audio_reports_format_rate_and_provider_profile_errors() {
        let format_error = resolve_voice_audio(
            Some("wav"),
            None,
            None,
            None,
            IatProvider::Standard,
            TtsProvider::Standard,
        )
        .unwrap_err();
        let rate_error = resolve_voice_audio(
            None,
            Some("44100"),
            None,
            None,
            IatProvider::Standard,
            TtsProvider::Standard,
        )
        .unwrap_err();
        let profile_error = resolve_voice_audio(
            Some("opus"),
            Some("16000"),
            None,
            None,
            IatProvider::Standard,
            TtsProvider::Standard,
        )
        .unwrap_err();
        let output_profile_error = resolve_voice_audio(
            None,
            None,
            Some("opus"),
            Some("16000"),
            IatProvider::Standard,
            TtsProvider::SuperSmart,
        )
        .unwrap_err();

        assert_eq!(format_error.code(), "unsupported_audio_format");
        assert_eq!(rate_error.code(), "unsupported_audio_rate");
        assert_eq!(profile_error.code(), "unsupported_audio_profile");
        assert_eq!(output_profile_error.code(), "unsupported_audio_profile");
    }

    #[tokio::test]
    async fn concurrent_voice_profiles_remain_isolated() {
        let (first, second) = tokio::join!(
            async {
                resolve_voice_audio(
                    Some("speex"),
                    Some("8000"),
                    Some("opus"),
                    Some("16000"),
                    IatProvider::Standard,
                    TtsProvider::Standard,
                )
                .unwrap()
            },
            async {
                resolve_voice_audio(
                    Some("pcm"),
                    Some("16000"),
                    Some("mp3"),
                    Some("8000"),
                    IatProvider::Standard,
                    TtsProvider::Standard,
                )
                .unwrap()
            }
        );

        assert_eq!(first.input.format, AudioFormat::Speex);
        assert_eq!(first.output.format, AudioFormat::Opus);
        assert_eq!(second.input.format, AudioFormat::Pcm);
        assert_eq!(second.output.sample_rate, AudioSampleRate::Hz8000);
    }

    #[tokio::test]
    async fn negotiated_audio_context_keeps_provider_config_snapshot() {
        let state = initialized_test_app_state().await;
        let mut config = crate::db::get_config(&state.pool).await.unwrap();
        config.iat_provider = "standard".to_string();
        config.tts_provider = "standard".to_string();
        config.iat_endpoint = "ws://old-iat.example/v1".to_string();
        config.tts_standard_endpoint = "ws://old-tts.example/v2".to_string();
        crate::db::save_config(&state.pool, &config).await.unwrap();
        let query = VoiceWsQuery::default();

        let old = negotiate_voice_audio(&state, &query).await.ok().unwrap();
        config.iat_provider = "super_smart".to_string();
        config.tts_provider = "super_smart".to_string();
        config.iat_endpoint = "ws://new-iat.example/v1".to_string();
        config.tts_endpoint = "ws://new-tts.example/v1".to_string();
        crate::db::save_config(&state.pool, &config).await.unwrap();
        let new = negotiate_voice_audio(&state, &query).await.ok().unwrap();

        assert_eq!(old.config.iat_provider, "standard");
        assert_eq!(old.config.iat_endpoint, "ws://old-iat.example/v1");
        assert_eq!(old.config.tts_standard_endpoint, "ws://old-tts.example/v2");
        assert_eq!(new.config.iat_provider, "super_smart");
        assert_eq!(new.config.iat_endpoint, "ws://new-iat.example/v1");
        assert_eq!(new.config.tts_endpoint, "ws://new-tts.example/v1");
    }

    #[tokio::test]
    async fn concurrent_audio_contexts_do_not_share_provider_snapshots() {
        let first_state = initialized_test_app_state().await;
        let second_state = initialized_test_app_state().await;
        let mut first_config = crate::db::get_config(&first_state.pool).await.unwrap();
        first_config.iat_endpoint = "ws://first.example/iat".to_string();
        crate::db::save_config(&first_state.pool, &first_config)
            .await
            .unwrap();
        let mut second_config = crate::db::get_config(&second_state.pool).await.unwrap();
        second_config.iat_endpoint = "ws://second.example/iat".to_string();
        crate::db::save_config(&second_state.pool, &second_config)
            .await
            .unwrap();

        let first_query = VoiceWsQuery::default();
        let second_query = VoiceWsQuery::default();
        let (first, second) = tokio::join!(
            negotiate_voice_audio(&first_state, &first_query),
            negotiate_voice_audio(&second_state, &second_query)
        );
        let first = first.ok().unwrap();
        let second = second.ok().unwrap();

        assert_eq!(first.config.iat_endpoint, "ws://first.example/iat");
        assert_eq!(second.config.iat_endpoint, "ws://second.example/iat");
    }

    #[tokio::test]
    async fn llm_failure_fallback_uses_snapshot_tts_profile_before_voice_done() {
        let state = initialized_test_app_state().await;
        let mut snapshot_config = crate::db::get_config(&state.pool).await.unwrap();
        snapshot_config.iat_provider = "standard".to_string();
        snapshot_config.tts_provider = "standard".to_string();
        snapshot_config.mock_providers = true;
        let audio_context = VoiceAudioContext {
            audio: crate::domain::audio::VoiceConnectionAudio::from_query(
                Some("speex"),
                Some("8000"),
                Some("opus"),
                Some("16000"),
            )
            .unwrap(),
            iat_provider: IatProvider::Standard,
            tts_provider: TtsProvider::Standard,
            config: snapshot_config,
        };
        let mut live_config = crate::db::get_config(&state.pool).await.unwrap();
        live_config.mock_providers = false;
        live_config.llm_endpoint = "ws://127.0.0.1:1/unreachable".to_string();
        crate::db::save_config(&state.pool, &live_config)
            .await
            .unwrap();
        let mut events = Vec::new();

        run_turn(
            &state,
            "fallback-conversation",
            "fallback-turn",
            &crate::db::ConversationOwner::Browser,
            "没有商品的测试输入",
            None,
            audio_context,
            |event| {
                events.push(event);
                async {}
            },
        )
        .await
        .unwrap();

        let reply_index = events
            .iter()
            .position(|event| event.event_type == "reply_sentence")
            .unwrap();
        let tts_index = events
            .iter()
            .position(|event| event.event_type == "tts_audio_chunk")
            .unwrap();
        let done_index = events
            .iter()
            .position(|event| event.event_type == "voice_done")
            .unwrap();
        assert!(reply_index < tts_index && tts_index < done_index);
        assert_eq!(events[tts_index].payload["format"], "opus");
        assert_eq!(events[tts_index].payload["sample_rate"], 16000);
        assert_eq!(events[tts_index].payload["mock"], true);
    }

    #[test]
    fn opus_tts_events_have_exact_profile_metadata_without_bit_depth() {
        let event = StreamEvent::tts_audio_chunk(
            "AAE=".to_string(),
            3,
            true,
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
        );

        assert_eq!(
            event.payload,
            serde_json::json!({
                "audio": "AAE=",
                "seq": 3,
                "format": "opus",
                "sample_rate": 16000,
                "channels": 1,
                "is_last": true
            })
        );
        assert!(event.payload.get("bit_depth").is_none());
    }

    #[test]
    fn standard_opus_provider_block_becomes_one_raw_packet_per_web_event() {
        let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
        let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
        let chunks = packetizer
            .push(TtsAudioChunk {
                audio: vec![0, 2, 1, 2, 0, 3, 3, 4, 5],
                is_last: true,
            })
            .unwrap();
        let events = chunks
            .into_iter()
            .map(|chunk| {
                StreamEvent::tts_audio_chunk(
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, chunk.audio),
                    7,
                    chunk.is_last,
                    profile,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["audio"], "AQI=");
        assert_eq!(events[1].payload["audio"], "AwQF");
        assert_eq!(events[0].payload["is_last"], false);
        assert_eq!(events[1].payload["is_last"], true);
        assert_eq!(events[0].payload["format"], "opus");
        assert_eq!(events[0].payload["sample_rate"], 16000);
    }

    #[test]
    fn standard_compressed_empty_upstream_final_becomes_a_real_final_web_packet() {
        let profile = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000);
        let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
        assert!(packetizer
            .push(TtsAudioChunk {
                audio: vec![9; 60],
                is_last: false,
            })
            .unwrap()
            .is_empty());
        let chunks = packetizer
            .push(TtsAudioChunk {
                audio: Vec::new(),
                is_last: true,
            })
            .unwrap();
        let event = StreamEvent::tts_audio_chunk(
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &chunks[0].audio),
            8,
            chunks[0].is_last,
            profile,
        );

        assert!(!chunks[0].audio.is_empty());
        assert_eq!(event.payload["is_last"], true);
        assert_eq!(event.payload["format"], "speex");
        assert_ne!(event.payload["audio"], "");
    }

    #[test]
    fn pcm_tts_events_use_the_connection_sample_rate() {
        let event = StreamEvent::tts_audio_chunk(
            "AAE=".to_string(),
            4,
            false,
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000),
        );

        assert_eq!(event.payload["format"], "pcm");
        assert_eq!(event.payload["sample_rate"], 8000);
        assert_eq!(event.payload["channels"], 1);
        assert_eq!(event.payload["bit_depth"], 16);
    }

    #[test]
    fn pcm_mock_audio_contains_complete_s16le_samples() {
        let profile = AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000);
        let audio = super::mock_audio_chunk("测试", profile);
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, audio.as_bytes())
                .expect("mock pcm base64");

        assert!(!decoded.is_empty());
        assert_eq!(decoded.len() % 2, 0);
        assert!(decoded.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn new_mock_audio_formats_are_non_empty_and_deterministic() {
        for format in [AudioFormat::Opus, AudioFormat::Speex] {
            let profile = AudioProfile::new(format, AudioSampleRate::Hz16000);
            let audio = mock_audio_chunk("测试", profile);
            let decoded = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                audio.as_bytes(),
            )
            .expect("mock encoded audio");

            assert!(!decoded.is_empty());
            let marker = String::from_utf8(decoded).unwrap();
            assert!(marker.starts_with("MOCK_"));
            assert!(marker.contains("UNENCODED_MARKER"));
            assert_eq!(audio, mock_audio_chunk("测试", profile));
        }

        let pcm8 = mock_audio_chunk(
            "测试",
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000),
        );
        let pcm16 = mock_audio_chunk(
            "测试",
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
        );
        let pcm8 =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, pcm8.as_bytes())
                .unwrap();
        let pcm16 =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, pcm16.as_bytes())
                .unwrap();
        assert_eq!(pcm16.len(), pcm8.len() * 2);
    }

    #[test]
    fn packetized_input_requires_stream_chunks_and_pcm_duration_uses_profile_rate() {
        let speex = AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000);
        let pcm8 = AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz8000);

        assert_eq!(
            decode_segment_audio_packet(Some("AA=="), speex)
                .unwrap_err()
                .code(),
            "invalid_audio_packet"
        );
        assert_eq!(pcm_duration_ms_from_bytes(16_000, pcm8.sample_rate), 1_000);
    }

    #[test]
    fn tts_profile_errors_are_classified_without_masking_network_failures() {
        let unsupported: anyhow::Error = tts_encoding(
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz24000),
            TtsProvider::Standard,
        )
        .unwrap_err()
        .into();
        let upstream = parse_standard_tts_audio_frame(&serde_json::json!({
            "code": 10006,
            "message": "unsupported audio codec opus-wb"
        }))
        .unwrap_err();
        let network = anyhow::anyhow!("connect standard tts websocket: network timeout");
        let rate_limit = parse_standard_tts_audio_frame(&serde_json::json!({
            "code": 10163,
            "message": "rate limit exceeded"
        }))
        .unwrap_err();

        assert_eq!(
            classify_tts_error(&unsupported),
            "unsupported_audio_profile"
        );
        assert_eq!(
            classify_tts_error(&upstream),
            "upstream_audio_profile_rejected"
        );
        assert_eq!(classify_tts_error(&network), "tts_failed");
        assert_eq!(classify_tts_error(&rate_limit), "tts_failed");
    }

    #[tokio::test]
    async fn relay_diagnostics_include_profile_provider_and_monotonic_duration() {
        let state = test_app_state();
        let mut diagnostics = state.diagnostics.subscribe();
        emit_audio_relay_diagnostic(
            &state,
            Some("conversation"),
            Some("turn"),
            None,
            "voice_audio_downlink_relay_duration",
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
            "standard",
            std::time::Instant::now(),
        );

        let event = diagnostics.try_recv().unwrap();
        assert_eq!(event.stage, "voice_audio_downlink_relay_duration");
        assert_eq!(event.detail["format"], "opus");
        assert_eq!(event.detail["sample_rate"], 16000);
        assert_eq!(event.detail["provider"], "standard");
        assert!(event.detail["duration_micros"].as_u64().is_some());

        let stream_event = StreamEvent::tts_audio_chunk(
            "AAE=".to_string(),
            0,
            false,
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
        )
        .with_audio_relay_timing(
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
            TtsProvider::Standard,
            std::time::Instant::now(),
        );
        let serialized = serde_json::to_value(&stream_event).unwrap();
        assert!(stream_event.relay_metric_context.is_some());
        assert!(serialized.get("relay_metric_context").is_none());
    }

    #[tokio::test]
    async fn downlink_relay_metric_waits_for_successful_client_send() {
        let state = test_app_state();
        let mut diagnostics = state.diagnostics.subscribe();
        let gate = Arc::new(tokio::sync::Notify::new());
        let send_gate = gate.clone();
        let event = StreamEvent::tts_audio_chunk(
            "AAE=".to_string(),
            0,
            false,
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
        )
        .with_audio_relay_timing(
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
            TtsProvider::Standard,
            std::time::Instant::now(),
        );
        let send_state = state.clone();
        let task = tokio::spawn(async move {
            send_client_event(&send_state, event, move |_| async move {
                send_gate.notified().await;
                Ok::<(), ()>(())
            })
            .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(matches!(
            diagnostics.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        gate.notify_one();
        task.await.unwrap().unwrap();
        let metric = diagnostics.recv().await.unwrap();
        assert_eq!(metric.stage, "voice_audio_downlink_relay_duration");
        assert!(metric.detail["duration_micros"].as_u64().unwrap() >= 10_000);
    }

    #[tokio::test]
    async fn failed_client_send_does_not_report_downlink_success_metric() {
        let state = test_app_state();
        let mut diagnostics = state.diagnostics.subscribe();
        let event = StreamEvent::tts_audio_chunk(
            "AAE=".to_string(),
            0,
            false,
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
        )
        .with_audio_relay_timing(
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
            TtsProvider::Standard,
            std::time::Instant::now(),
        );

        assert!(
            send_client_event(&state, event, |_| async { Err::<(), ()>(()) })
                .await
                .is_err()
        );
        assert!(matches!(
            diagnostics.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }

    #[derive(Debug)]
    struct RelayBenchmarkResult {
        packet_count: usize,
        aggregated_packets: usize,
        p95_nanos: u64,
    }

    fn benchmark_relay_profile(profile: AudioProfile, packet_count: usize) -> RelayBenchmarkResult {
        assert!(packet_count > 0);
        let packet_bytes = match (profile.format, profile.sample_rate) {
            (AudioFormat::Mp3, _) => 1_024,
            (AudioFormat::Opus, AudioSampleRate::Hz8000) => 20,
            (AudioFormat::Opus, AudioSampleRate::Hz16000) => 40,
            (AudioFormat::Speex, AudioSampleRate::Hz8000) => 38,
            (AudioFormat::Speex, AudioSampleRate::Hz16000) => 60,
            _ => panic!("unsupported benchmark profile: {profile:?}"),
        };
        let mut durations = Vec::with_capacity(packet_count);
        let mut aggregated_packets = 0usize;

        if profile.format == AudioFormat::Mp3 {
            for index in 0..packet_count {
                let started = std::time::Instant::now();
                let audio = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    vec![index as u8; packet_bytes],
                );
                let event =
                    StreamEvent::tts_audio_chunk(audio, 0, index + 1 == packet_count, profile);
                serde_json::to_vec(&event).unwrap();
                durations.push(started.elapsed().as_nanos() as u64);
            }
        } else {
            let mut packetizer = StandardTtsPacketizer::new(profile).unwrap();
            for index in 0..packet_count {
                let raw_packet = vec![index as u8; packet_bytes];
                let provider_audio = if profile.format == AudioFormat::Opus {
                    let mut framed = Vec::with_capacity(packet_bytes + 2);
                    framed.extend_from_slice(&(packet_bytes as u16).to_be_bytes());
                    framed.extend_from_slice(&raw_packet);
                    framed
                } else {
                    raw_packet
                };
                let packetize_started = std::time::Instant::now();
                let packets = packetizer
                    .push(TtsAudioChunk {
                        audio: provider_audio,
                        is_last: index + 1 == packet_count,
                    })
                    .unwrap();
                let packetize_nanos = packetize_started.elapsed().as_nanos() as u64;
                let per_packetize_nanos = packetize_nanos / packets.len().max(1) as u64;
                for packet in packets {
                    if packet.audio.len() != packet_bytes || packet.audio.is_empty() {
                        aggregated_packets += 1;
                    }
                    let relay_started = std::time::Instant::now();
                    let audio = base64::Engine::encode(
                        &base64::engine::general_purpose::STANDARD,
                        packet.audio,
                    );
                    let event = StreamEvent::tts_audio_chunk(audio, 0, packet.is_last, profile);
                    serde_json::to_vec(&event).unwrap();
                    durations.push(per_packetize_nanos + relay_started.elapsed().as_nanos() as u64);
                }
            }
        }

        assert_eq!(durations.len(), packet_count);
        durations.sort_unstable();
        let p95_index = (durations.len() * 95).div_ceil(100).saturating_sub(1);
        let result = RelayBenchmarkResult {
            packet_count: durations.len(),
            aggregated_packets,
            p95_nanos: durations[p95_index],
        };
        println!(
            "relay_benchmark format={} rate={} packets={} boundary=packetizer+base64+event_json p95_us={:.3} aggregated={}",
            profile.format.as_str(),
            profile.sample_rate.hz(),
            result.packet_count,
            result.p95_nanos as f64 / 1_000.0,
            result.aggregated_packets
        );
        result
    }

    #[derive(Debug)]
    struct DiagnosticRelayBenchmarkResult {
        packet_count: usize,
        aggregated_packets: usize,
        p95_micros: u64,
    }

    fn diagnostic_relay_result(
        direction: &str,
        profile: AudioProfile,
        mut durations: Vec<u64>,
        aggregated_packets: usize,
    ) -> DiagnosticRelayBenchmarkResult {
        durations.sort_unstable();
        let p95_index = (durations.len() * 95).div_ceil(100).saturating_sub(1);
        let result = DiagnosticRelayBenchmarkResult {
            packet_count: durations.len(),
            aggregated_packets,
            p95_micros: durations[p95_index],
        };
        println!(
            "relay_diagnostic_benchmark direction={} format={} rate={} packets={} boundary=server_entry_to_awaited_sink_success p95_us={} aggregated={}",
            direction,
            profile.format.as_str(),
            profile.sample_rate.hz(),
            result.packet_count,
            result.p95_micros,
            result.aggregated_packets
        );
        result
    }

    async fn benchmark_diagnostic_downlink(
        profile: AudioProfile,
        packet_count: usize,
    ) -> DiagnosticRelayBenchmarkResult {
        let state = test_app_state();
        let mut diagnostics = state.diagnostics.subscribe();
        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel::<Message>(packet_count + 2);
        let packet_bytes = match (profile.format, profile.sample_rate) {
            (AudioFormat::Mp3, _) => 1_024,
            (AudioFormat::Opus, AudioSampleRate::Hz8000) => 20,
            (AudioFormat::Opus, AudioSampleRate::Hz16000) => 40,
            (AudioFormat::Speex, AudioSampleRate::Hz8000) => 38,
            (AudioFormat::Speex, AudioSampleRate::Hz16000) => 60,
            _ => panic!("unsupported downlink diagnostic benchmark profile: {profile:?}"),
        };
        let provider = if profile.format == AudioFormat::Mp3 {
            TtsProvider::SuperSmart
        } else {
            TtsProvider::Standard
        };
        let mut durations = Vec::with_capacity(packet_count);
        let mut aggregated_packets = 0usize;
        let mut packetizer = (profile.format != AudioFormat::Mp3)
            .then(|| TimedStandardTtsPacketizer::new(profile).unwrap());

        for index in 0..packet_count {
            let raw_packet = vec![index as u8; packet_bytes];
            let is_last = index + 1 == packet_count;
            let chunks = if let Some(packetizer) = packetizer.as_mut() {
                let provider_audio = if profile.format == AudioFormat::Opus {
                    let mut framed = Vec::with_capacity(packet_bytes + 2);
                    framed.extend_from_slice(&(packet_bytes as u16).to_be_bytes());
                    framed.extend_from_slice(&raw_packet);
                    framed
                } else {
                    raw_packet
                };
                let provider_block_started = std::time::Instant::now();
                packetizer
                    .push(TimedTtsAudioChunk {
                        audio: provider_audio,
                        is_last,
                        relay_started: provider_block_started,
                    })
                    .unwrap()
            } else {
                vec![TimedTtsAudioChunk {
                    audio: raw_packet,
                    is_last,
                    relay_started: std::time::Instant::now(),
                }]
            };

            for chunk in chunks {
                let packet_started = chunk.relay_started;
                if chunk.audio.is_empty() || chunk.audio.len() != packet_bytes {
                    aggregated_packets += 1;
                }
                let event = StreamEvent::tts_audio_chunk(
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, chunk.audio),
                    0,
                    chunk.is_last,
                    profile,
                )
                .with_audio_relay_timing(profile, provider, packet_started);
                let sink = sink_tx.clone();
                send_client_event(&state, event, move |message| async move {
                    tokio::task::yield_now().await;
                    sink.send(message).await
                })
                .await
                .unwrap();
                let diagnostic = diagnostics.recv().await.unwrap();
                assert_eq!(diagnostic.stage, "voice_audio_downlink_relay_duration");
                assert_eq!(diagnostic.detail["format"], profile.format.as_str());
                assert_eq!(diagnostic.detail["sample_rate"], profile.sample_rate.hz());
                durations.push(diagnostic.detail["duration_micros"].as_u64().unwrap());
            }
        }
        drop(sink_tx);
        let mut sent_messages = 0usize;
        while let Some(message) = sink_rx.recv().await {
            let Message::Text(raw) = message else {
                panic!("downlink relay must send text events");
            };
            let event: Value = serde_json::from_str(raw.as_str()).unwrap();
            let decoded = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                event["payload"]["audio"].as_str().unwrap(),
            )
            .unwrap();
            assert_eq!(decoded.len(), packet_bytes);
            sent_messages += 1;
        }
        assert_eq!(sent_messages, packet_count);
        diagnostic_relay_result("downlink", profile, durations, aggregated_packets)
    }

    async fn benchmark_diagnostic_uplink(
        profile: AudioProfile,
        packet_count: usize,
    ) -> DiagnosticRelayBenchmarkResult {
        let state = test_app_state();
        let mut diagnostics = state.diagnostics.subscribe();
        let (sink_tx, mut sink_rx) =
            tokio::sync::mpsc::channel::<UpstreamMessage>(packet_count + 1);
        let packet_bytes = match (profile.format, profile.sample_rate) {
            (AudioFormat::Mp3, _) => 1_024,
            (AudioFormat::Speex, AudioSampleRate::Hz8000) => 38,
            (AudioFormat::Speex, AudioSampleRate::Hz16000) => 60,
            _ => panic!("unsupported uplink diagnostic benchmark profile: {profile:?}"),
        };
        let mut writer =
            LiveIatWriterState::new("benchmark".to_string(), profile, IatProvider::Standard);
        let mut durations = Vec::with_capacity(packet_count);
        let mut aggregated_packets = 0usize;

        for index in 0..packet_count {
            let server_entry_started = std::time::Instant::now();
            let (payload, is_end, relay_started) = writer
                .handle_frame(LiveAsrFrame::Audio {
                    audio: vec![index as u8; packet_bytes],
                    relay_started: server_entry_started,
                })
                .unwrap();
            assert!(!is_end);
            assert_eq!(relay_started, Some(server_entry_started));
            let sink = sink_tx.clone();
            tokio::task::yield_now().await;
            sink.send(UpstreamMessage::Text(payload.to_string().into()))
                .await
                .unwrap();
            emit_audio_relay_diagnostic(
                &state,
                None,
                None,
                None,
                "voice_audio_uplink_relay_duration",
                profile,
                "standard",
                relay_started.unwrap(),
            );
            let diagnostic = diagnostics.recv().await.unwrap();
            assert_eq!(diagnostic.stage, "voice_audio_uplink_relay_duration");
            durations.push(diagnostic.detail["duration_micros"].as_u64().unwrap());
        }
        drop(sink_tx);
        let mut sent_messages = 0usize;
        while let Some(message) = sink_rx.recv().await {
            let UpstreamMessage::Text(raw) = message else {
                panic!("uplink relay must send text frames");
            };
            let payload: Value = serde_json::from_str(raw.as_str()).unwrap();
            let decoded = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                payload["data"]["audio"].as_str().unwrap(),
            )
            .unwrap();
            if decoded.len() != packet_bytes || decoded.is_empty() {
                aggregated_packets += 1;
            }
            sent_messages += 1;
        }
        assert_eq!(sent_messages, packet_count);
        diagnostic_relay_result("uplink", profile, durations, aggregated_packets)
    }

    #[test]
    fn relay_microbenchmark_500_packets_keeps_compressed_p95_within_two_ms_of_mp3() {
        let mp3 = benchmark_relay_profile(
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            500,
        );
        assert_eq!(mp3.packet_count, 500);
        for profile in [
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
        ] {
            let compressed = benchmark_relay_profile(profile, 500);
            assert_eq!(compressed.packet_count, 500);
            assert_eq!(compressed.aggregated_packets, 0);
            assert!(
                compressed.p95_nanos <= mp3.p95_nanos + 2_000_000,
                "{} {}Hz p95={:.3}us exceeds mp3 p95={:.3}us + 2000us",
                profile.format.as_str(),
                profile.sample_rate.hz(),
                compressed.p95_nanos as f64 / 1_000.0,
                mp3.p95_nanos as f64 / 1_000.0
            );
        }
    }

    #[tokio::test]
    async fn production_relay_diagnostics_500_packets_preserve_boundaries_and_p95_budget() {
        let mp3 = benchmark_diagnostic_downlink(
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            500,
        )
        .await;
        for profile in [
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
        ] {
            let compressed = benchmark_diagnostic_downlink(profile, 500).await;
            assert_eq!(compressed.packet_count, 500);
            assert_eq!(compressed.aggregated_packets, 0);
            assert!(compressed.p95_micros <= mp3.p95_micros + 2_000);
        }

        let mp3_uplink = benchmark_diagnostic_uplink(
            AudioProfile::new(AudioFormat::Mp3, AudioSampleRate::Hz16000),
            500,
        )
        .await;
        for profile in [
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz8000),
            AudioProfile::new(AudioFormat::Speex, AudioSampleRate::Hz16000),
        ] {
            let compressed = benchmark_diagnostic_uplink(profile, 500).await;
            assert_eq!(compressed.packet_count, 500);
            assert_eq!(compressed.aggregated_packets, 0);
            assert!(compressed.p95_micros <= mp3_uplink.p95_micros + 2_000);
        }
    }

    #[test]
    fn timed_standard_packetizer_keeps_each_lookbehind_packet_original_arrival() {
        let profile = AudioProfile::new(AudioFormat::Opus, AudioSampleRate::Hz16000);
        let mut packetizer = TimedStandardTtsPacketizer::new(profile).unwrap();
        let first_started = std::time::Instant::now();
        assert!(packetizer
            .push(TimedTtsAudioChunk {
                audio: [vec![0, 2], vec![1, 2]].concat(),
                is_last: false,
                relay_started: first_started,
            })
            .unwrap()
            .is_empty());
        std::thread::sleep(std::time::Duration::from_millis(1));
        let second_started = std::time::Instant::now();
        let released = packetizer
            .push(TimedTtsAudioChunk {
                audio: [vec![0, 2], vec![3, 4]].concat(),
                is_last: true,
                relay_started: second_started,
            })
            .unwrap();
        assert_eq!(released.len(), 2);
        assert_eq!(released[0].relay_started, first_started);
        assert_eq!(released[1].relay_started, second_started);
        assert!(!released[0].is_last);
        assert!(released[1].is_last);
    }

    #[test]
    fn all_three_web_tts_consumers_use_propagated_relay_started() {
        let source = include_str!("mod.rs");
        let assignment = ["let relay_started = chunk.", "relay_started;"].concat();
        assert_eq!(source.matches(&assignment).count(), 3);
    }

    #[tokio::test]
    async fn replacing_and_closing_live_sessions_cancels_and_joins_owned_tasks() {
        let active = Arc::new(AtomicUsize::new(0));
        let mut session = Some(pending_live_session(active.clone()));

        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 1);

        stop_live_asr_session(&mut session).await.unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(session.is_none());

        session = Some(pending_live_session(active.clone()));
        tokio::task::yield_now().await;
        assert_eq!(active.load(Ordering::SeqCst), 1);

        stop_live_asr_session(&mut session).await.unwrap();
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(session.is_none());
    }

    #[tokio::test]
    async fn ending_input_keeps_task_owned_but_stops_accepting_chunks() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async move {
            assert!(matches!(receiver.recv().await, Some(LiveAsrFrame::End)));
            std::future::pending::<()>().await;
        });
        let mut session = LiveAsrSession::new(
            sender,
            task,
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
            IatProvider::Standard,
        );

        session.finish_input().await.unwrap();

        assert!(!session.is_accepting_audio());
        assert!(!session.task.as_ref().unwrap().is_finished());
        session.stop_and_join().await.unwrap();
    }

    #[tokio::test]
    async fn downstream_handoff_survives_stopping_the_finished_iat_session_once() {
        let active_iat = Arc::new(AtomicUsize::new(0));
        let mut session = Some(pending_live_session(active_iat.clone()));
        tokio::task::yield_now().await;
        let downstream_runs = Arc::new(AtomicUsize::new(0));
        let runs = downstream_runs.clone();
        let state = test_app_state();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let coordinator = WsTurnCoordinator::default();
        let turn_capacity = coordinator.try_reserve_turn_capacity().unwrap();
        let turn = RecognizedTurn::new(
            state,
            tx,
            "conversation-test".to_string(),
            "turn-test".to_string(),
            coordinator,
            turn_capacity,
            crate::db::ConversationOwner::Browser,
            "识别完成".to_string(),
            None,
            test_audio_context(),
        )
        .unwrap();
        let downstream = turn.handoff_with(move |_| async move {
            tokio::task::yield_now().await;
            runs.fetch_add(1, Ordering::SeqCst);
        });

        stop_live_asr_session(&mut session).await.unwrap();
        downstream.await.unwrap();

        assert_eq!(active_iat.load(Ordering::SeqCst), 0);
        assert_eq!(downstream_runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn finished_session_tasks_are_reaped_and_panics_are_observed() {
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let task = tokio::spawn(async { panic!("iat task panic") });
        let mut session = Some(LiveAsrSession::new(
            sender,
            task,
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
            IatProvider::Standard,
        ));
        tokio::task::yield_now().await;

        let error = reap_finished_live_asr_session(&mut session)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("iat task panic"));
        assert!(session.is_none());
    }

    #[tokio::test]
    async fn empty_recognition_does_not_create_a_downstream_handoff() {
        let state = test_app_state();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let coordinator = WsTurnCoordinator::default();
        let turn_capacity = coordinator.try_reserve_turn_capacity().unwrap();

        assert!(RecognizedTurn::new(
            state,
            tx,
            "conversation-test".to_string(),
            "turn-test".to_string(),
            coordinator,
            turn_capacity,
            crate::db::ConversationOwner::Browser,
            "   ".to_string(),
            None,
            test_audio_context(),
        )
        .is_none());
    }

    #[tokio::test]
    async fn coupled_live_io_cancels_the_other_future_on_either_failure() {
        let writer_dropped = Arc::new(AtomicUsize::new(0));
        let writer_signal = writer_dropped.clone();
        let writer = async move {
            writer_signal.fetch_add(1, Ordering::SeqCst);
            let _guard = ActiveGuard(writer_signal);
            std::future::pending::<anyhow::Result<usize>>().await
        };
        let reader = async { Err::<String, _>(anyhow::anyhow!("reader failed")) };

        assert!(couple_live_iat_io(writer, reader).await.is_err());
        assert_eq!(writer_dropped.load(Ordering::SeqCst), 0);

        let reader_dropped = Arc::new(AtomicUsize::new(0));
        let reader_signal = reader_dropped.clone();
        let writer = async { Err::<usize, _>(anyhow::anyhow!("writer failed")) };
        let reader = async move {
            reader_signal.fetch_add(1, Ordering::SeqCst);
            let _guard = ActiveGuard(reader_signal);
            std::future::pending::<anyhow::Result<String>>().await
        };

        assert!(couple_live_iat_io(writer, reader).await.is_err());
        assert_eq!(reader_dropped.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn live_writer_rejects_end_before_any_audio_frame() {
        let mut writer = LiveIatWriterState::new(
            "app".to_string(),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
            IatProvider::Standard,
        );

        let error = writer.handle_frame(LiveAsrFrame::End).unwrap_err();

        assert_eq!(
            error.downcast_ref::<AudioPacketError>().unwrap().code(),
            "invalid_audio_packet"
        );
    }

    #[test]
    fn live_writer_is_built_from_the_connection_audio_context() {
        let mut context = test_audio_context();
        context.audio = crate::domain::audio::VoiceConnectionAudio::from_query(
            Some("speex"),
            Some("8000"),
            Some("opus"),
            Some("16000"),
        )
        .unwrap();
        context.iat_provider = IatProvider::Standard;
        context.tts_provider = TtsProvider::Standard;
        context.config.app_id = "snapshot-app".to_string();
        context.config.iat_provider = "standard".to_string();
        context.config.tts_provider = "standard".to_string();
        let mut writer = LiveIatWriterState::from_audio_context(&context);

        let (payload, is_end, _) = writer
            .handle_frame(LiveAsrFrame::Audio {
                audio: vec![0; 38],
                relay_started: std::time::Instant::now(),
            })
            .unwrap();

        assert!(!is_end);
        assert_eq!(writer.profile, context.audio.input);
        assert_eq!(writer.provider, IatProvider::Standard);
        assert_eq!(payload["common"]["app_id"], "snapshot-app");
        assert_eq!(payload["data"]["encoding"], "speex");
        assert_eq!(payload["data"]["format"], "audio/L16;rate=8000");
    }

    #[test]
    fn live_writer_rejects_channel_close_before_end() {
        let mut writer = LiveIatWriterState::new(
            "app".to_string(),
            AudioProfile::new(AudioFormat::Pcm, AudioSampleRate::Hz16000),
            IatProvider::Standard,
        );
        writer
            .handle_frame(LiveAsrFrame::Audio {
                audio: vec![1, 2],
                relay_started: std::time::Instant::now(),
            })
            .unwrap();

        let error = writer.channel_closed().unwrap_err();

        assert_eq!(
            error.downcast_ref::<AudioPacketError>().unwrap().code(),
            "invalid_audio_packet"
        );
    }

    #[tokio::test]
    async fn device_button_interrupt_analysis_permits_follow_handoff_order_not_poll_order() {
        let coordinator = WsTurnCoordinator::default();
        let first = coordinator.try_reserve_analysis().unwrap();
        let mut second = Box::pin(coordinator.try_reserve_analysis().unwrap().acquire());

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );
        let first_guard = first.acquire().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );

        drop(first_guard);
        let _second_guard = tokio::time::timeout(std::time::Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn device_button_interrupt_analysis_capacity_rejects_n_plus_one_without_waiting() {
        let coordinator = WsTurnCoordinator::new_with_capacities(super::WS_TURN_CAPACITY, 2);
        let _first = coordinator.try_reserve_analysis().unwrap();
        let _second = coordinator.try_reserve_analysis().unwrap();

        assert!(coordinator.try_reserve_analysis().is_err());
    }

    #[tokio::test]
    async fn device_button_interrupt_turn_capacity_rejects_sixty_fifth_complete_turn() {
        let coordinator = WsTurnCoordinator::new_with_capacities(64, 64);
        let _running_turns = (0..64)
            .map(|_| coordinator.try_reserve_turn_capacity().unwrap())
            .collect::<Vec<_>>();

        assert!(coordinator.try_reserve_turn_capacity().is_err());
    }

    #[tokio::test]
    async fn device_button_interrupt_turn_capacity_lives_until_output_marker_is_enqueued() {
        let state = initialized_test_app_state().await;
        let mut config = crate::db::get_config(&state.pool).await.unwrap();
        config.mock_providers = true;
        crate::db::save_config(&state.pool, &config).await.unwrap();
        let mut audio_context = test_audio_context();
        audio_context.config = config;
        let coordinator = WsTurnCoordinator::new_with_capacities(64, 2);
        let _other_in_flight_turns = (0..63)
            .map(|_| coordinator.try_reserve_turn_capacity().unwrap())
            .collect::<Vec<_>>();
        let turn_capacity = coordinator.try_reserve_turn_capacity().unwrap();
        let analysis_permit = coordinator.try_reserve_analysis().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let state_for_turn = state.clone();
        let coordinator_for_turn = coordinator.clone();
        let turn = tokio::spawn(async move {
            super::run_turn_to_channel(
                state_for_turn,
                tx,
                "conversation-1".to_string(),
                "turn-1".to_string(),
                coordinator_for_turn,
                turn_capacity,
                analysis_permit,
                crate::db::ConversationOwner::Browser,
                "买一瓶水".to_string(),
                None,
                audio_context,
            )
            .await;
        });

        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if event.event_type == "voice_done" {
                break;
            }
        }
        tokio::task::yield_now().await;
        assert!(
            coordinator.try_reserve_turn_capacity().is_err(),
            "voice reply completion must not release capacity before the output marker"
        );

        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if event.event_type == "_turn_output_finished" {
                break;
            }
        }
        turn.await.unwrap();
        assert!(coordinator.try_reserve_turn_capacity().is_ok());
    }

    #[tokio::test]
    async fn device_button_interrupt_reply_failure_waits_for_analysis_before_marker_and_release() {
        let coordinator = WsTurnCoordinator::new_with_capacities(1, 1);
        let turn_capacity = coordinator.try_reserve_turn_capacity().unwrap();
        let turn_hold = turn_capacity.hold();
        let (analysis_started_tx, analysis_started_rx) = tokio::sync::oneshot::channel();
        let (analysis_release_tx, analysis_release_rx) = tokio::sync::oneshot::channel();
        let analysis = tokio::spawn(async move {
            let _ = analysis_started_tx.send(());
            let _ = analysis_release_rx.await;
            Ok::<_, super::ApiError>(vec![StreamEvent::new(
                "order_analysis_complete",
                serde_json::json!({}),
            )])
        });
        let emitted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured = emitted.clone();
        let (marker_tx, mut marker_rx) = tokio::sync::mpsc::channel(1);
        let turn = tokio::spawn(async move {
            let mut emit = move |event: StreamEvent| {
                let captured = captured.clone();
                async move {
                    captured.lock().await.push(event.event_type);
                }
            };
            let reply_result = Err(super::ApiError::from(anyhow::anyhow!(
                "controlled reply failure"
            )));
            let result = super::settle_turn_reply_and_analysis(
                analysis,
                reply_result,
                "conversation-1",
                "turn-1",
                &mut emit,
            )
            .await;
            marker_tx.send("_turn_output_finished").await.unwrap();
            drop(turn_hold);
            drop(turn_capacity);
            result
        });

        analysis_started_rx.await.unwrap();
        assert!(coordinator.try_reserve_turn_capacity().is_err());
        assert!(matches!(
            marker_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));

        analysis_release_tx.send(()).unwrap();
        assert_eq!(marker_rx.recv().await.unwrap(), "_turn_output_finished");
        let error = turn.await.unwrap().unwrap_err();
        assert_eq!(error.message, "controlled reply failure");
        assert_eq!(emitted.lock().await.as_slice(), ["order_analysis_complete"]);
        let _released = coordinator.try_reserve_turn_capacity().unwrap();
    }

    #[tokio::test]
    async fn device_button_interrupt_analysis_ticket_is_reserved_only_at_recognized_handoff() {
        let coordinator = WsTurnCoordinator::new_with_capacities(1, 1);
        let _occupied_analysis = coordinator.try_reserve_analysis().unwrap();
        let turn_capacity = coordinator.try_reserve_turn_capacity().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let turn = RecognizedTurn::new(
            test_app_state(),
            tx,
            "conversation-1".to_string(),
            "turn-1".to_string(),
            coordinator.clone(),
            turn_capacity,
            crate::db::ConversationOwner::Browser,
            "识别完成".to_string(),
            None,
            test_audio_context(),
        )
        .expect("ASR completion must not reserve an analysis ticket before handoff");

        turn.handoff();

        let error = rx.recv().await.unwrap();
        assert_eq!(error.event_type, "error");
        assert_eq!(error.payload["code"], "analysis_queue_full");
        assert!(
            coordinator.try_reserve_turn_capacity().is_ok(),
            "failed handoff must release the complete-turn reservation"
        );
    }

    #[tokio::test]
    async fn device_button_interrupt_analysis_boundary_prevents_snapshot_pollution() {
        let state = initialized_test_app_state().await;
        let coordinator = WsTurnCoordinator::new_with_capacities(super::WS_TURN_CAPACITY, 2);
        let owner = crate::db::ConversationOwner::Browser;
        let first = super::prepare_analysis_turn(
            &state,
            "conversation-1",
            "turn-1",
            &owner,
            "第一轮",
            coordinator.try_reserve_analysis().unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(first.round_text, "第一轮");

        let second_state = state.clone();
        let second_owner = owner.clone();
        let second_permit = coordinator.try_reserve_analysis().unwrap();
        let mut second = tokio::spawn(async move {
            super::prepare_analysis_turn(
                &second_state,
                "conversation-1",
                "turn-2",
                &second_owner,
                "第二轮",
                second_permit,
            )
            .await
        });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut second)
                .await
                .is_err(),
            "later turns must wait before appending or querying"
        );
        let second_message_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_messages WHERE turn_id = 'turn-2'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!(second_message_count, 0);

        crate::db::log_event(
            &state.pool,
            "conversation-1",
            "turn-1",
            "order_created",
            &serde_json::json!({}),
        )
        .await;
        drop(first);

        let second = tokio::time::timeout(std::time::Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(second.round_text, "第二轮");
    }

    #[tokio::test]
    async fn device_button_interrupt_control_ack_has_priority_over_reply_backpressure() {
        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(1);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(1);
        reply_tx
            .send(StreamEvent::new("tts_audio_chunk", serde_json::json!({})))
            .await
            .unwrap();
        control_tx
            .try_send(StreamEvent::new("tts_interrupted", serde_json::json!({})))
            .unwrap();

        let event = super::next_ws_output_event(&mut control_rx, &mut reply_rx)
            .await
            .unwrap();

        assert_eq!(event.event_type, "tts_interrupted");
    }

    #[tokio::test]
    async fn device_button_interrupt_already_finished_ack_filters_queued_final_packets() {
        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel(5);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel(1);
        for event_type in [
            "llm_delta",
            "reply_sentence",
            "tts_audio_chunk",
            "voice_done",
        ] {
            reply_tx
                .send(
                    StreamEvent::new(event_type, serde_json::json!({}))
                        .with_context("conversation-1", "turn-1"),
                )
                .await
                .unwrap();
        }
        reply_tx
            .send(
                StreamEvent::new("_turn_output_finished", serde_json::json!({}))
                    .with_context("conversation-1", "turn-1"),
            )
            .await
            .unwrap();
        control_tx
            .send(
                StreamEvent::new(
                    "tts_interrupted",
                    serde_json::json!({"source":"button", "status":"already_finished"}),
                )
                .with_context("conversation-1", "turn-1"),
            )
            .await
            .unwrap();
        drop(reply_tx);
        drop(control_tx);

        let mut filter = super::InterruptedOutputFilter::default();
        let mut delivered = Vec::new();
        while let Some(event) = super::next_ws_output_event(&mut control_rx, &mut reply_rx).await {
            if let Some(event) = super::filter_ws_output_event(&mut filter, event) {
                delivered.push(event.event_type);
            }
        }

        assert_eq!(delivered, vec!["tts_interrupted"]);
    }

    #[tokio::test]
    async fn device_button_interrupt_control_queue_is_bounded_and_nonblocking() {
        let (control_tx, _control_rx) = tokio::sync::mpsc::channel(1);

        assert!(super::try_enqueue_ws_control(
            &control_tx,
            StreamEvent::new("tts_interrupted", serde_json::json!({}))
        ));
        assert!(!super::try_enqueue_ws_control(
            &control_tx,
            StreamEvent::new("tts_interrupted", serde_json::json!({}))
        ));
    }

    #[test]
    fn device_button_interrupt_output_filter_lives_until_turn_marker_without_blind_eviction() {
        let mut filter = super::InterruptedOutputFilter::default();
        for index in 0..65 {
            filter.interrupt(format!("turn-{index}"));
        }

        assert!(filter.suppresses("turn-0", "tts_audio_chunk"));
        filter.finish("turn-0");
        assert!(!filter.suppresses("turn-0", "tts_audio_chunk"));
        filter.interrupt("turn-0".to_string());
        assert!(
            !filter.suppresses("turn-0", "tts_audio_chunk"),
            "a delayed acknowledgement must not reopen a finished output filter"
        );
    }

    #[tokio::test]
    async fn device_button_interrupt_standard_fallback_stops_after_running_provider_is_cancelled() {
        let state = initialized_test_app_state().await;
        let (llm_endpoint, _llm_entered, llm_release, llm_closed, llm_server) =
            spawn_controlled_ws_provider(Some(serde_json::json!({
                "header": {"code": 500, "message": "controlled test failure"}
            })))
            .await;
        let (tts_endpoint, tts_entered, _tts_release, tts_closed, tts_server) =
            spawn_controlled_ws_provider(None).await;
        let mut config = crate::db::get_config(&state.pool).await.unwrap();
        config.mock_providers = false;
        config.app_id = "test-app".to_string();
        config.api_key = "test-key".to_string();
        config.api_secret = "test-secret".to_string();
        config.llm_endpoint = llm_endpoint;
        config.tts_provider = "standard".to_string();
        config.tts_standard_endpoint = tts_endpoint;
        crate::db::save_config(&state.pool, &config).await.unwrap();
        let mut audio_context = test_audio_context();
        audio_context.config = config.clone();
        audio_context.tts_provider = TtsProvider::Standard;
        let emitted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured = emitted.clone();
        let emit = move |event: StreamEvent| {
            let captured = captured.clone();
            async move {
                captured.lock().await.push(event.event_type);
            }
        };
        let coordinator = WsTurnCoordinator::new_with_capacities(1, 64);
        let ticket = coordinator.try_reserve_analysis().unwrap();
        let turn_capacity = coordinator.try_reserve_turn_capacity().unwrap();
        let turn_hold = turn_capacity.hold();
        let (cancel_tx, cancellation) = tokio::sync::watch::channel(false);
        let turn_state = state.clone();
        let turn = tokio::spawn(async move {
            let result = super::run_turn_with_interrupt(
                &turn_state,
                "conversation-1",
                "turn-1",
                &crate::db::ConversationOwner::Browser,
                "买一瓶水",
                None,
                audio_context,
                cancellation,
                ticket,
                turn_hold,
                emit,
            )
            .await;
            drop(turn_capacity);
            result
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), tts_entered)
            .await
            .expect("standard fallback TTS provider was never entered")
            .unwrap();
        let before_cancel = emitted.lock().await;
        assert!(before_cancel.iter().any(|event| event == "reply_sentence"));
        let cancel_index = before_cancel.len();
        drop(before_cancel);
        cancel_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), turn)
            .await
            .expect("standard fallback did not stop after cancellation")
            .unwrap()
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), tts_closed)
            .await
            .expect("standard provider connection stayed open after cancellation")
            .unwrap();
        let _recovered = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_turn_capacity(&coordinator),
        )
        .await
        .expect("turn capacity was not released after standard provider cancellation");
        let _ = llm_release.send(());
        let _ = llm_closed.await;
        let _ = llm_server.await;
        let _ = tts_server.await;
        let emitted = emitted.lock().await;
        assert!(emitted[cancel_index..]
            .iter()
            .all(|event_type| !forbidden_reply_event(event_type)));
        let assistant_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_messages WHERE turn_id = 'turn-1' AND role = 'assistant'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!(assistant_count, 0);
    }

    #[tokio::test]
    async fn device_button_interrupt_super_smart_stream_stops_after_running_provider_is_cancelled()
    {
        let state = initialized_test_app_state().await;
        let (llm_endpoint, _llm_entered, llm_release, llm_closed, llm_server) =
            spawn_controlled_ws_provider(Some(serde_json::json!({
                "header": {"code": 0},
                "payload": {
                    "choices": {
                        "status": 0,
                        "text": [{"content": "这是正在播报的测试。"}]
                    }
                }
            })))
            .await;
        let (tts_endpoint, tts_entered, _tts_release, tts_closed, tts_server) =
            spawn_controlled_ws_provider(None).await;
        let mut config = crate::db::get_config(&state.pool).await.unwrap();
        config.mock_providers = false;
        config.app_id = "test-app".to_string();
        config.api_key = "test-key".to_string();
        config.api_secret = "test-secret".to_string();
        config.llm_endpoint = llm_endpoint;
        config.tts_provider = "super_smart".to_string();
        config.tts_endpoint = tts_endpoint;
        crate::db::save_config(&state.pool, &config).await.unwrap();
        let mut audio_context = test_audio_context();
        audio_context.config = config;
        audio_context.tts_provider = TtsProvider::SuperSmart;
        let emitted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured = emitted.clone();
        let emit = move |event: StreamEvent| {
            let captured = captured.clone();
            async move {
                captured.lock().await.push(event.event_type);
            }
        };
        let coordinator = WsTurnCoordinator::new_with_capacities(1, 64);
        let ticket = coordinator.try_reserve_analysis().unwrap();
        let turn_capacity = coordinator.try_reserve_turn_capacity().unwrap();
        let turn_hold = turn_capacity.hold();
        let (cancel_tx, cancellation) = tokio::sync::watch::channel(false);
        let turn_state = state.clone();
        let turn = tokio::spawn(async move {
            let result = super::run_turn_with_interrupt(
                &turn_state,
                "conversation-1",
                "turn-1",
                &crate::db::ConversationOwner::Browser,
                "买一瓶水",
                None,
                audio_context,
                cancellation,
                ticket,
                turn_hold,
                emit,
            )
            .await;
            drop(turn_capacity);
            result
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), tts_entered)
            .await
            .expect("super-smart streaming TTS provider was never entered")
            .unwrap();
        let before_cancel = emitted.lock().await;
        assert!(before_cancel.iter().any(|event| event == "llm_delta"));
        assert!(before_cancel.iter().any(|event| event == "reply_sentence"));
        let cancel_index = before_cancel.len();
        drop(before_cancel);
        cancel_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), turn)
            .await
            .expect("super-smart stream did not stop after cancellation")
            .unwrap()
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(1), tts_closed)
            .await
            .expect("super-smart provider connection stayed open after cancellation")
            .unwrap();
        let _ = llm_closed.await;
        let _ = llm_release.send(());
        let _ = llm_server.await;
        let _ = tts_server.await;
        let _recovered = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_turn_capacity(&coordinator),
        )
        .await
        .expect("turn capacity was not released after super-smart provider exit");
        let emitted = emitted.lock().await;
        assert!(emitted[cancel_index..]
            .iter()
            .all(|event_type| !forbidden_reply_event(event_type)));
        let assistant_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_messages WHERE turn_id = 'turn-1' AND role = 'assistant'",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap();
        assert_eq!(assistant_count, 0);
    }
}
