use std::{net::SocketAddr, str::FromStr, sync::Arc};

use anyhow::Result;
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};
use tracing_subscriber::EnvFilter;

use mjy_voice_shop_rs::{
    admin_auth::{self, AdminConfig, ADMIN_USERNAME},
    db,
    web::{router, AppState},
};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

    let database_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://mjy_voice_shop.db".to_string());
    let options = SqliteConnectOptions::from_str(&database_url)?.create_if_missing(true);
    let pool = SqlitePool::connect_with(options).await?;
    db::init(&pool).await?;
    admin_auth::init_schema(&pool).await?;

    let admin_username =
        std::env::var("ADMIN_USERNAME").unwrap_or_else(|_| ADMIN_USERNAME.to_string());
    let admin_password_hash = match std::env::var("ADMIN_PASSWORD_HASH") {
        Ok(value) => value,
        Err(_)
            if host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback()) =>
        {
            tracing::warn!("using development-only admin credentials on loopback bind");
            admin_auth::hash_password("local-admin-password")?
        }
        Err(_) => anyhow::bail!("ADMIN_PASSWORD_HASH is required for a public bind"),
    };
    let admin_config = AdminConfig::new(admin_username, admin_password_hash)?;

    let state = AppState {
        pool,
        server_secret: Arc::new(
            std::env::var("SERVER_SECRET").unwrap_or_else(|_| "local-dev-secret".to_string()),
        ),
        admin_config,
        diagnostics: tokio::sync::broadcast::channel(256).0,
    };
    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8787);
    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    tracing::info!("mjy voice shop listening on http://{host}:{port}");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
