use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    extract::{connect_info::ConnectInfo, Extension, Path, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use cookie::{Cookie, SameSite};
use rand::Rng;
use serde::Deserialize;
use serde_json::json;

use crate::{admin_auth, db};

use super::{is_internal_management_path, is_trusted_internal_source, AppState};

const LOGIN_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_LOGIN_FAILURES: usize = 5;
const MAX_LOGIN_SOURCES: usize = 2048;
const DEVICE_SECRET_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

static LOGIN_FAILURES: OnceLock<Mutex<HashMap<String, Vec<Instant>>>> = OnceLock::new();

#[derive(Clone)]
pub(super) enum AdminPrincipal {
    Local,
    Session(String),
}

#[derive(Clone)]
struct LoginSource(String);

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/admin/auth/login", post(login))
        .route("/api/admin/auth/logout", post(logout))
        .route("/api/admin/auth/me", get(me))
        .route(
            "/api/admin/device-authorizations",
            get(list_device_authorizations).post(create_device_authorization),
        )
        .route(
            "/api/admin/device-authorizations/{device_id}",
            put(update_device_authorization),
        )
        .route(
            "/api/admin/device-authorizations/{device_id}/reset-secret",
            post(reset_device_secret),
        )
}

pub(super) async fn require_admin_access(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    if !is_internal_management_path(request.uri().path()) {
        return next.run(request).await;
    }

    let connect_info = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .copied();
    let trusted_local = is_trusted_internal_source(connect_info, request.headers());
    let source = login_source(&state, connect_info, request.headers());
    request.extensions_mut().insert(LoginSource(source));

    if trusted_local {
        request.extensions_mut().insert(AdminPrincipal::Local);
        return next.run(request).await;
    }

    if request.uri().path() == "/api/admin/auth/login" {
        if !has_same_host_origin(request.headers()) {
            return error_response(StatusCode::FORBIDDEN, "invalid_origin");
        }
        return next.run(request).await;
    }

    let Some(token) = session_cookie(request.headers()) else {
        return error_response(StatusCode::UNAUTHORIZED, "login_required");
    };
    match admin_auth::load_session(&state.pool, &state.admin_config, &token).await {
        Ok(true) => {
            if is_state_changing(request.method()) && !has_same_host_origin(request.headers()) {
                return error_response(StatusCode::FORBIDDEN, "invalid_origin");
            }
            request
                .extensions_mut()
                .insert(AdminPrincipal::Session(token));
            next.run(request).await
        }
        Ok(false) => error_response(StatusCode::UNAUTHORIZED, "login_required"),
        Err(error) => {
            tracing::error!(%error, "failed to load admin session");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}

fn is_state_changing(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn has_same_host_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next().and_then(|value| value.to_str().ok()) else {
        return false;
    };
    if origins.next().is_some() {
        return false;
    }
    let Ok(origin) = url::Url::parse(origin) else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || origin
            .host_str()
            .is_none_or(|host| !host.eq_ignore_ascii_case(authority.host()))
    {
        return false;
    }
    match (authority.port_u16(), origin.port_or_known_default()) {
        (Some(request_port), Some(origin_port)) => request_port == origin_port,
        (None, Some(80)) if origin.scheme() == "http" => true,
        (None, Some(443)) if origin.scheme() == "https" => true,
        _ => false,
    }
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(name, value)| {
            (name == admin_auth::ADMIN_COOKIE && !value.is_empty()).then(|| value.to_string())
        })
}

fn login_source(
    state: &AppState,
    connect_info: Option<ConnectInfo<SocketAddr>>,
    headers: &HeaderMap,
) -> String {
    let peer_ip = connect_info.map(|ConnectInfo(address)| address.ip());
    let proxied_ip = peer_ip.filter(|ip| ip.is_loopback()).and_then(|_| {
        let mut values = headers.get_all("x-real-ip").iter();
        let first = values
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<std::net::IpAddr>().ok());
        (values.next().is_none()).then_some(first).flatten()
    });
    let public_source = proxied_ip
        .or(peer_ip)
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!("{}:{public_source}", state.admin_config.fingerprint)
}

