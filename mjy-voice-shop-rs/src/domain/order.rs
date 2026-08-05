use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::matching::ProductMatch;

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
    headers: HeaderMap,
    http: reqwest::Client,
}

impl OrderMcpClient {
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            headers: HeaderMap::new(),
            http: reqwest::Client::new(),
        }
    }

    pub fn new_with_context(
        url: impl Into<String>,
        token: impl Into<String>,
        context: &Value,
    ) -> Self {
        let mut client = Self::new(url, token);
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
        client
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Value {
        if self.url.trim().is_empty() {
            return order_error("ORDER_MCP_NOT_CONFIGURED", "订单 MCP 地址未配置");
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
                return order_error(
                    "ORDER_MCP_UNAVAILABLE",
                    &format!("订单 MCP 暂不可用：{error}"),
                );
            }
        };
        let status = response.status();
        let body = match response.json::<Value>().await {
            Ok(body) => body,
            Err(error) => {
                return order_error(
                    "ORDER_MCP_BAD_RESPONSE",
                    &format!("订单 MCP 返回无法解析：{error}"),
                );
            }
        };
        if !status.is_success() {
            return order_error(
                "ORDER_MCP_HTTP_ERROR",
                &format!("订单 MCP HTTP 状态异常：{status}"),
            );
        }
        if let Some(error) = body.get("error") {
            return order_error(
                "ORDER_MCP_JSONRPC_ERROR",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("订单 MCP JSON-RPC 调用失败"),
            );
        }
        let text = body
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        if text.trim().is_empty() {
            return order_error("ORDER_MCP_EMPTY_RESPONSE", "订单 MCP 返回内容为空");
        }
        serde_json::from_str(text)
            .unwrap_or_else(|error| order_error("ORDER_MCP_BAD_TOOL_RESULT", &error.to_string()))
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
