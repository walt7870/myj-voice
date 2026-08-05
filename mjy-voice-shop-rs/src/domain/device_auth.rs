use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceClaims {
    pub device_id: String,
    pub exp: i64,
}

pub fn issue_device_token(device_id: &str, server_secret: &str, exp: i64) -> Result<String> {
    let claims = DeviceClaims {
        device_id: device_id.to_string(),
        exp,
    };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
    let signature = sign(&payload, server_secret)?;
    Ok(format!("{payload}.{signature}"))
}

pub fn verify_device_token(token: &str, server_secret: &str, now: i64) -> Result<DeviceClaims> {
    let (payload, signature) = token.split_once('.').context("invalid token shape")?;
    let expected = sign(payload, server_secret)?;
    if signature != expected {
        anyhow::bail!("invalid token signature");
    }
    let claims: DeviceClaims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload)?)?;
    if claims.exp < now {
        anyhow::bail!("device token expired");
    }
    Ok(claims)
}

pub fn secret_hash(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn sign(payload: &str, secret: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())?;
    mac.update(payload.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

trait DigestExt {
    fn digest(data: &[u8]) -> Vec<u8>;
}

impl DigestExt for Sha256 {
    fn digest(data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        let mut hasher = Sha256::new();
        hasher.update(data);
        hasher.finalize().to_vec()
    }
}
