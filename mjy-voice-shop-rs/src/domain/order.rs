use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use chrono::Utc;
use hmac::{Hmac, Mac};
use rand::RngCore;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::matching::ProductMatch;
use crate::config::MjyOpenApiConfig;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MockOrder {
    pub order_id: String,
    pub conversation_id: String,
    pub items: Vec<MockOrderItem>,
    pub total_amount: f64,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MockOrderItem {
    pub product_id: String,
    pub name: String,
    pub spec: String,
    pub quantity: u32,
    pub unit_price: f64,
    pub amount: f64,
}

pub fn create_mock_order(conversation_id: &str, matches: &[ProductMatch]) -> MockOrder {
    let items = matches
        .iter()
        .map(|item| MockOrderItem {
            product_id: item.product_id.clone(),
            name: item.name.clone(),
            spec: item.spec.clone(),
            quantity: item.quantity,
            unit_price: item.unit_price,
            amount: round2(item.unit_price * item.quantity as f64),
        })
        .collect::<Vec<_>>();
    let total_amount = round2(items.iter().map(|item| item.amount).sum());
    MockOrder {
        order_id: format!("MOCK-{}", Utc::now().timestamp_millis()),
        conversation_id: conversation_id.to_string(),
        items,
        total_amount,
        created_at: Utc::now().to_rfc3339(),
    }
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[derive(Debug, Clone)]
pub struct OrderMcpClient {
    url: String,
    token: String,
    trace_id: Option<String>,
    headers: HeaderMap,
    http: reqwest::Client,
}

impl OrderMcpClient {
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            trace_id: None,
            headers: HeaderMap::new(),
            http: reqwest::Client::new(),
        }
    }

    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        let trace_id = trace_id.into();
        if !trace_id.trim().is_empty() {
            self.trace_id = Some(trace_id);
        }
        self
    }

    pub fn trace_id(&self) -> Option<&str> {
        self.trace_id.as_deref()
    }

    pub fn new_with_context(
        url: impl Into<String>,
        token: impl Into<String>,
        context: &Value,
    ) -> Self {
        let mut client = Self::new(url, token);
        for (header_name, keys) in [
            ("__app", &["appId", "app"] as &[&str]),
            ("__appver", &["appVersion", "appver"]),
            ("__src_channel", &["srcChannel", "src_channel"]),
            ("CompanyCode", &["companyCode", "CompanyCode"]),
            ("__store", &["storeId", "store"]),
            ("__storeno", &["storeNo", "storeNumber", "store_no"]),
        ] {
            if let Some(value) = context_string(context, keys) {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(header_name.as_bytes()),
                    HeaderValue::from_str(&value),
                ) {
                    client.headers.insert(name, value);
                }
            }
        }
        if let Some(user_id) = context_string(
            context,
            &["xUserId", "x-user-id", "userId", "uid", "thirdUserId"],
        ) {
            if let Ok(value) = HeaderValue::from_str(&user_id) {
                client
                    .headers
                    .insert(HeaderName::from_static("x-user-id"), value);
            }
        }
        if let Some(phone) = context_string(
            context,
            &["xUserPhone", "x-user-phone", "phone", "userPhone"],
        ) {
            if let Ok(value) = HeaderValue::from_str(&phone) {
                client
                    .headers
                    .insert(HeaderName::from_static("x-user-phone"), value);
            }
        }
        if let Some(auth) = context_string(context, &["xLtAuth", "x-lt-auth", "ltAuth"]) {
            if let Ok(value) = HeaderValue::from_str(&auth) {
                client
                    .headers
                    .insert(HeaderName::from_static("x-lt-auth"), value);
            }
        }
        client
    }

    /// Builds the headers required by the latest BeCoCo MCP contract.
    ///
    /// The member code is fetched from the Myj Open API for every client
    /// construction, then encrypted as `ENC(base64(nonce + ciphertext + tag))`
    /// using the customer's AES-GCM reference implementation.
    pub async fn new_with_member_auth(
        url: impl Into<String>,
        token: impl Into<String>,
        auth: &MjyOpenApiConfig,
    ) -> Result<Self, Value> {
        if !auth.is_configured() {
            return Err(order_error(
                "ORDER_MEMBER_AUTH_NOT_CONFIGURED",
                "美宜佳会员授权配置未完成",
            ));
        }
        let member_code = fetch_member_code(auth).await?;
        let encrypted_member_code = encrypt_member_code(&member_code, &auth.aes_gcm_key)?;
        let mut client = Self::new(url, token);
        for (name, value) in [
            ("x-source", auth.mcp_source.as_str()),
            ("x-member-id", auth.member_id.as_str()),
            ("x-member-code", encrypted_member_code.as_str()),
        ] {
            let value = HeaderValue::from_str(value).map_err(|_| {
                order_error(
                    "ORDER_MEMBER_AUTH_INVALID_HEADER",
                    "美宜佳会员授权请求头包含非法字符",
                )
            })?;
            client.headers.insert(HeaderName::from_static(name), value);
        }
        Ok(client)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Value {
        let trace_id = self
            .trace_id
            .clone()
            .unwrap_or_else(|| format!("mjy-{}", Uuid::new_v4()));
        if self.url.trim().is_empty() {
            return self.error_with_trace(
                &trace_id,
                "ORDER_MCP_NOT_CONFIGURED",
                "订单 MCP 地址未配置",
            );
        }
        let payload = json!({
            "jsonrpc": "2.0",
            "id": format!("rust-{}", Utc::now().timestamp_millis()),
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments
            }
        });
        let mut request = self
            .http
            .post(&self.url)
            .header(HeaderName::from_static("x-lt-traceid"), trace_id.clone())
            .header(ACCEPT, "text/event-stream")
            .json(&payload);
        if !self.token.trim().is_empty() {
            request = request.bearer_auth(&self.token);
        }
        for (name, value) in self.headers.iter() {
            request = request.header(name, value);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                return self.error_with_trace(
                    &trace_id,
                    "ORDER_MCP_UNAVAILABLE",
                    &format!("订单 MCP 暂不可用：{error}"),
                );
            }
        };
        let status = response.status();
        let raw_body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                return self.error_with_trace(
                    &trace_id,
                    "ORDER_MCP_BAD_RESPONSE",
                    &format!("订单 MCP 返回无法读取：{error}"),
                );
            }
        };
        if !status.is_success() {
            return self.error_with_trace(
                &trace_id,
                "ORDER_MCP_HTTP_ERROR",
                &format!("订单 MCP HTTP 状态异常：{status}"),
            );
        }
        let body = match parse_mcp_http_body(&raw_body) {
            Ok(body) => body,
            Err(error) => {
                return self.error_with_trace(&trace_id, "ORDER_MCP_BAD_RESPONSE", &error)
            }
        };
        if let Some(error) = body.get("error") {
            return self.error_with_trace(
                &trace_id,
                "ORDER_MCP_JSONRPC_ERROR",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("订单 MCP JSON-RPC 调用失败"),
            );
        }
        let mut body = body;
        if let Some(object) = body.as_object_mut() {
            object
                .entry("traceId")
                .or_insert_with(|| Value::String(trace_id));
        }
        body
    }

    fn error_with_trace(&self, trace_id: &str, code: &str, message: &str) -> Value {
        let mut value = order_error(code, message);
        value["traceId"] = json!(trace_id);
        value
    }
}

