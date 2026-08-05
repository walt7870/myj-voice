use anyhow::Result;
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};

use crate::config::AppConfig;
use crate::domain::device_auth::secret_hash;
use crate::domain::matching::Product;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationSummary {
    pub conversation_id: String,
    pub device_id: Option<String>,
    pub created_at: String,
    pub last_message_at: Option<String>,
    pub message_count: i64,
    pub last_user_text: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationPage {
    pub items: Vec<ConversationSummary>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
    pub total_pages: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceAuthorization {
    pub device_id: String,
    pub name: String,
    pub enabled: bool,
    pub last_conversation_at: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationMessage {
    pub turn_id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationEvent {
    pub turn_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationOrder {
    pub order_id: String,
    pub conversation_id: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationOwner {
    Browser,
    Device(String),
}

pub async fn connect(database_url: &str) -> Result<SqlitePool> {
    Ok(SqlitePoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await?)
}

pub async fn init(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS app_config (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            data TEXT NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS products (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            aliases TEXT NOT NULL,
            spec TEXT NOT NULL,
            price REAL NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1
        );
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS devices (
            device_id TEXT PRIMARY KEY,
            secret_hash TEXT NOT NULL,
            name TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 1
        );
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS conversations (
            conversation_id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;
    let conversation_columns = sqlx::query("PRAGMA table_info(conversations)")
        .fetch_all(pool)
        .await?;
    if !conversation_columns
        .iter()
        .any(|column| column.get::<String, _>("name") == "device_id")
    {
        sqlx::query("ALTER TABLE conversations ADD COLUMN device_id TEXT NULL")
            .execute(pool)
            .await?;
    }
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_conversations_device_created ON conversations(device_id, created_at DESC)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS conversation_messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id TEXT NOT NULL,
            turn_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_conversation_messages_conversation_created ON conversation_messages(conversation_id, created_at DESC)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS turn_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id TEXT NOT NULL,
            turn_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            payload TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS mock_orders (
            order_id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL,
            payload TEXT NOT NULL,
            created_at TEXT NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;

    seed_config(pool).await?;
    seed_products(pool).await?;
    seed_device(pool).await?;
    Ok(())
}

pub async fn get_config(pool: &SqlitePool) -> Result<AppConfig> {
    let row = sqlx::query("SELECT data FROM app_config WHERE id = 1")
        .fetch_one(pool)
        .await?;
    Ok(serde_json::from_str::<AppConfig>(row.get("data"))?.normalize_voice())
}

pub async fn save_config(pool: &SqlitePool, config: &AppConfig) -> Result<()> {
    sqlx::query("INSERT OR REPLACE INTO app_config(id, data) VALUES(1, ?)")
        .bind(serde_json::to_string(config)?)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_products(pool: &SqlitePool) -> Result<Vec<Product>> {
    let rows = sqlx::query(
        "SELECT id, name, aliases, spec, price FROM products WHERE enabled = 1 ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let aliases: Vec<String> = serde_json::from_str(row.get("aliases"))?;
            Ok(Product {
                id: row.get("id"),
                name: row.get("name"),
                aliases,
                spec: row.get("spec"),
                price: row.get("price"),
            })
        })
        .collect()
}

pub async fn upsert_product(pool: &SqlitePool, product: &Product) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO products(id, name, aliases, spec, price, enabled) VALUES(?, ?, ?, ?, ?, 1)",
    )
    .bind(&product.id)
    .bind(&product.name)
    .bind(serde_json::to_string(&product.aliases)?)
    .bind(&product.spec)
    .bind(product.price)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_device_authorizations(pool: &SqlitePool) -> Result<Vec<DeviceAuthorization>> {
    let rows = sqlx::query(
        r#"
        SELECT
            devices.device_id,
            devices.name,
            devices.enabled,
            MAX(conversations.created_at) AS last_conversation_at
        FROM devices
        LEFT JOIN conversations ON conversations.device_id = devices.device_id
        GROUP BY devices.device_id, devices.name, devices.enabled
        ORDER BY devices.device_id
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| DeviceAuthorization {
            device_id: row.get("device_id"),
            name: row.get("name"),
            enabled: row.get::<i64, _>("enabled") == 1,
            last_conversation_at: row.get("last_conversation_at"),
        })
        .collect())
}

pub async fn create_device_authorization(
    pool: &SqlitePool,
    device_id: &str,
    name: &str,
    secret: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO devices(device_id, secret_hash, name, enabled) VALUES(?, ?, ?, 1)")
        .bind(device_id)
        .bind(secret_hash(secret))
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_device_authorization(
    pool: &SqlitePool,
    device_id: &str,
    name: &str,
    enabled: bool,
) -> Result<bool> {
    let result = sqlx::query("UPDATE devices SET name = ?, enabled = ? WHERE device_id = ?")
        .bind(name)
        .bind(i64::from(enabled))
        .bind(device_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn reset_device_secret(pool: &SqlitePool, device_id: &str, secret: &str) -> Result<bool> {
    let result = sqlx::query("UPDATE devices SET secret_hash = ? WHERE device_id = ?")
        .bind(secret_hash(secret))
        .bind(device_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn device_is_enabled(pool: &SqlitePool, device_id: &str) -> Result<bool> {
    let enabled = sqlx::query_scalar::<_, i64>("SELECT enabled FROM devices WHERE device_id = ?")
        .bind(device_id)
        .fetch_optional(pool)
        .await?;
    Ok(enabled == Some(1))
}

pub async fn log_event(
    pool: &SqlitePool,
    conversation_id: &str,
    turn_id: &str,
    event_type: &str,
    payload: &serde_json::Value,
) {
    let _ = sqlx::query(
        "INSERT INTO turn_events(conversation_id, turn_id, event_type, payload, created_at) VALUES(?, ?, ?, ?, ?)",
    )
    .bind(conversation_id)
    .bind(turn_id)
    .bind(event_type)
    .bind(payload.to_string())
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await;
}

pub async fn ensure_conversation(pool: &SqlitePool, conversation_id: &str) -> Result<()> {
    ensure_conversation_owned(pool, conversation_id, &ConversationOwner::Browser).await
}

pub async fn ensure_conversation_owned(
    pool: &SqlitePool,
    conversation_id: &str,
    owner: &ConversationOwner,
) -> Result<()> {
    let mut transaction = pool.begin().await?;
    let existing_owner = sqlx::query_scalar::<_, Option<String>>(
        "SELECT device_id FROM conversations WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .fetch_optional(&mut *transaction)
    .await?;

    match (existing_owner, owner) {
        (None, owner) => {
            let device_id = match owner {
                ConversationOwner::Browser => None,
                ConversationOwner::Device(device_id) => Some(device_id.as_str()),
            };
            sqlx::query(
                "INSERT INTO conversations(conversation_id, created_at, device_id) VALUES(?, ?, ?)",
            )
            .bind(conversation_id)
            .bind(chrono::Utc::now().to_rfc3339())
            .bind(device_id)
            .execute(&mut *transaction)
            .await?;
        }
        (Some(None), ConversationOwner::Browser) => {}
        (Some(Some(existing_device_id)), ConversationOwner::Device(device_id))
            if existing_device_id == *device_id => {}
        _ => return Err(anyhow::anyhow!("conversation owner mismatch")),
    }

    transaction.commit().await?;
    Ok(())
}

pub async fn append_conversation_message(
    pool: &SqlitePool,
    conversation_id: &str,
    turn_id: &str,
    role: &str,
    content: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO conversation_messages(conversation_id, turn_id, role, content, created_at) VALUES(?, ?, ?, ?, ?)",
    )
    .bind(conversation_id)
    .bind(turn_id)
    .bind(role)
    .bind(content)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn pending_order_user_text(pool: &SqlitePool, conversation_id: &str) -> Result<String> {
    let rows = sqlx::query(
        r#"
        SELECT content
        FROM conversation_messages
        WHERE conversation_id = ?
          AND role = 'user'
          AND created_at > COALESCE(
              (
                  SELECT created_at
                  FROM turn_events
                  WHERE conversation_id = ? AND event_type = 'order_created'
                  ORDER BY id DESC
                  LIMIT 1
              ),
              ''
          )
        ORDER BY id
        "#,
    )
    .bind(conversation_id)
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| row.get::<String, _>("content"))
        .collect::<Vec<_>>()
        .join("\n"))
}

pub async fn list_conversations(pool: &SqlitePool) -> Result<Vec<ConversationSummary>> {
    Ok(list_conversations_page(pool, 1, 100).await?.items)
}

pub async fn list_conversations_page(
    pool: &SqlitePool,
    page: i64,
    page_size: i64,
) -> Result<ConversationPage> {
    let page = page.max(1);
    let page_size = page_size.clamp(5, 50);
    let total: i64 = sqlx::query("SELECT COUNT(*) AS c FROM conversations")
        .fetch_one(pool)
        .await?
        .get("c");
    let total_pages = if total == 0 {
        1
    } else {
        (total + page_size - 1) / page_size
    };
    let page = page.min(total_pages);
    let offset = (page - 1) * page_size;
    let rows = sqlx::query(
        r#"
        SELECT
            c.conversation_id,
            c.device_id,
            c.created_at,
            MAX(m.created_at) AS last_message_at,
            COUNT(m.id) AS message_count,
            (
                SELECT content
                FROM conversation_messages um
                WHERE um.conversation_id = c.conversation_id AND um.role = 'user'
                ORDER BY um.id DESC
                LIMIT 1
            ) AS last_user_text
        FROM conversations c
        LEFT JOIN conversation_messages m ON m.conversation_id = c.conversation_id
        GROUP BY c.conversation_id, c.device_id, c.created_at
        ORDER BY COALESCE(MAX(m.created_at), c.created_at) DESC
        LIMIT ? OFFSET ?
        "#,
    )
    .bind(page_size)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let items = rows
        .into_iter()
        .map(|row| ConversationSummary {
            conversation_id: row.get("conversation_id"),
            device_id: row.get("device_id"),
            created_at: row.get("created_at"),
            last_message_at: row.get("last_message_at"),
            message_count: row.get("message_count"),
            last_user_text: row.get("last_user_text"),
        })
        .collect();

    Ok(ConversationPage {
        items,
        total,
        page,
        page_size,
        total_pages,
    })
}

pub async fn list_conversation_messages(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<ConversationMessage>> {
    let rows = sqlx::query(
        "SELECT turn_id, role, content, created_at FROM conversation_messages WHERE conversation_id = ? ORDER BY id",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ConversationMessage {
            turn_id: row.get("turn_id"),
            role: row.get("role"),
            content: row.get("content"),
            created_at: row.get("created_at"),
        })
        .collect())
}

pub async fn list_conversation_events(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<ConversationEvent>> {
    let rows = sqlx::query(
        "SELECT turn_id, event_type, payload, created_at FROM turn_events WHERE conversation_id = ? ORDER BY id",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let payload: String = row.get("payload");
            Ok(ConversationEvent {
                turn_id: row.get("turn_id"),
                event_type: row.get("event_type"),
                payload: serde_json::from_str(&payload)?,
                created_at: row.get("created_at"),
            })
        })
        .collect()
}

pub async fn list_mock_order_payloads_by_conversation(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<ConversationOrder>> {
    let rows = sqlx::query(
        "SELECT order_id, conversation_id, payload, created_at FROM mock_orders WHERE conversation_id = ? ORDER BY datetime(created_at) DESC, order_id DESC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            let payload: String = row.get("payload");
            Ok(ConversationOrder {
                order_id: row.get("order_id"),
                conversation_id: row.get("conversation_id"),
                payload: serde_json::from_str(&payload)?,
                created_at: row.get("created_at"),
            })
        })
        .collect()
}

pub async fn save_mock_order(
    pool: &SqlitePool,
    order: &crate::domain::order::MockOrder,
) -> Result<()> {
    save_mock_order_payload(
        pool,
        &order.order_id,
        &order.conversation_id,
        &serde_json::to_value(order)?,
    )
    .await
}

pub async fn save_mock_order_payload(
    pool: &SqlitePool,
    order_id: &str,
    conversation_id: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let created_at = payload
        .get("created_at")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    sqlx::query(
        "INSERT OR REPLACE INTO mock_orders(order_id, conversation_id, payload, created_at) VALUES(?, ?, ?, ?)",
    )
    .bind(order_id)
    .bind(conversation_id)
    .bind(payload.to_string())
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_mock_order_payloads(pool: &SqlitePool) -> Result<Vec<serde_json::Value>> {
    let rows = sqlx::query(
        "SELECT payload FROM mock_orders ORDER BY datetime(created_at) DESC, order_id DESC",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let payload: String = row.get("payload");
            Ok(serde_json::from_str(&payload)?)
        })
        .collect()
}

pub async fn get_mock_order_payload(
    pool: &SqlitePool,
    order_id: &str,
) -> Result<Option<serde_json::Value>> {
    let Some(row) = sqlx::query("SELECT payload FROM mock_orders WHERE order_id = ?")
        .bind(order_id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    let payload: String = row.get("payload");
    Ok(Some(serde_json::from_str(&payload)?))
}

async fn seed_config(pool: &SqlitePool) -> Result<()> {
    let count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM app_config")
        .fetch_one(pool)
        .await?
        .get("c");
    if count == 0 {
        save_config(pool, &AppConfig::default_from_env()).await?;
    }
    Ok(())
}

async fn seed_products(pool: &SqlitePool) -> Result<()> {
    let count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM products")
        .fetch_one(pool)
        .await?
        .get("c");
    if count == 0 {
        for product in [
            Product::new("cola-500", "可口可乐", vec!["可乐", "可口"], "500ml", 3.5),
            Product::new(
                "water-555",
                "怡宝矿泉水",
                vec!["水", "矿泉水", "怡宝"],
                "555ml",
                2.0,
            ),
            Product::new("milk-250", "纯牛奶", vec!["牛奶", "奶"], "250ml", 4.5),
        ] {
            upsert_product(pool, &product).await?;
        }
    }
    Ok(())
}

async fn seed_device(pool: &SqlitePool) -> Result<()> {
    let count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM devices")
        .fetch_one(pool)
        .await?
        .get("c");
    if count == 0 {
        sqlx::query(
            "INSERT INTO devices(device_id, secret_hash, name, enabled) VALUES(?, ?, ?, 1)",
        )
        .bind("DOLL-0001")
        .bind(secret_hash("demo-secret"))
        .bind("本地调试玩偶")
        .execute(pool)
        .await?;
    }
    Ok(())
}
