use std::{
    io::Write,
    net::SocketAddr,
    process::{Command, Stdio},
    str::FromStr,
    sync::Arc,
};

use axum::{
    body::{to_bytes, Body},
    extract::{connect_info::ConnectInfo, Extension},
    http::{Request, StatusCode},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use mjy_voice_shop_rs::{
    admin_auth::{hash_password, AdminConfig},
    db,
    domain::{
        device_auth::{issue_device_token, secret_hash},
        matching::Product,
    },
    web::{router as production_router, AppState},
};
use serde_json::{json, Value};
use sqlx::{sqlite::SqliteConnectOptions, Row, SqlitePool};
use tempfile::tempdir;
use tower::ServiceExt;

async fn test_state() -> AppState {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join(format!("test-{}.db", uuid::Uuid::new_v4()));
    let _kept = dir.keep();
    let url = format!("sqlite://{}", db_path.display());
    let options = SqliteConnectOptions::from_str(&url)
        .unwrap()
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await.unwrap();
    db::init(&pool).await.unwrap();
    let mut config = db::get_config(&pool).await.unwrap();
    config.mock_providers = true;
    db::save_config(&pool, &config).await.unwrap();
    mjy_voice_shop_rs::admin_auth::init_schema(&pool)
        .await
        .unwrap();
    AppState {
        pool,
        server_secret: Arc::new("test-server-secret".to_string()),
        admin_config: AdminConfig::new("myjadmin", hash_password("test-admin-password").unwrap())
            .unwrap()
            .with_secure_cookie(false),
        diagnostics: tokio::sync::broadcast::channel(256).0,
    }
}

fn router(state: AppState) -> Router {
    production_router(state).layer(Extension(ConnectInfo(SocketAddr::from((
        [127, 0, 0, 1],
        0,
    )))))
}

#[tokio::test]
async fn mcp_catalog_sync_replaces_legacy_mock_products_and_keeps_manual_products() {
    let state = test_state().await;
    db::upsert_product(
        &state.pool,
        &Product {
            id: "manual-snack".to_string(),
            name: "手工维护商品".to_string(),
            aliases: vec!["手工商品".to_string()],
            spec: "件".to_string(),
            price: 9.9,
        },
    )
    .await
    .unwrap();
    let removed = db::replace_mcp_catalog_products(
        &state.pool,
        &[Product {
            id: "mcp-latte".to_string(),
            name: "拿铁".to_string(),
            aliases: vec!["拿铁".to_string()],
            spec: "冷".to_string(),
            price: 24.0,
        }],
    )
    .await
    .unwrap();

    let products = db::list_products(&state.pool).await.unwrap();
    assert_eq!(removed, 3);
    assert!(products.iter().any(|product| product.id == "mcp-latte"));
    assert!(products.iter().any(|product| product.id == "manual-snack"));
    assert!(!products.iter().any(|product| product.id == "cola-500"));
}

#[tokio::test]
async fn admin_entrypoint_busts_stale_assets_and_requires_revalidation() {
    let app = router(test_state().await);
    let response = app
        .oneshot(Request::get("/admin.html").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );

    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    for asset in [
        "styles.css?v=20260730-admin-data",
        "admin-time.js?v=20260730-admin-data",
        "admin-auth.js?v=20260730-admin-data",
        "admin.js?v=20260730-admin-data",
    ] {
        assert!(html.contains(asset), "missing cache-busted asset: {asset}");
    }
}

#[test]
fn admin_password_cli_generate_outputs_only_lowercase_hex() {
    let output = Command::new(env!("CARGO_BIN_EXE_mjy-admin-password"))
        .arg("generate")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let password = stdout.strip_suffix('\n').unwrap();
    assert_eq!(stdout.lines().count(), 1);
    assert_eq!(password.len(), 24);
    assert!(password
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    assert!(!password.contains('/'));
}

#[test]
fn admin_password_cli_hash_outputs_only_valid_argon2id_hash() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mjy-admin-password"))
        .arg("hash")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"correct-pass\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let hash = stdout.strip_suffix('\n').unwrap();
    assert_eq!(stdout.lines().count(), 1);
    assert!(hash.starts_with("$argon2id$"));
    assert!(mjy_voice_shop_rs::admin_auth::verify_password(
        hash,
        "correct-pass"
    ));
}

#[test]
fn admin_password_cli_rejects_unknown_subcommands_with_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_mjy-admin-password"))
        .arg("unknown")
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("usage: mjy-admin-password <hash|generate>"));
}

#[tokio::test]
async fn migration_preserves_legacy_conversations_and_adds_device_owner() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("legacy.db");
    let url = format!("sqlite://{}", db_path.display());
    let options = SqliteConnectOptions::from_str(&url)
        .unwrap()
        .create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await.unwrap();

    sqlx::query(
        "CREATE TABLE conversations (conversation_id TEXT PRIMARY KEY, created_at TEXT NOT NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO conversations(conversation_id, created_at) VALUES(?, ?)")
        .bind("legacy")
        .bind("2026-07-28T00:00:00Z")
        .execute(&pool)
        .await
        .unwrap();

    db::init(&pool).await.unwrap();
    db::init(&pool).await.unwrap();

    let columns = sqlx::query("PRAGMA table_info(conversations)")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        columns
            .iter()
            .filter(|column| column.get::<String, _>("name") == "device_id")
            .count(),
        1
    );

    let device_id: Option<String> =
        sqlx::query_scalar("SELECT device_id FROM conversations WHERE conversation_id = ?")
            .bind("legacy")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(device_id, None);
}

#[tokio::test]
async fn device_conversation_ownership_cannot_be_claimed_or_crossed() {
    let pool = test_state().await.pool;
    let browser = db::ConversationOwner::Browser;
    let doll_a = db::ConversationOwner::Device("DOLL-A".to_string());
    let doll_b = db::ConversationOwner::Device("DOLL-B".to_string());

    db::ensure_conversation_owned(&pool, "legacy", &browser)
        .await
        .unwrap();
    assert!(db::ensure_conversation_owned(&pool, "legacy", &doll_a)
        .await
        .is_err());

    db::ensure_conversation_owned(&pool, "owned-a", &doll_a)
        .await
        .unwrap();
    db::ensure_conversation_owned(&pool, "owned-a", &doll_a)
        .await
        .unwrap();
    assert!(db::ensure_conversation_owned(&pool, "owned-a", &doll_b)
        .await
        .is_err());
    assert!(db::ensure_conversation_owned(&pool, "owned-a", &browser)
        .await
        .is_err());
}

async fn spawn_test_server(state: AppState) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            production_router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (address, task)
}

#[tokio::test]
async fn admin_config_masks_secrets_and_exposes_default_model() {
    let app = router(test_state().await);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/admin/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();

    assert_eq!(body["app_id"], "048c5dc4");
    assert_eq!(body["llm_model"], "xopdeepseekv4flash");
    assert_eq!(body["order_context"]["storeId"], "57");
    assert_eq!(body["order_context"]["deptId"], 57);
    assert_eq!(
        body["order_context"]["xUserId"],
        "3a224c9c-5652-92e1-8610-920b228febb3"
    );
    assert!(body["available_models"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m == "xopdsv32exp"));
    assert!(body.get("api_secret").is_none());
    assert!(body.get("mjy_open_api").is_none());
}