async fn fetch_member_code(auth: &MjyOpenApiConfig) -> Result<String, Value> {
    let base_url = auth.base_url.trim_end_matches('/');
    let token_url = format!("{base_url}/open/getAccessToken");
    let app_secret_md5 = format!("{:x}", md5::compute(auth.app_secret.as_bytes()));
    let http = reqwest::Client::new();
    let timestamp = Utc::now().timestamp().to_string();
    let token_response = http
        .post(&token_url)
        .header("Content-Type", "application/json")
        .header("version", &auth.version)
        .header("timestamp", &timestamp)
        .header(
            "sign",
            open_api_sign(&token_url, &timestamp, &app_secret_md5),
        )
        .json(&json!({
            "appId": auth.app_id,
            "appSecret": app_secret_md5,
        }))
        .send()
        .await
        .map_err(|error| {
            order_error(
                "ORDER_MEMBER_TOKEN_UNAVAILABLE",
                &format!("获取美宜佳 AccessToken 失败：{error}"),
            )
        })?;
    let token_status = token_response.status();
    let token_body = token_response.json::<Value>().await.map_err(|error| {
        order_error(
            "ORDER_MEMBER_TOKEN_BAD_RESPONSE",
            &format!("美宜佳 AccessToken 返回无法解析：{error}"),
        )
    })?;
    let access_token = token_body
        .pointer("/data/accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            order_error(
                "ORDER_MEMBER_TOKEN_REJECTED",
                &format!("美宜佳 AccessToken 获取失败（HTTP {token_status}）"),
            )
        })?;

    let member_url = format!(
        "{base_url}/open/bcc/api/v1/members/codes/{}",
        auth.member_id
    );
    let timestamp = Utc::now().timestamp().to_string();
    let member_response = http
        .get(&member_url)
        .header("token", access_token)
        .header("version", &auth.version)
        .header("timestamp", &timestamp)
        .header(
            "sign",
            open_api_sign(&member_url, &timestamp, &app_secret_md5),
        )
        .header("__app", &auth.app_id)
        .header("__appver", &auth.app_version)
        .header("__company", &auth.company)
        .header("__store", &auth.store_id)
        .header("__storeno", &auth.store_no)
        .header("__src_channel", &auth.source_channel)
        .header("CompanyCode", &auth.company_code)
        .header("debug", &auth.debug)
        .send()
        .await
        .map_err(|error| {
            order_error(
                "ORDER_MEMBER_CODE_UNAVAILABLE",
                &format!("获取美宜佳会员码失败：{error}"),
            )
        })?;
    let member_status = member_response.status();
    let member_body = member_response.json::<Value>().await.map_err(|error| {
        order_error(
            "ORDER_MEMBER_CODE_BAD_RESPONSE",
            &format!("美宜佳会员码返回无法解析：{error}"),
        )
    })?;
    member_body
        .get("data")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| {
            order_error(
                "ORDER_MEMBER_CODE_REJECTED",
                &format!("美宜佳会员码获取失败（HTTP {member_status}）"),
            )
        })
}