fn login_is_rate_limited(source: &str) -> bool {
    let now = Instant::now();
    let mut failures = LOGIN_FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    prune_and_limit_login_failures(&mut failures, now);
    failures
        .get(source)
        .is_some_and(|attempts| attempts.len() >= MAX_LOGIN_FAILURES)
}

fn record_login_failure(source: &str) {
    let now = Instant::now();
    let mut failures = LOGIN_FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    failures.entry(source.to_string()).or_default().push(now);
    prune_and_limit_login_failures(&mut failures, now);
}

fn prune_and_limit_login_failures(failures: &mut HashMap<String, Vec<Instant>>, now: Instant) {
    failures.retain(|_, attempts| {
        attempts.retain(|attempt| now.duration_since(*attempt) < LOGIN_WINDOW);
        !attempts.is_empty()
    });
    while failures.len() > MAX_LOGIN_SOURCES {
        let Some(oldest_source) = failures
            .iter()
            .min_by_key(|(_, attempts)| attempts.last().copied())
            .map(|(source, _)| source.clone())
        else {
            break;
        };
        failures.remove(&oldest_source);
    }
}

fn clear_login_failures(source: &str) {
    LOGIN_FAILURES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(source);
}

#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    Extension(source): Extension<LoginSource>,
    Json(request): Json<LoginRequest>,
) -> Response {
    if login_is_rate_limited(&source.0) {
        return error_response(StatusCode::TOO_MANY_REQUESTS, "login_rate_limited");
    }

    let password_matches =
        admin_auth::verify_password(&state.admin_config.password_hash, &request.password);
    if request.username != *state.admin_config.username || !password_matches {
        record_login_failure(&source.0);
        return error_response(StatusCode::UNAUTHORIZED, "invalid_credentials");
    }
    clear_login_failures(&source.0);

    match admin_auth::create_session(&state.pool, &state.admin_config).await {
        Ok(token) => {
            let cookie = Cookie::build((admin_auth::ADMIN_COOKIE, token))
                .path("/")
                .http_only(true)
                .secure(state.admin_config.secure_cookie)
                .same_site(SameSite::Strict)
                .build();
            (
                StatusCode::OK,
                [(header::SET_COOKIE, cookie.to_string())],
                Json(json!({"username": state.admin_config.username.as_str()})),
            )
                .into_response()
        }
        Err(error) => {
            tracing::error!(%error, "failed to create admin session");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}

async fn logout(
    State(state): State<AppState>,
    Extension(principal): Extension<AdminPrincipal>,
) -> Response {
    if let AdminPrincipal::Session(token) = principal {
        if let Err(error) = admin_auth::revoke_session(&state.pool, &token).await {
            tracing::error!(%error, "failed to revoke admin session");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
        }
    }
    let mut cookie = Cookie::build((admin_auth::ADMIN_COOKIE, ""))
        .path("/")
        .http_only(true)
        .secure(state.admin_config.secure_cookie)
        .same_site(SameSite::Strict)
        .build();
    cookie.make_removal();
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie.to_string())],
        Json(json!({"ok": true})),
    )
        .into_response()
}

async fn me(
    State(state): State<AppState>,
    Extension(principal): Extension<AdminPrincipal>,
) -> Json<serde_json::Value> {
    Json(json!({
        "username": state.admin_config.username.as_str(),
        "local": matches!(principal, AdminPrincipal::Local),
    }))
}

