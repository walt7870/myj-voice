use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

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
            .header(
                HeaderName::from_static("x-lt-traceid"),
                format!("mjy-{}", Uuid::new_v4()),
            )
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
        let raw_body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                return order_error(
                    "ORDER_MCP_BAD_RESPONSE",
                    &format!("订单 MCP 返回无法读取：{error}"),
                );
            }
        };
        if !status.is_success() {
            return order_error(
                "ORDER_MCP_HTTP_ERROR",
                &format!("订单 MCP HTTP 状态异常：{status}"),
            );
        }
        let body = match parse_mcp_http_body(&raw_body) {
            Ok(body) => body,
            Err(error) => return order_error("ORDER_MCP_BAD_RESPONSE", &error),
        };
        if let Some(error) = body.get("error") {
            return order_error(
                "ORDER_MCP_JSONRPC_ERROR",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("订单 MCP JSON-RPC 调用失败"),
            );
        }
        body
    }
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
    let text = rpc
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    if text.trim().is_empty() {
        return Err("订单 MCP 返回内容为空".to_string());
    }
    serde_json::from_str(text).map_err(|error| format!("订单 MCP 工具结果无法解析：{error}"))
}

#[cfg(test)]
mod tests {
    use super::{parse_mcp_http_body, OrderMcpClient};
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