#[tokio::test]
async fn text_chat_returns_voice_and_analysis_events() {
    let app = router(test_state().await);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(json!({"text":"买两瓶可乐和一瓶水"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let events = body["events"].as_array().unwrap();
    let event_types = events
        .iter()
        .map(|e| e["event_type"].as_str().unwrap())
        .collect::<Vec<_>>();

    assert!(event_types.contains(&"asr_final"));
    assert!(event_types.contains(&"voice_done"));
    assert!(event_types.contains(&"intent_analysis"));
    assert!(event_types.contains(&"order_draft"));
}

#[tokio::test]
async fn conversation_round_accumulates_purchase_items() {
    let app = router(test_state().await);

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/conversations/new")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), usize::MAX).await.unwrap()).unwrap();
    let conversation_id = first_body["conversation_id"].as_str().unwrap().to_string();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"先要两瓶可乐"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"再加一瓶水"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let draft = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_type"] == "order_draft")
        .unwrap();
    let items = draft["payload"]["items"].as_array().unwrap();

    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|item| item["product_id"] == "cola-500"));
    assert!(items.iter().any(|item| item["product_id"] == "water-555"));
}

#[tokio::test]
async fn natural_affirmation_turn_submits_order_with_local_mock_fallback() {
    let app = router(test_state().await);

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/conversations/new")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), usize::MAX).await.unwrap()).unwrap();
    let conversation_id = first_body["conversation_id"].as_str().unwrap().to_string();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"买两瓶可乐和一瓶水"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"对的。"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let order_created = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_type"] == "order_created")
        .expect("confirmation should create an order");

    assert_eq!(order_created["payload"]["ok"], true);
    assert_eq!(order_created["payload"]["mock"], true);
    assert_eq!(
        order_created["payload"]["items"].as_array().unwrap().len(),
        2
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/admin/conversations/{conversation_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let orders = detail["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0]["conversation_id"], conversation_id);
    assert_eq!(orders[0]["payload"]["items"].as_array().unwrap().len(), 2);
    let event_types = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event_type"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(event_types.contains(&"product_matches"));
    assert!(event_types.contains(&"order_draft"));
    assert!(event_types.contains(&"order_submit_started"));
    assert!(event_types.contains(&"order_create_call"));
    assert!(event_types.contains(&"order_persisted"));
    assert!(event_types.contains(&"order_created"));
}

#[tokio::test]
async fn one_conversation_can_submit_a_second_independent_order() {
    let app = router(test_state().await);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/conversations/new")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let conversation_id = body["conversation_id"].as_str().unwrap().to_string();

    for text in ["我要一瓶可口可乐", "确认下单"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/chat/text")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"conversation_id": conversation_id, "text": text}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"我还要一杯纯牛奶"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let second_draft = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_type"] == "order_draft")
        .expect("a new purchase after the first order should create a second draft");
    let second_items = second_draft["payload"]["items"].as_array().unwrap();
    assert_eq!(second_items.len(), 1);
    assert_eq!(second_items[0]["product_id"], "milk-250");

    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"嗯嗯，不用了，直接下单吧"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let events = body["events"].as_array().unwrap();
    assert!(events
        .iter()
        .any(|event| event["event_type"] == "order_created"));
    assert!(!events
        .iter()
        .any(|event| event["event_type"] == "conversation_ended"));
    assert_eq!(
        events
            .iter()
            .find(|event| event["event_type"] == "intent_analysis")
            .unwrap()["payload"]["intent"],
        "confirm_order"
    );
    let assistant_reply = events
        .iter()
        .filter(|event| event["event_type"] == "llm_delta")
        .filter_map(|event| event["payload"]["content"].as_str())
        .collect::<String>();
    assert!(assistant_reply.contains("下发订单"));
    assert!(!assistant_reply.contains("退下"));

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/admin/conversations/{conversation_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let detail: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let orders = detail["orders"].as_array().unwrap();
    assert_eq!(orders.len(), 2);
    assert!(orders.iter().any(|order| {
        order["payload"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["product_id"] == "cola-500")
    }));
    assert!(orders.iter().any(|order| {
        order["payload"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["product_id"] == "milk-250")
    }));
}

#[tokio::test]
async fn order_tool_mapping_is_configurable_for_order_events() {
    let state = test_state().await;
    let mut config = db::get_config(&state.pool).await.unwrap();
    config.order_mcp_tools = json!({
        "create_order": "customerCreateSaleOrder",
        "list_orders": "customerListOrders",
        "get_order_detail": "customerOrderDetail",
        "refund_order": "customerRefundOrder",
        "resolve_context": "customerResolveContext"
    });
    db::save_config(&state.pool, &config).await.unwrap();
    let app = router(state);

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/conversations/new")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), usize::MAX).await.unwrap()).unwrap();
    let conversation_id = first_body["conversation_id"].as_str().unwrap().to_string();

    for text in ["买一瓶可乐", "确认下单"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/chat/text")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"conversation_id": conversation_id, "text": text}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/admin/conversations/{conversation_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let create_call = detail["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_type"] == "order_create_call")
        .expect("order create call should be logged");
    assert_eq!(create_call["payload"]["tool"], "customerCreateSaleOrder");
}

#[tokio::test]
async fn dismiss_after_created_order_only_ends_conversation() {
    let app = router(test_state().await);

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/conversations/new")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), usize::MAX).await.unwrap()).unwrap();
    let conversation_id = first_body["conversation_id"].as_str().unwrap().to_string();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"买一瓶可乐"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"确认下单"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let order_id = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event_type"] == "order_created")
        .and_then(|event| {
            event["payload"]["saleOrderId"]
                .as_str()
                .or_else(|| event["payload"]["order_id"].as_str())
        })
        .expect("confirmation should create an order")
        .to_string();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"退单"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let events = body["events"].as_array().unwrap();
    let event_types = events
        .iter()
        .map(|event| event["event_type"].as_str().unwrap())
        .collect::<Vec<_>>();
    let intent = events
        .iter()
        .find(|event| event["event_type"] == "intent_analysis")
        .expect("bare refund word should still be analyzed");
    assert_eq!(intent["payload"]["intent"], "refund_order");
    assert_eq!(
        events
            .iter()
            .find(|event| event["event_type"] == "order_refund_started")
            .and_then(|event| event["payload"]["saleOrderId"].as_str()),
        Some(order_id.as_str())
    );
    assert!(event_types.contains(&"order_refund_started"));
    assert!(event_types.contains(&"order_refunded"));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"你可以退下了"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let events = body["events"].as_array().unwrap();
    let event_types = events
        .iter()
        .map(|event| event["event_type"].as_str().unwrap())
        .collect::<Vec<_>>();
    let intent = events
        .iter()
        .find(|event| event["event_type"] == "intent_analysis")
        .expect("dismiss after order should be analyzed");

    assert_eq!(intent["payload"]["intent"], "end_conversation");
    let assistant_text = events
        .iter()
        .filter(|event| event["event_type"] == "llm_delta")
        .filter_map(|event| event["payload"]["content"].as_str())
        .collect::<String>();
    assert_eq!(assistant_text, "好的主人，我退下了。");
    assert!(!assistant_text.contains("退单"));
    assert!(event_types.contains(&"conversation_ended"));
    assert!(!event_types.contains(&"order_refund_started"));
    assert!(!event_types.contains(&"order_refunded"));
    assert!(!event_types.contains(&"order_submit_started"));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orders/detail")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"saleOrderId": order_id, "conversation_id": conversation_id})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(detail["status"], "refunded");
    assert_eq!(detail["saleOrderId"], order_id);
}

#[tokio::test]
async fn explicit_cancel_after_created_order_refunds_and_ends_conversation() {
    let app = router(test_state().await);

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/conversations/new")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_body: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), usize::MAX).await.unwrap()).unwrap();
    let conversation_id = first_body["conversation_id"].as_str().unwrap().to_string();

    for text in ["买一瓶可乐", "确认下单"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/chat/text")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"conversation_id": conversation_id, "text": text}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"帮我取消订单"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let events = body["events"].as_array().unwrap();
    let event_types = events
        .iter()
        .map(|event| event["event_type"].as_str().unwrap())
        .collect::<Vec<_>>();
    let intent = events
        .iter()
        .find(|event| event["event_type"] == "intent_analysis")
        .expect("cancel after order should be analyzed");

    assert_eq!(intent["payload"]["intent"], "refund_order");
    assert!(event_types.contains(&"order_refund_started"));
    assert!(event_types.contains(&"order_refunded"));
    assert!(event_types.contains(&"conversation_ended"));
}

#[tokio::test]
async fn end_conversation_intent_stops_round_without_order_submission() {
    let app = router(test_state().await);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(json!({"text":"我要一瓶可乐"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let first_body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let conversation_id = first_body["conversation_id"].as_str().unwrap().to_string();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id": conversation_id, "text":"退一下吧。退一下吧"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let events = body["events"].as_array().unwrap();
    let event_types = events
        .iter()
        .map(|event| event["event_type"].as_str().unwrap())
        .collect::<Vec<_>>();

    let intent = events
        .iter()
        .find(|event| event["event_type"] == "intent_analysis")
        .expect("end conversation should be analyzed");
    assert_eq!(intent["payload"]["intent"], "end_conversation");
    assert!(event_types.contains(&"conversation_ended"));
    assert!(!event_types.contains(&"order_submit_started"));
    assert!(!event_types.contains(&"order_created"));
}

#[tokio::test]
async fn order_apis_create_query_detail_and_refund_with_mock_fallback() {
    let app = router(test_state().await);
    let item = json!({
        "product_id": "cola-500",
        "name": "可口可乐",
        "spec": "500ml",
        "quantity": 2,
        "unit_price": 3.5,
        "confidence": 0.86
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/order/confirm")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "conversation_id": "test-order-round",
                        "items": [item]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(created["ok"], true);
    assert_eq!(created["mock"], true);
    let order_id = created["saleOrderId"].as_str().unwrap().to_string();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orders/list")
                .header("content-type", "application/json")
                .body(Body::from(json!({}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let listed: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(listed["orders"]
        .as_array()
        .unwrap()
        .iter()
        .any(|order| { order["saleOrderId"] == order_id || order["order_id"] == order_id }));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orders/detail")
                .header("content-type", "application/json")
                .body(Body::from(json!({"saleOrderId": order_id}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(detail["saleOrderId"], order_id);
    assert_eq!(detail["status"], "created");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orders/refund")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"saleOrderId": order_id, "reason": "测试退单"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let refunded: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(refunded["ok"], true);
    assert_eq!(refunded["status"], "refunded");

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/orders/detail")
                .header("content-type", "application/json")
                .body(Body::from(json!({"saleOrderId": order_id}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(detail["status"], "refunded");
}

#[tokio::test]
async fn miniprogram_c_debug_exposes_and_mocks_confirmed_apifox_interfaces() {
    let app = router(test_state().await);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/debug/miniprogram-c/interfaces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let interfaces = body["interfaces"].as_array().unwrap();
    assert_eq!(interfaces.len(), 6);
    assert!(interfaces
        .iter()
        .any(|item| item["id"] == "get-user-sale-orders"));
    assert!(interfaces
        .iter()
        .any(|item| item["id"] == "get-user-sale-order-detail"));
    assert!(interfaces.iter().any(|item| item["id"] == "create-order"));
    assert!(interfaces
        .iter()
        .any(|item| item["id"] == "cancel-sale-order"));
    assert!(interfaces.iter().any(|item| item["id"] == "pay-order"));
    assert!(interfaces.iter().any(|item| item["id"] == "apply-refund"));
    assert!(body["missing_interfaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["name"] == "创建订单"));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mock/app-catering/api/app/saleorder/get-user-sale-orders?pageIndex=1&pageSize=2&status=102")
                .header("__app", "mjy-miniapp")
                .header("__appver", "1.0.0")
                .header("__company", "CC")
                .header("__store", "999006940")
                .header("__storeno", "6634")
                .header("__src_channel", "2")
                .header("CompanyCode", "CC")
                .header("Authorization", "Bearer mock-token")
                .header("debug", "true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let list: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(list["code"], 0);
    assert_eq!(list["data"]["pageIndex"], 1);
    assert_eq!(list["data"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        list["_debug"]["missingHeaders"].as_array().unwrap().len(),
        0
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mock/app-catering/api/app/saleorder/get-user-sale-order-detail?saleOrderId=mock-sale-order-002&srcChannel=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let detail: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(detail["code"], 0);
    assert_eq!(detail["data"]["saleOrderId"], "mock-sale-order-002");
    assert_eq!(detail["data"]["displayStatus"], "待取餐");
    assert!(detail["_debug"]["missingHeaders"].as_array().unwrap().len() > 0);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/debug/miniprogram-c/call")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "interface_id": "get-user-sale-order-detail",
                        "query": {"saleOrderId": "mock-sale-order-001", "srcChannel": "2"},
                        "headers": {"Authorization": "Bearer mock-token"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let debug: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(debug["ok"], true);
    assert_eq!(
        debug["response"]["data"]["saleOrderId"],
        "mock-sale-order-001"
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mock/app-catering/api/app/saleorder/create-order?srcChannel=2")
                .header("content-type", "application/json")
                .header("__app", "mjy-miniapp")
                .header("__appver", "1.0.0")
                .header("__company", "CC")
                .header("__store", "999006940")
                .header("__storeno", "6634")
                .header("__src_channel", "2")
                .header("CompanyCode", "CC")
                .header("Authorization", "Bearer mock-token")
                .header("debug", "true")
                .body(Body::from(
                    json!({
                        "storeId": "999006940",
                        "storeNo": "6634",
                        "goodses": [
                            {"goodsId": "cola-500", "goodsName": "可口可乐", "qty": 2, "salePrice": 3.5}
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(created["code"], 0);
    assert_eq!(created["data"]["displayStatus"], "待支付");
    assert_eq!(created["_debug"]["mockOnly"], true);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/debug/miniprogram-c/call")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "interface_id": "apply-refund",
                        "query": {"srcChannel": "2"},
                        "body": {"saleOrderId": "mock-sale-order-003", "refundAmt": 16.8},
                        "headers": {"Authorization": "Bearer mock-token"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let refund: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(refund["ok"], true);
    assert_eq!(refund["response"]["data"]["refundStatus"], 1);
}

#[tokio::test]
async fn admin_can_read_conversation_history() {
    let state = test_state().await;
    let pool = state.pool.clone();
    let app = router(state);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(json!({"text":"买两瓶可乐"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let conversation_id = body["conversation_id"].as_str().unwrap();
    db::ensure_conversation_owned(
        &pool,
        "device-source-conversation",
        &db::ConversationOwner::Device("DOLL-SOURCE".to_string()),
    )
    .await
    .unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/admin/conversations?page=1&page_size=50")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["page"], 1);
    assert_eq!(body["page_size"], 50);
    assert!(body["total"].as_i64().unwrap() >= 1);
    let rows = body["items"].as_array().unwrap();
    assert!(rows.iter().any(|row| {
        row["conversation_id"] == conversation_id
            && row["message_count"].as_i64().unwrap() >= 1
            && row.as_object().unwrap().contains_key("device_id")
            && row["device_id"].is_null()
    }));
    assert!(rows.iter().any(|row| {
        row["conversation_id"] == "device-source-conversation" && row["device_id"] == "DOLL-SOURCE"
    }));

    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/admin/conversations/{conversation_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["conversation_id"], conversation_id);
    assert!(body["messages"].as_array().unwrap().iter().any(|message| {
        message["role"] == "user" && message["content"].as_str().unwrap().contains("可乐")
    }));
}

#[tokio::test]
async fn public_experience_can_read_only_masked_runtime_config() {
    let app = production_router(test_state().await).layer(Extension(ConnectInfo(
        SocketAddr::from(([127, 0, 0, 1], 0)),
    )));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/public/config")
                .header("x-real-ip", "203.0.113.42")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(body["available_models"].is_array());
    for secret in ["api_key", "api_secret", "order_mcp_token"] {
        assert!(body.get(secret).is_none() || body[secret] == "");
    }
}

#[tokio::test]
async fn admin_conversations_are_paginated() {
    let app = router(test_state().await);
    for index in 0..7 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/chat/text")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"text": format!("第 {index} 轮买一瓶水")}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/admin/conversations?page=2&page_size=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();

    assert_eq!(body["page"], 2);
    assert_eq!(body["page_size"], 5);
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["total"], 7);
    assert_eq!(body["total_pages"], 2);
}

#[tokio::test]
async fn device_auth_issues_token_for_seeded_demo_device() {
    let app = router(test_state().await);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/device/auth")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"device_id":"DOLL-0001","device_secret":"demo-secret"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();

    assert!(body["token"].as_str().unwrap().contains('.'));
}

#[tokio::test]
async fn device_config_describes_voice_stream_protocol_for_sdks() {
    let app = router(test_state().await);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/device/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();

    assert!(body.get("audio").is_none());
    assert!(body.get("tts").is_none());
    assert_eq!(body["auth"]["type"], "device_token");
    assert_eq!(body["auth"]["auth_url"], "/api/device/auth");
    assert_eq!(
        body["auth"]["request"],
        json!({
            "device_id": "<configured-device-id>",
            "device_secret": "<provisioned-device-secret>"
        })
    );
    let raw = serde_json::to_string(&body).unwrap();
    assert!(!raw.contains("DOLL-0001"));
    assert!(!raw.contains("demo-secret"));
    assert_eq!(body["voice_ws"]["path"], "/api/device/voice");
    assert_eq!(body["voice_ws"]["query"][0], "device_id");
    assert_eq!(body["voice_ws"]["query"][1], "token");
    assert_eq!(body["voice_ws"]["query"][2], "in_format");
    assert_eq!(body["voice_ws"]["query"][3], "in_rate");
    assert_eq!(body["voice_ws"]["query"][4], "out_format");
    assert_eq!(body["voice_ws"]["query"][5], "out_rate");
    assert!(body.get("audio_formats").is_none());
    assert_eq!(
        body["audio_profiles"]["query"],
        json!(["in_format", "in_rate", "out_format", "out_rate"])
    );
    assert_eq!(
        body["audio_profiles"]["input"]["default"],
        json!({"format": "mp3", "sample_rate": 16000})
    );
    assert_eq!(
        body["audio_profiles"]["input"]["supported"],
        json!([
            {"format": "mp3", "sample_rates": [16000]},
            {"format": "pcm", "sample_rates": [16000]}
        ])
    );
    assert_eq!(
        body["audio_profiles"]["output"]["default"],
        json!({"format": "mp3", "sample_rate": 16000})
    );
    assert_eq!(
        body["audio_profiles"]["output"]["supported"],
        json!([
            {"format": "mp3", "sample_rates": [8000, 16000, 24000]},
            {"format": "pcm", "sample_rates": [16000]}
        ])
    );
    assert_eq!(body["audio_profiles"]["pcm"]["bit_depth"], 16);
    assert_eq!(body["audio_profiles"]["pcm"]["channels"], 1);
    assert_eq!(body["audio_profiles"]["pcm"]["endianness"], "little");
    assert_eq!(
        body["audio_profiles"]["packetized"]["opus"]["frame_duration_ms"],
        20
    );
    assert_eq!(
        body["audio_profiles"]["packetized"]["speex"]["one_packet_per_chunk"],
        true
    );
    for direction in ["input", "output"] {
        let default = &body["audio_profiles"][direction]["default"];
        assert!(body["audio_profiles"][direction]["supported"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry["format"] == default["format"]
                    && entry["sample_rates"]
                        .as_array()
                        .unwrap()
                        .contains(&default["sample_rate"])
            }));
    }
    assert!(body["voice_ws"]["client_events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event == "audio_stream_chunk"));
    assert!(body["voice_ws"]["client_events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event == "tts_interrupt"));
    assert!(body["voice_ws"]["server_events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event == "tts_audio_chunk"));
    assert!(body["voice_ws"]["server_events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event == "tts_interrupted"));
    assert!(body["voice_ws"]["server_events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event == "conversation_ended"));
}

fn websocket_upgrade_request(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn chat_voice_rejects_invalid_audio_query_before_upgrade() {
    let response = router(test_state().await)
        .oneshot(websocket_upgrade_request(
            "/api/chat/voice?in_format=wav&in_rate=16000",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"], "unsupported_audio_format");
}

#[tokio::test]
async fn device_voice_rejects_invalid_audio_query_before_upgrade() {
    let token = issue_device_token(
        "DOLL-0001",
        "test-server-secret",
        chrono::Utc::now().timestamp() + 3600,
    )
    .unwrap();
    let response = router(test_state().await)
        .oneshot(websocket_upgrade_request(&format!(
            "/api/device/voice?device_id=DOLL-0001&token={token}&out_rate=44100"
        )))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"], "unsupported_audio_rate");
}

#[tokio::test]
async fn device_voice_auth_precedes_audio_profile_validation() {
    let response = router(test_state().await)
        .oneshot(websocket_upgrade_request(
            "/api/device/voice?device_id=DOLL-0001&in_format=wav",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn device_config_rejects_unknown_audio_provider_configuration() {
    let state = test_state().await;
    let mut config = db::get_config(&state.pool).await.unwrap();
    config.iat_provider = "unknown-provider".to_string();
    db::save_config(&state.pool, &config).await.unwrap();

    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/device/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(response.status().is_server_error());
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["error"], "config_error");
}

#[tokio::test]
async fn chat_and_authenticated_device_voice_upgrade_over_real_tcp() {
    let chat_state = test_state().await;
    let (chat_address, chat_server) = spawn_test_server(chat_state).await;
    let (mut chat, chat_response) = tokio_tungstenite::connect_async(format!(
        "ws://{chat_address}/api/chat/voice?in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    ))
    .await
    .unwrap();
    assert_eq!(chat_response.status().as_u16(), 101);
    chat.close(None).await.unwrap();
    chat_server.abort();

    let device_state = test_state().await;
    let token = issue_device_token(
        "DOLL-0001",
        "test-server-secret",
        chrono::Utc::now().timestamp() + 3600,
    )
    .unwrap();
    let (device_address, device_server) = spawn_test_server(device_state).await;
    let (mut device, device_response) = tokio_tungstenite::connect_async(format!(
        "ws://{device_address}/api/device/voice?device_id=DOLL-0001&token={token}&in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    ))
    .await
    .unwrap();
    assert_eq!(device_response.status().as_u16(), 101);
    device.close(None).await.unwrap();
    device_server.abort();
}

#[tokio::test]
async fn disabled_device_cannot_reconnect_with_preissued_token() {
    use tokio_tungstenite::tungstenite::Error as WsError;

    let state = test_state().await;
    let token = issue_device_token(
        "DOLL-0001",
        "test-server-secret",
        chrono::Utc::now().timestamp() + 3600,
    )
    .unwrap();
    sqlx::query("UPDATE devices SET enabled = 0 WHERE device_id = ?")
        .bind("DOLL-0001")
        .execute(&state.pool)
        .await
        .unwrap();
    let (address, server) = spawn_test_server(state).await;

    let error = tokio_tungstenite::connect_async(format!(
        "ws://{address}/api/device/voice?device_id=DOLL-0001&token={token}&in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    ))
    .await
    .unwrap_err();
    let WsError::Http(response) = error else {
        panic!("expected HTTP rejection, got {error}");
    };
    assert_eq!(response.status().as_u16(), 401);
    server.abort();
}

#[tokio::test]
async fn browser_and_device_voice_create_distinct_conversation_owners() {
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let state = test_state().await;
    let pool = state.pool.clone();
    let browser = production_router(state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(
        ([127, 0, 0, 1], 0),
    ))));
    let response = browser
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/chat/text")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"conversation_id":"browser-one","text":"买一瓶水"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let token = issue_device_token(
        "DOLL-0001",
        "test-server-secret",
        chrono::Utc::now().timestamp() + 3600,
    )
    .unwrap();
    let (address, server) = spawn_test_server(state).await;
    let (mut device, _) = tokio_tungstenite::connect_async(format!(
        "ws://{address}/api/device/voice?device_id=DOLL-0001&token={token}&in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    ))
    .await
    .unwrap();
    device
        .send(WsMessage::Text(
            json!({"type":"text","conversation_id":"device-one","text":"买一瓶水"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    while let Some(message) = device.next().await {
        let text = message.unwrap().into_text().unwrap();
        let event: Value = serde_json::from_str(&text).unwrap();
        if event["event_type"] == "voice_done" {
            break;
        }
    }
    device.close(None).await.unwrap();
    server.abort();

    let browser_owner: Option<String> =
        sqlx::query_scalar("SELECT device_id FROM conversations WHERE conversation_id = ?")
            .bind("browser-one")
            .fetch_one(&pool)
            .await
            .unwrap();
    let device_owner: Option<String> =
        sqlx::query_scalar("SELECT device_id FROM conversations WHERE conversation_id = ?")
            .bind("device-one")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(browser_owner, None);
    assert_eq!(device_owner.as_deref(), Some("DOLL-0001"));
}

#[tokio::test]
async fn device_voice_cannot_reuse_another_owner_conversation_id() {
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let state = test_state().await;
    let pool = state.pool.clone();
    db::ensure_conversation_owned(
        &pool,
        "legacy-device-reuse",
        &db::ConversationOwner::Browser,
    )
    .await
    .unwrap();
    db::ensure_conversation_owned(
        &pool,
        "other-device-reuse",
        &db::ConversationOwner::Device("DOLL-OTHER".to_string()),
    )
    .await
    .unwrap();
    let token = issue_device_token(
        "DOLL-0001",
        "test-server-secret",
        chrono::Utc::now().timestamp() + 3600,
    )
    .unwrap();
    let (address, server) = spawn_test_server(state).await;
    let (mut device, _) = tokio_tungstenite::connect_async(format!(
        "ws://{address}/api/device/voice?device_id=DOLL-0001&token={token}&in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    ))
    .await
    .unwrap();

    for conversation_id in ["legacy-device-reuse", "other-device-reuse"] {
        device
            .send(WsMessage::Text(
                json!({"type":"text","conversation_id":conversation_id,"text":"买一瓶水"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let terminal_event = loop {
            let text = device.next().await.unwrap().unwrap().into_text().unwrap();
            let event: Value = serde_json::from_str(&text).unwrap();
            if event["event_type"] == "error" && event["payload"]["code"] == "turn_failed" {
                break "turn_failed".to_string();
            }
            if event["event_type"] == "voice_done" {
                break "voice_done".to_string();
            }
        };
        assert_eq!(terminal_event, "turn_failed");
    }
    device.close(None).await.unwrap();
    server.abort();

    for conversation_id in ["legacy-device-reuse", "other-device-reuse"] {
        let message_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_messages WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            message_count, 0,
            "{conversation_id} accepted a device message"
        );
    }
}

#[tokio::test]
async fn public_proxy_rejects_seeded_demo_device_auth_and_preissued_voice_token() {
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error as WsError};

    let state = test_state().await;
    let token = issue_device_token(
        "DOLL-0001",
        "test-server-secret",
        chrono::Utc::now().timestamp() + 3600,
    )
    .unwrap();
    let (address, server) = spawn_test_server(state).await;

    let auth = reqwest::Client::new()
        .post(format!("http://{address}/api/device/auth"))
        .header("x-real-ip", "203.0.113.42")
        .header("x-forwarded-for", "127.0.0.1")
        .json(&json!({"device_id":"DOLL-0001","device_secret":"demo-secret"}))
        .send()
        .await
        .unwrap();
    assert_eq!(auth.status(), reqwest::StatusCode::UNAUTHORIZED);

    let mut request = format!(
        "ws://{address}/api/device/voice?device_id=DOLL-0001&token={token}&in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    )
    .into_client_request()
    .unwrap();
    request
        .headers_mut()
        .insert("x-real-ip", "203.0.113.42".parse().unwrap());
    request
        .headers_mut()
        .insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
    let error = tokio_tungstenite::connect_async(request).await.unwrap_err();
    let WsError::Http(response) = error else {
        panic!("expected HTTP rejection, got {error}");
    };
    assert_eq!(response.status().as_u16(), 401);

    server.abort();
}

#[tokio::test]
async fn configured_non_demo_device_can_auth_and_upgrade_through_public_proxy() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let state = test_state().await;
    sqlx::query("INSERT INTO devices(device_id, secret_hash, name, enabled) VALUES(?, ?, ?, 1)")
        .bind("DEVICE-PROD-0001")
        .bind(secret_hash("independent-test-secret"))
        .bind("已配置生产设备")
        .execute(&state.pool)
        .await
        .unwrap();
    let (address, server) = spawn_test_server(state).await;

    let auth = reqwest::Client::new()
        .post(format!("http://{address}/api/device/auth"))
        .header("x-real-ip", "203.0.113.42")
        .header("x-forwarded-for", "127.0.0.1")
        .json(&json!({
            "device_id":"DEVICE-PROD-0001",
            "device_secret":"independent-test-secret"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(auth.status(), reqwest::StatusCode::OK);
    let body: Value = auth.json().await.unwrap();
    let token = body["token"].as_str().unwrap();

    let mut request = format!(
        "ws://{address}/api/device/voice?device_id=DEVICE-PROD-0001&token={token}&in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    )
    .into_client_request()
    .unwrap();
    request
        .headers_mut()
        .insert("x-real-ip", "203.0.113.42".parse().unwrap());
    request
        .headers_mut()
        .insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.status().as_u16(), 101);
    socket.close(None).await.unwrap();

    server.abort();
}

#[tokio::test]
async fn public_admin_login_and_device_authorization_management() {
    let state = test_state().await;
    let pool = state.pool.clone();
    sqlx::query(
        "INSERT INTO conversations(conversation_id, created_at, device_id) VALUES(?, ?, ?)",
    )
    .bind("managed-device-conversation")
    .bind("2026-07-28T08:30:00Z")
    .bind("DOLL-0001")
    .execute(&pool)
    .await
    .unwrap();
    let (address, server) = spawn_test_server(state).await;
    let base_url = format!("http://{address}");
    let origin = base_url.clone();
    let client = reqwest::Client::new();

    let unauthenticated = client
        .get(format!("{base_url}/api/admin/device-authorizations"))
        .header("x-real-ip", "203.0.113.42")
        .header("x-forwarded-for", "127.0.0.1")
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauthenticated.json::<Value>().await.unwrap()["error"],
        "login_required"
    );

    for credentials in [
        json!({"username":"nobody","password":"test-admin-password"}),
        json!({"username":"admin","password":"test-admin-password"}),
    ] {
        let invalid = client
            .post(format!("{base_url}/api/admin/auth/login"))
            .header("x-real-ip", "203.0.113.42")
            .header("origin", &origin)
            .json(&credentials)
            .send()
            .await
            .unwrap();
        assert_eq!(invalid.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            invalid.json::<Value>().await.unwrap()["error"],
            "invalid_credentials"
        );
    }

    let login = client
        .post(format!("{base_url}/api/admin/auth/login"))
        .header("x-real-ip", "203.0.113.42")
        .header("origin", &origin)
        .json(&json!({
            "username":"myjadmin",
            "password":"test-admin-password"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), reqwest::StatusCode::OK);
    let set_cookie = login
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(set_cookie.starts_with("mjy_admin_session="));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
    assert!(set_cookie.contains("Path=/"));
    assert!(!set_cookie.contains("Max-Age"));
    assert!(!set_cookie.contains("Expires"));
    assert!(!set_cookie.contains("Secure"));
    let cookie = set_cookie.split(';').next().unwrap().to_string();

    let me = client
        .get(format!("{base_url}/api/admin/auth/me"))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), reqwest::StatusCode::OK);
    assert_eq!(me.json::<Value>().await.unwrap()["username"], "myjadmin");

    let listed = client
        .get(format!("{base_url}/api/admin/device-authorizations"))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(listed.status(), reqwest::StatusCode::OK);
    let listed: Value = listed.json().await.unwrap();
    let demo = listed.as_array().unwrap().first().unwrap();
    assert_eq!(demo["device_id"], "DOLL-0001");
    assert_eq!(demo["last_conversation_at"], "2026-07-28T08:30:00Z");
    assert!(demo.get("secret_hash").is_none());
    assert!(demo.get("device_secret").is_none());

    let missing_origin = client
        .post(format!("{base_url}/api/admin/device-authorizations"))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .json(&json!({"device_id":"DEVICE-MISSING-ORIGIN","name":"bad"}))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_origin.status(), reqwest::StatusCode::FORBIDDEN);

    let created = client
        .post(format!("{base_url}/api/admin/device-authorizations"))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .header("origin", &origin)
        .json(&json!({"device_id":"DEVICE-ADMIN-0001","name":"门店玩偶"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
    let created: Value = created.json().await.unwrap();
    let first_secret = created["device_secret"].as_str().unwrap();
    assert_eq!(first_secret.chars().count(), 24);
    assert!(first_secret.chars().all(|character| {
        "ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789".contains(character)
    }));
    assert_ne!(
        sqlx::query_scalar::<_, String>("SELECT secret_hash FROM devices WHERE device_id = ?")
            .bind("DEVICE-ADMIN-0001")
            .fetch_one(&pool)
            .await
            .unwrap(),
        first_secret
    );

    let duplicate = client
        .post(format!("{base_url}/api/admin/device-authorizations"))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .header("origin", &origin)
        .json(&json!({"device_id":"DEVICE-ADMIN-0001","name":"重复"}))
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        duplicate.json::<Value>().await.unwrap()["error"],
        "device_already_exists"
    );

    let updated = client
        .put(format!(
            "{base_url}/api/admin/device-authorizations/DEVICE-ADMIN-0001"
        ))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .header("origin", &origin)
        .json(&json!({"name":"收银台玩偶","enabled":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(updated.status(), reqwest::StatusCode::OK);
    let updated: Value = updated.json().await.unwrap();
    assert_eq!(updated["name"], "收银台玩偶");
    assert_eq!(updated["enabled"], false);
    assert!(updated.get("device_secret").is_none());

    let unconfirmed = client
        .post(format!(
            "{base_url}/api/admin/device-authorizations/DEVICE-ADMIN-0001/reset-secret"
        ))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .header("origin", &origin)
        .json(&json!({"confirm":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(unconfirmed.status(), reqwest::StatusCode::BAD_REQUEST);

    let reset = client
        .post(format!(
            "{base_url}/api/admin/device-authorizations/DEVICE-ADMIN-0001/reset-secret"
        ))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .header("origin", &origin)
        .json(&json!({"confirm":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(reset.status(), reqwest::StatusCode::OK);
    let reset: Value = reset.json().await.unwrap();
    let reset_secret = reset["device_secret"].as_str().unwrap();
    assert_eq!(reset_secret.chars().count(), 24);
    assert_ne!(reset_secret, first_secret);

    let logout = client
        .post(format!("{base_url}/api/admin/auth/logout"))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .header("origin", &origin)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), reqwest::StatusCode::OK);
    let clear_cookie = logout
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(clear_cookie.starts_with("mjy_admin_session="));

    let revoked = client
        .get(format!("{base_url}/api/admin/auth/me"))
        .header("x-real-ip", "203.0.113.42")
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), reqwest::StatusCode::UNAUTHORIZED);

    server.abort();
}

#[tokio::test]
async fn public_admin_login_rate_limits_after_five_failures() {
    let (address, server) = spawn_test_server(test_state().await).await;
    let base_url = format!("http://{address}");
    let client = reqwest::Client::new();

    for attempt in 1..=6 {
        let response = client
            .post(format!("{base_url}/api/admin/auth/login"))
            .header("x-real-ip", "198.51.100.77")
            .header("origin", &base_url)
            .json(&json!({"username":"myjadmin","password":"wrong"}))
            .send()
            .await
            .unwrap();
        let expected = if attempt <= 5 {
            reqwest::StatusCode::UNAUTHORIZED
        } else {
            reqwest::StatusCode::TOO_MANY_REQUESTS
        };
        assert_eq!(response.status(), expected, "attempt {attempt}");
        let expected_error = if attempt <= 5 {
            "invalid_credentials"
        } else {
            "login_rate_limited"
        };
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"],
            expected_error
        );
    }

    server.abort();
}

#[tokio::test]
async fn admin_login_and_logout_do_not_modify_existing_devices() {
    let state = test_state().await;
    let pool = state.pool.clone();
    let before: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT device_id, secret_hash, name, enabled FROM devices ORDER BY device_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let (address, server) = spawn_test_server(state).await;
    let base_url = format!("http://{address}");
    let client = reqwest::Client::new();

    let login = client
        .post(format!("{base_url}/api/admin/auth/login"))
        .header("x-real-ip", "192.0.2.45")
        .header("origin", &base_url)
        .json(&json!({
            "username":"myjadmin",
            "password":"test-admin-password"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), reqwest::StatusCode::OK);
    let cookie = login
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let logout = client
        .post(format!("{base_url}/api/admin/auth/logout"))
        .header("x-real-ip", "192.0.2.45")
        .header("origin", &base_url)
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(logout.status(), reqwest::StatusCode::OK);

    let after: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT device_id, secret_hash, name, enabled FROM devices ORDER BY device_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(after, before);

    server.abort();
}

#[tokio::test]
async fn voice_websocket_rejects_messages_above_the_bounded_limit() {
    let (address, server) = spawn_test_server(test_state().await).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{address}/api/chat/voice?in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    ))
    .await
    .unwrap();
    let oversized = json!({"type": "text", "text": "x".repeat(129 * 1024)}).to_string();

    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            oversized.into(),
        ))
        .await
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .expect("server must close an oversized websocket frame");
    assert!(!matches!(
        result,
        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
    ));
    server.abort();
}

#[tokio::test]
async fn voice_websocket_accepts_json_with_a_64k_decoded_audio_packet() {
    let (address, server) = spawn_test_server(test_state().await).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{address}/api/chat/voice?in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    ))
    .await
    .unwrap();
    let audio = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        vec![0; 64 * 1024],
    );
    let message = json!({"type": "audio_segment", "audio": audio}).to_string();

    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            message.into(),
        ))
        .await
        .unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let tokio_tungstenite::tungstenite::Message::Text(response) = response else {
        panic!("expected a text event for a legal 64KiB audio packet");
    };
    let event: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(event["event_type"], "asr_partial");
    server.abort();
}

#[tokio::test]
async fn standard_provider_device_config_uses_exact_dynamic_matrix() {
    let state = test_state().await;
    let mut config = db::get_config(&state.pool).await.unwrap();
    config.iat_provider = "standard".to_string();
    config.tts_provider = "standard".to_string();
    db::save_config(&state.pool, &config).await.unwrap();
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/device/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();

    assert_eq!(
        body["audio_profiles"]["input"]["supported"],
        json!([
            {"format": "mp3", "sample_rates": [8000, 16000]},
            {"format": "pcm", "sample_rates": [8000, 16000]},
            {"format": "speex", "sample_rates": [8000, 16000]}
        ])
    );
    assert_eq!(
        body["audio_profiles"]["output"]["supported"],
        json!([
            {"format": "mp3", "sample_rates": [8000, 16000]},
            {"format": "pcm", "sample_rates": [8000, 16000]},
            {"format": "opus", "sample_rates": [8000, 16000]},
            {"format": "speex", "sample_rates": [8000, 16000]}
        ])
    );
    for direction in ["input", "output"] {
        let default = &body["audio_profiles"][direction]["default"];
        let supported = body["audio_profiles"][direction]["supported"]
            .as_array()
            .unwrap();
        assert!(supported.iter().any(|entry| {
            entry["format"] == default["format"]
                && entry["sample_rates"]
                    .as_array()
                    .unwrap()
                    .contains(&default["sample_rate"])
        }));
    }
}

#[tokio::test]
async fn internal_management_routes_reject_public_proxy_identity_without_leaking_data() {
    let (address, server) = spawn_test_server(test_state().await).await;
    let client = reqwest::Client::new();
    let protected_requests = [
        (reqwest::Method::GET, "/api/admin/config", None),
        (
            reqwest::Method::PUT,
            "/api/admin/config",
            Some(serde_json::json!({})),
        ),
        (reqwest::Method::GET, "/api/admin/products", None),
        (
            reqwest::Method::POST,
            "/api/admin/products",
            Some(serde_json::json!({})),
        ),
        (
            reqwest::Method::POST,
            "/api/admin/products/sync",
            Some(serde_json::json!({})),
        ),
        (
            reqwest::Method::PUT,
            "/api/admin/products/cola-500",
            Some(serde_json::json!({})),
        ),
        (reqwest::Method::GET, "/api/admin/conversations", None),
        (
            reqwest::Method::GET,
            "/api/admin/conversations/not-found",
            None,
        ),
        (reqwest::Method::GET, "/api/admin/future-endpoint", None),
        (
            reqwest::Method::GET,
            "/api/debug/miniprogram-c/interfaces",
            None,
        ),
        (
            reqwest::Method::POST,
            "/api/debug/miniprogram-c/call",
            Some(serde_json::json!({})),
        ),
        (reqwest::Method::GET, "/api/debug/future-endpoint", None),
        (
            reqwest::Method::GET,
            "/mock/app-catering/api/app/saleorder/get-user-sale-orders",
            None,
        ),
        (
            reqwest::Method::GET,
            "/mock/app-catering/api/app/saleorder/get-user-sale-order-detail",
            None,
        ),
        (
            reqwest::Method::POST,
            "/mock/app-catering/api/app/saleorder/create-order",
            Some(serde_json::json!({})),
        ),
        (
            reqwest::Method::POST,
            "/mock/app-catering/api/app/saleorder/cancel-sale-order",
            Some(serde_json::json!({})),
        ),
        (
            reqwest::Method::POST,
            "/mock/app-catering/api/app/saleorder/pay-order",
            Some(serde_json::json!({})),
        ),
        (
            reqwest::Method::POST,
            "/mock/app-catering/api/app/saleorder/apply-refund",
            Some(serde_json::json!({})),
        ),
        (reqwest::Method::GET, "/mock/future-endpoint", None),
        (reqwest::Method::GET, "/api/diagnostics/latency", None),
        (
            reqwest::Method::GET,
            "/api/diagnostics/future-endpoint",
            None,
        ),
        (
            reqwest::Method::POST,
            "/api/orders/list",
            Some(serde_json::json!({})),
        ),
        (
            reqwest::Method::POST,
            "/api/orders/detail",
            Some(serde_json::json!({"saleOrderId":"hidden"})),
        ),
        (
            reqwest::Method::POST,
            "/api/orders/refund",
            Some(serde_json::json!({"saleOrderId":"hidden","reason":"hidden"})),
        ),
        (reqwest::Method::GET, "/api/orders/future-endpoint", None),
        (
            reqwest::Method::POST,
            "/api/order/confirm",
            Some(serde_json::json!({})),
        ),
        (reqwest::Method::GET, "/api/order/future-endpoint", None),
    ];

    for (method, path, body) in protected_requests {
        let mut request = client
            .request(method, format!("http://{address}{path}"))
            .header("x-real-ip", "203.0.113.42")
            .header("x-forwarded-for", "127.0.0.1");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path}"
        );
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"error":"login_required"}"#,
            "{path} must not expose handler output"
        );
    }

    server.abort();
}

#[tokio::test]
async fn protected_route_without_tcp_connect_info_is_denied_by_default() {
    let response = production_router(test_state().await)
        .oneshot(
            Request::builder()
                .uri("/api/admin/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        r#"{"error":"login_required"}"#
    );
}

#[tokio::test]
async fn local_tcp_can_list_detail_and_refund_orders_for_admin_workflow() {
    let (address, server) = spawn_test_server(test_state().await).await;
    let client = reqwest::Client::new();
    let created: Value = client
        .post(format!("http://{address}/api/order/confirm"))
        .json(&json!({
            "conversation_id": "internal-admin-order",
            "items": [{
                "product_id": "cola-500",
                "name": "可口可乐",
                "spec": "500ml",
                "quantity": 1,
                "unit_price": 3.5,
                "confidence": 1.0
            }]
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let order_id = created["saleOrderId"].as_str().unwrap();

    let listed = client
        .post(format!("http://{address}/api/orders/list"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(listed.status(), reqwest::StatusCode::OK);

    let detail = client
        .post(format!("http://{address}/api/orders/detail"))
        .json(&json!({"saleOrderId": order_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(detail.status(), reqwest::StatusCode::OK);

    let refunded = client
        .post(format!("http://{address}/api/orders/refund"))
        .json(&json!({"saleOrderId": order_id, "reason": "本机后台退单"}))
        .send()
        .await
        .unwrap();
    assert_eq!(refunded.status(), reqwest::StatusCode::OK);

    server.abort();
}

#[tokio::test]
async fn public_business_routes_ignore_public_proxy_identity() {
    let (address, server) = spawn_test_server(test_state().await).await;
    let client = reqwest::Client::new();
    let requests = [
        (reqwest::Method::GET, "/api/health", None),
        (reqwest::Method::GET, "/api/device/config", None),
        (
            reqwest::Method::POST,
            "/api/device/status",
            Some(json!({"device_id":"DOLL-0001","online":true})),
        ),
        (
            reqwest::Method::POST,
            "/api/conversations/new",
            Some(json!({})),
        ),
        (
            reqwest::Method::POST,
            "/api/chat/text",
            Some(json!({"text":"你好"})),
        ),
    ];

    for (method, path, body) in requests {
        let mut request = client
            .request(method, format!("http://{address}{path}"))
            .header("x-real-ip", "203.0.113.42")
            .header("x-forwarded-for", "127.0.0.1");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{path}");
    }

    server.abort();
}

#[tokio::test]
async fn internal_management_routes_allow_local_tcp_and_loopback_proxy_identity() {
    let (address, server) = spawn_test_server(test_state().await).await;
    let client = reqwest::Client::new();

    for x_real_ip in [None, Some("127.0.0.1"), Some("::1")] {
        let mut request = client.get(format!("http://{address}/api/admin/config"));
        if let Some(x_real_ip) = x_real_ip {
            request = request.header("x-real-ip", x_real_ip);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{x_real_ip:?}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["app_id"], "048c5dc4");
    }

    let me = client
        .get(format!("http://{address}/api/admin/auth/me"))
        .send()
        .await
        .unwrap();
    assert_eq!(me.status(), reqwest::StatusCode::OK);
    let me: Value = me.json().await.unwrap();
    assert_eq!(me["username"], "myjadmin");
    assert_eq!(me["local"], true);

    server.abort();
}

type DeviceButtonInterruptSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn device_button_interrupt_connect(
    state: AppState,
) -> (DeviceButtonInterruptSocket, tokio::task::JoinHandle<()>) {
    let token = issue_device_token(
        "DOLL-0001",
        "test-server-secret",
        chrono::Utc::now().timestamp() + 3600,
    )
    .unwrap();
    let (address, server) = spawn_test_server(state).await;
    let (socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{address}/api/device/voice?device_id=DOLL-0001&token={token}&in_format=mp3&in_rate=16000&out_format=mp3&out_rate=16000"
    ))
    .await
    .unwrap();
    (socket, server)
}

async fn device_button_interrupt_send(socket: &mut DeviceButtonInterruptSocket, input: Value) {
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            input.to_string().into(),
        ))
        .await
        .unwrap();
}

async fn device_button_interrupt_next(socket: &mut DeviceButtonInterruptSocket) -> Value {
    let message = tokio::time::timeout(std::time::Duration::from_secs(4), socket.next())
        .await
        .expect("timed out waiting for voice websocket event")
        .expect("voice websocket closed before expected event")
        .unwrap();
    serde_json::from_str(&message.into_text().unwrap()).unwrap()
}

async fn spawn_device_button_interrupt_delayed_mcp(
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/mcp",
        axum::routing::post(|| async {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            axum::Json(json!({
                "jsonrpc": "2.0",
                "id": "device-button-interrupt-test",
                "result": {
                    "content": [{
                        "type": "text",
                        "text": json!({
                            "ok": true,
                            "code": 0,
                            "data": {
                                "saleOrderId": "device-button-interrupt-order",
                                "orderId": "device-button-interrupt-order",
                                "orderNo": "DEVICE-BUTTON-INTERRUPT-ORDER"
                            }
                        }).to_string()
                    }]
                }
            }))
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (address, task)
}

#[tokio::test]
async fn device_button_interrupt_stops_reply_keeps_order_analysis_fifo_and_allows_next_turn() {
    let state = test_state().await;
    let pool = state.pool.clone();
    let (mcp_address, mcp_server) = spawn_device_button_interrupt_delayed_mcp().await;
    let mut config = db::get_config(&pool).await.unwrap();
    config.order_mcp_enabled = true;
    config.order_mcp_url = format!("http://{mcp_address}/mcp");
    db::save_config(&pool, &config).await.unwrap();

    let (mut socket, server) = device_button_interrupt_connect(state).await;
    let conversation_id = "device-button-interrupt-order-flow";

    device_button_interrupt_send(
        &mut socket,
        json!({"type":"text", "conversation_id":conversation_id, "text":"买一瓶可乐"}),
    )
    .await;
    loop {
        let event = device_button_interrupt_next(&mut socket).await;
        if event["event_type"] == "analysis_done" {
            break;
        }
    }

    device_button_interrupt_send(
        &mut socket,
        json!({"type":"text", "conversation_id":conversation_id, "text":"确认下单"}),
    )
    .await;
    let interrupted_turn_id = loop {
        let event = device_button_interrupt_next(&mut socket).await;
        if event["event_type"] == "asr_final" {
            break event["turn_id"].as_str().unwrap().to_string();
        }
    };
    device_button_interrupt_send(
        &mut socket,
        json!({
            "type":"tts_interrupt",
            "conversation_id":conversation_id,
            "turn_id":interrupted_turn_id,
            "source":"button"
        }),
    )
    .await;

    let mut saw_first_ack = false;
    let mut saw_repeat_ack = false;
    let mut saw_order_draft = false;
    let mut saw_order_created = false;
    let mut saw_analysis_done = false;
    let mut next_turn_id = None;
    let mut saw_next_tts = false;
    let mut saw_next_voice_done = false;
    let mut saw_next_order_draft = false;
    while !saw_analysis_done || !saw_repeat_ack || !saw_next_voice_done || !saw_next_order_draft {
        let event = device_button_interrupt_next(&mut socket).await;
        if saw_first_ack
            && event["turn_id"] == interrupted_turn_id
            && matches!(
                event["event_type"].as_str(),
                Some("llm_delta" | "reply_sentence" | "tts_audio_chunk" | "voice_done")
            )
        {
            panic!("reply event leaked after interrupt acknowledgement: {event}");
        }
        match event["event_type"].as_str() {
            Some("tts_interrupted") if !saw_first_ack => {
                assert_eq!(event["conversation_id"], conversation_id);
                assert_eq!(event["turn_id"], interrupted_turn_id);
                assert_eq!(event["payload"]["source"], "button");
                assert_eq!(event["payload"]["status"], "interrupted");
                saw_first_ack = true;
                device_button_interrupt_send(
                    &mut socket,
                    json!({
                        "type":"tts_interrupt",
                        "conversation_id":conversation_id,
                        "turn_id":interrupted_turn_id,
                        "source":"button"
                    }),
                )
                .await;
                device_button_interrupt_send(
                    &mut socket,
                    json!({"type":"text", "conversation_id":conversation_id, "text":"再买一瓶水"}),
                )
                .await;
            }
            Some("tts_interrupted") if event["turn_id"] == interrupted_turn_id => {
                assert_eq!(event["payload"]["status"], "already_interrupted");
                saw_repeat_ack = true;
            }
            Some("asr_final") if event["turn_id"] != interrupted_turn_id => {
                next_turn_id = event["turn_id"].as_str().map(ToString::to_string);
            }
            Some("tts_audio_chunk") if event["turn_id"] != interrupted_turn_id => {
                saw_next_tts = true;
            }
            Some("voice_done") if event["turn_id"] != interrupted_turn_id => {
                saw_next_voice_done = true;
            }
            Some("order_draft") if event["turn_id"] == interrupted_turn_id => {
                saw_order_draft = true;
            }
            Some("order_draft") => {
                assert!(
                    saw_order_created,
                    "the next purchase draft overtook the interrupted confirmation order"
                );
                saw_next_order_draft = true;
            }
            Some("order_created") if event["turn_id"] == interrupted_turn_id => {
                saw_order_created = true;
            }
            Some("analysis_done") if event["turn_id"] == interrupted_turn_id => {
                saw_analysis_done = true;
            }
            _ => {}
        }
    }
    assert!(saw_order_draft);
    assert!(saw_order_created);
    assert_ne!(next_turn_id.as_deref(), Some(interrupted_turn_id.as_str()));
    assert!(saw_next_tts);
    assert!(saw_next_voice_done);

    let assistant_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_messages WHERE conversation_id = ? AND turn_id = ? AND role = 'assistant'",
    )
    .bind(conversation_id)
    .bind(&interrupted_turn_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        assistant_count, 0,
        "interrupted reply must not be persisted"
    );

    device_button_interrupt_send(
        &mut socket,
        json!({
            "type":"tts_interrupt",
            "conversation_id":conversation_id,
            "turn_id":interrupted_turn_id,
            "source":"button"
        }),
    )
    .await;
    let finished_ack = loop {
        let event = device_button_interrupt_next(&mut socket).await;
        if event["event_type"] == "tts_interrupted" {
            break event;
        }
    };
    assert_eq!(finished_ack["payload"]["status"], "already_finished");

    device_button_interrupt_send(
        &mut socket,
        json!({"type":"text", "conversation_id":conversation_id, "text":"最后再问一次"}),
    )
    .await;
    loop {
        let event = device_button_interrupt_next(&mut socket).await;
        assert!(
            !(event["turn_id"] == interrupted_turn_id
                && matches!(
                    event["event_type"].as_str(),
                    Some("llm_delta" | "reply_sentence" | "tts_audio_chunk" | "voice_done")
                )),
            "target turn leaked a final reply packet after already_finished acknowledgement: {event}"
        );
        if event["event_type"] == "asr_final" && event["turn_id"] != interrupted_turn_id {
            break;
        }
    }

    socket.close(None).await.unwrap();
    server.abort();
    mcp_server.abort();
}

#[tokio::test]
async fn device_button_interrupt_rejects_bad_control_without_cancelling_real_turn() {
    let (mut socket, server) = device_button_interrupt_connect(test_state().await).await;
    let conversation_id = "device-button-interrupt-validation";
    device_button_interrupt_send(
        &mut socket,
        json!({"type":"text", "conversation_id":conversation_id, "text":"买一瓶水"}),
    )
    .await;
    let real_turn_id = loop {
        let event = device_button_interrupt_next(&mut socket).await;
        if event["event_type"] == "asr_final" {
            break event["turn_id"].as_str().unwrap().to_string();
        }
    };

    let mut saw_tts = false;
    let mut saw_voice_done = false;
    for input in [
        json!({"type":"tts_interrupt", "conversation_id":conversation_id, "source":"button"}),
        json!({"type":"tts_interrupt", "conversation_id":conversation_id, "turn_id":real_turn_id}),
        json!({"type":"tts_interrupt", "conversation_id":conversation_id, "turn_id":real_turn_id, "source":"voice"}),
        json!({"type":"tts_interrupt", "conversation_id":conversation_id, "turn_id":"unknown-turn", "source":"button"}),
        json!({"type":"tts_interrupt", "conversation_id":"another-conversation", "turn_id":real_turn_id, "source":"button"}),
    ] {
        device_button_interrupt_send(&mut socket, input).await;
        let error = loop {
            let event = device_button_interrupt_next(&mut socket).await;
            if event["event_type"] == "tts_audio_chunk" && event["turn_id"] == real_turn_id {
                saw_tts = true;
            }
            if event["event_type"] == "voice_done" && event["turn_id"] == real_turn_id {
                saw_voice_done = true;
            }
            if event["event_type"] == "error" {
                break event;
            }
        };
        assert_eq!(error["payload"]["code"], "bad_request");
    }

    while !saw_voice_done {
        let event = device_button_interrupt_next(&mut socket).await;
        if event["event_type"] == "tts_audio_chunk" && event["turn_id"] == real_turn_id {
            saw_tts = true;
        }
        if event["event_type"] == "voice_done" && event["turn_id"] == real_turn_id {
            saw_voice_done = true;
        }
    }
    assert!(saw_tts, "invalid controls must not cancel the real turn");
    socket.close(None).await.unwrap();
    server.abort();
}

#[tokio::test]
async fn device_button_interrupt_whole_segment_reader_remains_responsive() {
    let (mut socket, server) = device_button_interrupt_connect(test_state().await).await;
    let conversation_id = "device-button-interrupt-whole-segment";
    let audio = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [1, 2, 3, 4]);
    device_button_interrupt_send(
        &mut socket,
        json!({"type":"audio_segment", "conversation_id":conversation_id, "audio":audio}),
    )
    .await;
    let turn_id = loop {
        let event = device_button_interrupt_next(&mut socket).await;
        if event["event_type"] == "asr_final" {
            break event["turn_id"].as_str().unwrap().to_string();
        }
    };
    device_button_interrupt_send(
        &mut socket,
        json!({"type":"tts_interrupt", "conversation_id":conversation_id, "turn_id":turn_id, "source":"button"}),
    )
    .await;
    let ack = loop {
        let event = device_button_interrupt_next(&mut socket).await;
        if event["event_type"] == "tts_interrupted" {
            break event;
        }
    };
    assert_eq!(ack["payload"]["status"], "interrupted");
    socket.close(None).await.unwrap();
    server.abort();
}

#[tokio::test]
async fn device_button_interrupt_streaming_asr_reader_remains_responsive() {
    let (mut socket, server) = device_button_interrupt_connect(test_state().await).await;
    let conversation_id = "device-button-interrupt-streaming";
    device_button_interrupt_send(
        &mut socket,
        json!({"type":"audio_stream_start", "conversation_id":conversation_id}),
    )
    .await;
    let turn_id = loop {
        let event = device_button_interrupt_next(&mut socket).await;
        if event["event_type"] == "asr_final" {
            break event["turn_id"].as_str().unwrap().to_string();
        }
    };
    device_button_interrupt_send(
        &mut socket,
        json!({"type":"tts_interrupt", "conversation_id":conversation_id, "turn_id":turn_id, "source":"button"}),
    )
    .await;
    let ack = loop {
        let event = device_button_interrupt_next(&mut socket).await;
        if event["event_type"] == "tts_interrupted" {
            break event;
        }
    };
    assert_eq!(ack["payload"]["status"], "interrupted");
    socket.close(None).await.unwrap();
    server.abort();
}
