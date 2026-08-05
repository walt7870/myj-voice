use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use url::Url;

type HmacSha256 = Hmac<Sha256>;

pub fn build_signed_ws_url(
    endpoint: &str,
    api_key: &str,
    api_secret: &str,
    date: &str,
) -> Result<String> {
    let mut url = Url::parse(endpoint).context("invalid websocket endpoint")?;
    let host = url
        .host_str()
        .context("endpoint host is required")?
        .to_string();
    let path = match url.query() {
        Some(query) => format!("{}?{}", url.path(), query),
        None => url.path().to_string(),
    };
    let signature_origin = format!("host: {host}\ndate: {date}\nGET {path} HTTP/1.1");
    let mut mac =
        HmacSha256::new_from_slice(api_secret.as_bytes()).context("invalid api secret")?;
    mac.update(signature_origin.as_bytes());
    let signature = STANDARD.encode(mac.finalize().into_bytes());
    let authorization_origin = format!(
        "api_key=\"{api_key}\", algorithm=\"hmac-sha256\", headers=\"host date request-line\", signature=\"{signature}\""
    );
    let authorization = STANDARD.encode(authorization_origin.as_bytes());

    url.query_pairs_mut()
        .append_pair("authorization", &authorization)
        .append_pair("date", date)
        .append_pair("host", &host);
    Ok(url.to_string())
}

pub fn current_rfc1123_date() -> String {
    chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}
