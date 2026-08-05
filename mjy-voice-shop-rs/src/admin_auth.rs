use std::sync::Arc;

use anyhow::{bail, Result};
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

pub const ADMIN_COOKIE: &str = "mjy_admin_session";
pub const ADMIN_USERNAME: &str = "myjadmin";

#[derive(Clone)]
pub struct AdminConfig {
    pub username: Arc<String>,
    pub password_hash: Arc<String>,
    pub fingerprint: Arc<String>,
    pub secure_cookie: bool,
}

impl AdminConfig {
    pub fn new(username: impl Into<String>, password_hash: impl Into<String>) -> Result<Self> {
        let username = username.into();
        let password_hash = password_hash.into();
        if username != ADMIN_USERNAME {
            bail!("ADMIN_USERNAME must be {ADMIN_USERNAME}");
        }
        PasswordHash::new(&password_hash)
            .map_err(|_| anyhow::anyhow!("invalid ADMIN_PASSWORD_HASH"))?;
        let mut digest = Sha256::new();
        digest.update((username.len() as u64).to_be_bytes());
        digest.update(username.as_bytes());
        digest.update((password_hash.len() as u64).to_be_bytes());
        digest.update(password_hash.as_bytes());
        let fingerprint = format!("{:x}", digest.finalize());
        Ok(Self {
            username: Arc::new(username),
            password_hash: Arc::new(password_hash),
            fingerprint: Arc::new(fingerprint),
            secure_cookie: true,
        })
    }

    pub fn with_secure_cookie(mut self, secure_cookie: bool) -> Self {
        self.secure_cookie = secure_cookie;
        self
    }
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.is_empty() {
        bail!("password must not be empty");
    }
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

pub fn generate_admin_password() -> String {
    let mut bytes = [0_u8; 12];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).ok().is_some_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS admin_sessions (
            session_hash TEXT PRIMARY KEY,
            config_fingerprint TEXT NOT NULL,
            created_at TEXT NOT NULL,
            revoked_at TEXT
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

fn session_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub async fn create_session(pool: &SqlitePool, config: &AdminConfig) -> Result<String> {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    sqlx::query(
        "INSERT INTO admin_sessions(session_hash, config_fingerprint, created_at, revoked_at) VALUES(?, ?, ?, NULL)",
    )
    .bind(session_hash(&token))
    .bind(config.fingerprint.as_str())
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(token)
}

pub async fn load_session(pool: &SqlitePool, config: &AdminConfig, token: &str) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM admin_sessions WHERE session_hash = ? AND config_fingerprint = ? AND revoked_at IS NULL",
    )
    .bind(session_hash(token))
    .bind(config.fingerprint.as_str())
    .fetch_one(pool)
    .await?;
    Ok(count == 1)
}

pub async fn revoke_session(pool: &SqlitePool, token: &str) -> Result<()> {
    sqlx::query("UPDATE admin_sessions SET revoked_at = ? WHERE session_hash = ?")
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(session_hash(token))
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
    use std::str::FromStr;
    use tempfile::tempdir;

    async fn test_pool() -> SqlitePool {
        let dir = tempdir().unwrap();
        let path = dir.path().join("admin-session.db");
        let _kept = dir.keep();
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        SqlitePool::connect_with(options).await.unwrap()
    }

    #[test]
    fn argon2_password_round_trip_rejects_wrong_password() {
        let hash = hash_password("correct-pass").unwrap();
        assert!(verify_password(&hash, "correct-pass"));
        assert!(!verify_password(&hash, "wrong-pass"));
    }

    #[test]
    fn only_myjadmin_is_accepted_as_the_single_admin_username() {
        let hash = hash_password("correct-pass").unwrap();
        assert!(AdminConfig::new("myjadmin", hash.clone()).is_ok());
        assert!(AdminConfig::new("admin", hash).is_err());
    }

    #[test]
    fn generated_admin_password_is_24_character_lowercase_hex() {
        for _ in 0..100 {
            let password = generate_admin_password();
            assert_eq!(password.len(), 24);
            assert!(password
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
            assert!(!password.contains('/'));
        }
    }

    #[tokio::test]
    async fn opaque_session_is_revoked_and_invalidated_by_config_change() {
        let pool = test_pool().await;
        init_schema(&pool).await.unwrap();
        init_schema(&pool).await.unwrap();
        let config = AdminConfig::new("myjadmin", hash_password("first-pass").unwrap()).unwrap();
        let token = create_session(&pool, &config).await.unwrap();
        assert!(load_session(&pool, &config, &token).await.unwrap());
        revoke_session(&pool, &token).await.unwrap();
        assert!(!load_session(&pool, &config, &token).await.unwrap());

        let second = create_session(&pool, &config).await.unwrap();
        let changed = AdminConfig::new("myjadmin", hash_password("second-pass").unwrap()).unwrap();
        assert!(!load_session(&pool, &changed, &second).await.unwrap());
    }
}