async fn list_device_authorizations(State(state): State<AppState>) -> Response {
    match db::list_device_authorizations(&state.pool).await {
        Ok(devices) => Json(json!(devices)).into_response(),
        Err(error) => {
            tracing::error!(%error, "failed to list device authorizations");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}

#[derive(Deserialize)]
struct CreateDeviceAuthorizationRequest {
    device_id: String,
    name: String,
}

async fn create_device_authorization(
    State(state): State<AppState>,
    Json(request): Json<CreateDeviceAuthorizationRequest>,
) -> Response {
    let device_id = request.device_id.trim();
    let name = request.name.trim();
    if device_id.is_empty() || name.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let secret = generate_device_secret();
    match db::create_device_authorization(&state.pool, device_id, name, &secret).await {
        Ok(()) => (
            StatusCode::CREATED,
            Json(json!({
                "device_id": device_id,
                "name": name,
                "enabled": true,
                "device_secret": secret,
            })),
        )
            .into_response(),
        Err(error) if is_unique_violation(&error) => {
            error_response(StatusCode::CONFLICT, "device_already_exists")
        }
        Err(error) => {
            tracing::error!(%error, "failed to create device authorization");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}

#[derive(Deserialize)]
struct UpdateDeviceAuthorizationRequest {
    name: String,
    enabled: bool,
}

async fn update_device_authorization(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(request): Json<UpdateDeviceAuthorizationRequest>,
) -> Response {
    let name = request.name.trim();
    if name.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "invalid_request");
    }
    match db::update_device_authorization(&state.pool, &device_id, name, request.enabled).await {
        Ok(true) => Json(json!({
            "device_id": device_id,
            "name": name,
            "enabled": request.enabled,
        }))
        .into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "device_not_found"),
        Err(error) => {
            tracing::error!(%error, "failed to update device authorization");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}

#[derive(Deserialize)]
struct ResetDeviceSecretRequest {
    #[serde(default)]
    confirm: bool,
}

async fn reset_device_secret(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(request): Json<ResetDeviceSecretRequest>,
) -> Response {
    if !request.confirm {
        return error_response(StatusCode::BAD_REQUEST, "confirmation_required");
    }
    let secret = generate_device_secret();
    match db::reset_device_secret(&state.pool, &device_id, &secret).await {
        Ok(true) => Json(json!({
            "device_id": device_id,
            "device_secret": secret,
        }))
        .into_response(),
        Ok(false) => error_response(StatusCode::NOT_FOUND, "device_not_found"),
        Err(error) => {
            tracing::error!(%error, "failed to reset device secret");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
        }
    }
}

fn generate_device_secret() -> String {
    let mut rng = rand::rng();
    (0..24)
        .map(|_| {
            let index = rng.random_range(0..DEVICE_SECRET_ALPHABET.len());
            DEVICE_SECRET_ALPHABET[index] as char
        })
        .collect()
}

fn is_unique_violation(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<sqlx::Error>()
        .is_some_and(|error| match error {
            sqlx::Error::Database(database) => database.is_unique_violation(),
            _ => false,
        })
}

fn error_response(status: StatusCode, error: &'static str) -> Response {
    (status, Json(json!({"error": error}))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn public_peer_cannot_spoof_login_rate_limit_source() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let state = AppState {
            pool,
            server_secret: Arc::new("test-secret".to_string()),
            admin_config: crate::admin_auth::AdminConfig::new(
                "myjadmin",
                crate::admin_auth::hash_password("test-password").unwrap(),
            )
            .unwrap()
            .with_secure_cookie(false),
            diagnostics: tokio::sync::broadcast::channel(1).0,
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "198.51.100.99".parse().unwrap());
        let source = login_source(
            &state,
            Some(ConnectInfo("203.0.113.42:4567".parse().unwrap())),
            &headers,
        );
        assert!(source.ends_with(":203.0.113.42"));
    }

    #[test]
    fn login_failure_sources_are_pruned_and_capacity_limited() {
        let now = Instant::now();
        let mut failures = HashMap::new();
        failures.insert(
            "expired".to_string(),
            vec![now - LOGIN_WINDOW - Duration::from_secs(1)],
        );
        for index in 0..(MAX_LOGIN_SOURCES + 10) {
            failures.insert(
                format!("source-{index}"),
                vec![
                    now - Duration::from_millis(
                        (MAX_LOGIN_SOURCES - index.min(MAX_LOGIN_SOURCES)) as u64,
                    ),
                ],
            );
        }

        prune_and_limit_login_failures(&mut failures, now);

        assert_eq!(failures.len(), MAX_LOGIN_SOURCES);
        assert!(!failures.contains_key("expired"));
        assert!(failures.contains_key(&format!("source-{}", MAX_LOGIN_SOURCES + 9)));
    }
}