fn open_api_sign(url: &str, timestamp: &str, app_secret_md5: &str) -> String {
    let payload = format!("{url}&{timestamp}&{app_secret_md5}").to_ascii_lowercase();
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(app_secret_md5.as_bytes())
        .expect("HMAC accepts arbitrary key lengths");
    mac.update(payload.as_bytes());
    format!("{:x}", mac.finalize().into_bytes())
}

fn encrypt_member_code(member_code: &str, key_string: &str) -> Result<String, Value> {
    let key = Sha256::digest(key_string.as_bytes());
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| {
        order_error(
            "ORDER_MEMBER_CODE_ENCRYPT_FAILED",
            "美宜佳会员码 AES-GCM 密钥初始化失败",
        )
    })?;
    let mut nonce = [0_u8; 12];
    rand::rng().fill_bytes(&mut nonce);
    let ciphertext_and_tag = cipher
        .encrypt(Nonce::from_slice(&nonce), member_code.as_bytes())
        .map_err(|_| {
            order_error(
                "ORDER_MEMBER_CODE_ENCRYPT_FAILED",
                "美宜佳会员码 AES-GCM 加密失败",
            )
        })?;
    let mut combined = Vec::with_capacity(nonce.len() + ciphertext_and_tag.len());
    combined.extend_from_slice(&nonce);
    combined.extend_from_slice(&ciphertext_and_tag);
    Ok(format!("ENC({})", BASE64_STANDARD.encode(combined)))
}

/// Parses both ordinary JSON-RPC responses and the SSE envelope required by
/// the customer MCP contract. The final tool result is still returned as the
/// JSON object embedded in `result.content[0].text`.
pub fn parse_mcp_http_body(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    let rpc = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str::<Value>(trimmed)
            .map_err(|error| format!("订单 MCP JSON 返回无法解析：{error}"))?
    } else {
        let data = trimmed
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .filter(|line| !line.is_empty() && *line != "[DONE]")
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            return Err("订单 MCP SSE 返回没有 data 事件".to_string());
        }
        serde_json::from_str::<Value>(&data)
            .map_err(|error| format!("订单 MCP SSE 返回无法解析：{error}"))?
    };
    if rpc.get("error").is_some() {
        return Ok(rpc);
    }
    if let Some(structured_content) = rpc.pointer("/result/structuredContent") {
        return Ok(structured_content.clone());
    }
    let text = rpc
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    if rpc.pointer("/result/isError").and_then(Value::as_bool) == Some(true) {
        return Ok(order_error(
            "ORDER_MCP_TOOL_ERROR",
            if text.trim().is_empty() {
                "订单 MCP 工具执行失败"
            } else {
                text.trim()
            },
        ));
    }
    if text.trim().is_empty() {
        return Err("订单 MCP 返回内容为空".to_string());
    }
    serde_json::from_str(text).map_err(|error| format!("订单 MCP 工具结果无法解析：{error}"))
}

#[cfg(test)]
mod tests {
    use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
    use sha2::{Digest, Sha256};

    use super::{encrypt_member_code, parse_mcp_http_body, OrderMcpClient};
    use serde_json::json;

    #[test]
    fn parses_json_rpc_tool_result() {
        let rpc = json!({
            "jsonrpc": "2.0",
            "result": {"content": [{"type": "text", "text": "{\"code\":0,\"success\":true}"}]}
        });
        let parsed = parse_mcp_http_body(&rpc.to_string()).unwrap();
        assert_eq!(parsed["code"], 0);
        assert_eq!(parsed["success"], true);
    }

    #[test]
    fn parses_sse_json_rpc_tool_result() {
        let raw = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"data\\\":{\\\"id\\\":123}}\"}]}}\n\n";
        let parsed = parse_mcp_http_body(raw).unwrap();
        assert_eq!(
            parsed.pointer("/data/id").and_then(|v| v.as_i64()),
            Some(123)
        );
    }

    #[test]
    fn prefers_structured_content_from_current_mcp_protocol() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": "structured-result",
            "result": {
                "content": [{"type": "text", "text": "not-json"}],
                "structuredContent": {"code": 0, "success": true, "data": {"result": true}},
                "isError": false
            }
        })
        .to_string();
        let parsed = parse_mcp_http_body(&raw).unwrap();
        assert_eq!(parsed["code"], 0);
        assert_eq!(parsed["data"]["result"], true);
    }

    #[test]
    fn converts_plain_text_tool_errors_to_structured_order_errors() {
        let raw = json!({
            "jsonrpc": "2.0",
            "id": "tool-error",
            "result": {
                "content": [{"type": "text", "text": "An error occurred invoking 'previewOrder'."}],
                "isError": true
            }
        })
        .to_string();
        let parsed = parse_mcp_http_body(&raw).unwrap();
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["code"], "ORDER_MCP_TOOL_ERROR");
        assert_eq!(
            parsed["message"],
            "An error occurred invoking 'previewOrder'."
        );
    }

    #[test]
    fn rejects_sse_without_data() {
        assert!(parse_mcp_http_body("event: message\n\n").is_err());
    }

    #[test]
    fn fixed_customer_context_builds_document_headers() {
        let client = OrderMcpClient::new_with_context(
            "https://mcp.example.invalid/mcp",
            "token",
            &json!({
                "appId": "app-1",
                "appVersion": "1.0",
                "srcChannel": 1,
                "companyCode": "CC",
                "storeId": 57,
                "storeNo": "MJ-057",
                "xUserId": "member-1",
                "xLtAuth": "encrypted-member-context"
            }),
        );
        assert_eq!(client.headers.get("__app").unwrap(), "app-1");
        assert_eq!(client.headers.get("__appver").unwrap(), "1.0");
        assert_eq!(client.headers.get("__src_channel").unwrap(), "1");
        assert_eq!(client.headers.get("CompanyCode").unwrap(), "CC");
        assert_eq!(client.headers.get("__store").unwrap(), "57");
        assert_eq!(client.headers.get("__storeno").unwrap(), "MJ-057");
        assert_eq!(client.headers.get("x-user-id").unwrap(), "member-1");
        assert_eq!(
            client.headers.get("x-lt-auth").unwrap(),
            "encrypted-member-context"
        );
    }

    #[test]
    fn encrypts_member_code_with_customer_aes_gcm_envelope() {
        let plaintext = "123456789012345678";
        let key_string = "customer-key-material";
        let encrypted = encrypt_member_code(plaintext, key_string).unwrap();
        assert!(encrypted.starts_with("ENC("));
        assert!(encrypted.ends_with(')'));

        let raw = BASE64_STANDARD
            .decode(&encrypted[4..encrypted.len() - 1])
            .unwrap();
        assert_eq!(raw.len(), 12 + plaintext.len() + 16);
        let key = Sha256::digest(key_string.as_bytes());
        let cipher = Aes256Gcm::new_from_slice(&key).unwrap();
        let decrypted = cipher
            .decrypt(Nonce::from_slice(&raw[..12]), &raw[12..])
            .unwrap();
        assert_eq!(String::from_utf8(decrypted).unwrap(), plaintext);
    }
}

pub fn order_error(code: &str, message: &str) -> Value {
    json!({
        "ok": false,
        "code": code,
        "message": message
    })
}

fn context_string(context: &Value, keys: &[&str]) -> Option<String> {
    let object = context.as_object()?;
    for key in keys {
        if let Some(value) = object.get(*key) {
            if let Some(text) = value.as_str() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            } else if value.is_number() {
                return Some(value.to_string());
            }
        }
    }
    None
}
