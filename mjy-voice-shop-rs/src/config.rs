use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub app_id: String,
    pub api_key: String,
    pub api_secret: String,
    pub iat_endpoint: String,
    #[serde(default = "default_iat_provider")]
    pub iat_provider: String,
    #[serde(default = "default_tts_provider")]
    pub tts_provider: String,
    pub tts_endpoint: String,
    #[serde(default = "default_standard_tts_endpoint")]
    pub tts_standard_endpoint: String,
    #[serde(default = "default_standard_tts_voice")]
    pub tts_standard_voice: String,
    #[serde(default = "default_tts_voice_name")]
    pub tts_voice_name: String,
    pub tts_voice: String,
    #[serde(default = "default_tts_no_interrupt")]
    pub tts_no_interrupt: bool,
    #[serde(default = "default_tts_interrupt_word")]
    pub tts_interrupt_word: String,
    pub llm_endpoint: String,
    pub llm_model: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub role_prompt: String,
    pub analysis_prompt: String,
    #[serde(default = "default_order_mcp_url")]
    pub order_mcp_url: String,
    #[serde(default = "default_order_mcp_enabled")]
    pub order_mcp_enabled: bool,
    #[serde(default)]
    pub order_mcp_token: String,
    #[serde(default = "default_order_context")]
    pub order_context: Value,
    #[serde(default = "default_order_mcp_tools")]
    pub order_mcp_tools: Value,
    pub mock_providers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicAppConfig {
    pub app_id: String,
    pub api_key_masked: String,
    pub api_secret_masked: String,
    pub iat_endpoint: String,
    #[serde(default = "default_iat_provider")]
    pub iat_provider: String,
    pub tts_provider: String,
    pub tts_endpoint: String,
    pub tts_standard_endpoint: String,
    pub tts_standard_voice: String,
    pub tts_voice_name: String,
    pub tts_voice: String,
    pub tts_no_interrupt: bool,
    pub tts_interrupt_word: String,
    pub available_super_smart_voices: Vec<VoiceOption>,
    pub llm_endpoint: String,
    pub llm_model: String,
    pub available_models: Vec<String>,
    pub temperature: f32,
    pub max_tokens: u32,
    pub role_prompt: String,
    pub analysis_prompt: String,
    pub order_mcp_url: String,
    pub order_mcp_enabled: bool,
    pub order_mcp_token_masked: String,
    pub order_context: Value,
    pub order_mcp_tools: Value,
    pub mock_providers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VoiceOption {
    pub name: String,
    pub code: String,
}

impl AppConfig {
    pub fn default_from_env() -> Self {
        Self {
            app_id: std::env::var("XF_APP_ID").unwrap_or_else(|_| "048c5dc4".to_string()),
            api_key: std::env::var("XF_API_KEY").unwrap_or_default(),
            api_secret: std::env::var("XF_API_SECRET").unwrap_or_default(),
            iat_endpoint: std::env::var("XF_IAT_ENDPOINT").unwrap_or_else(|_| "ws://iat.xf-yun.com/v1".to_string()),
            iat_provider: std::env::var("XF_IAT_PROVIDER")
                .unwrap_or_else(|_| default_iat_provider()),
            tts_provider: std::env::var("XF_TTS_PROVIDER").unwrap_or_else(|_| default_tts_provider()),
            tts_endpoint: std::env::var("XF_TTS_ENDPOINT")
                .unwrap_or_else(|_| "wss://cbm01.cn-huabei-1.xf-yun.com/v1/private/mcd9m97e6".to_string()),
            tts_standard_endpoint: std::env::var("XF_STANDARD_TTS_ENDPOINT")
                .unwrap_or_else(|_| default_standard_tts_endpoint()),
            tts_standard_voice: std::env::var("XF_STANDARD_TTS_VOICE")
                .unwrap_or_else(|_| default_standard_tts_voice()),
            tts_voice_name: std::env::var("XF_TTS_VOICE_NAME")
                .unwrap_or_else(|_| default_tts_voice_name()),
            tts_voice: std::env::var("XF_TTS_VOICE").unwrap_or_else(|_| default_tts_voice()),
            tts_no_interrupt: default_tts_no_interrupt(),
            tts_interrupt_word: std::env::var("TTS_INTERRUPT_WORD")
                .unwrap_or_else(|_| default_tts_interrupt_word()),
            llm_endpoint: std::env::var("XF_LLM_ENDPOINT")
                .unwrap_or_else(|_| "wss://maas-api.cn-huabei-1.xf-yun.com/v1.1/chat".to_string()),
            llm_model: std::env::var("XF_LLM_MODEL").unwrap_or_else(|_| "xopdeepseekv4flash".to_string()),
            temperature: 0.4,
            max_tokens: 1024,
            role_prompt: "你是美宜佳智能玩偶，回复要短、自然、适合语音播报。遇到购买意图先帮用户确认商品，用户明确确认后才下发订单；订单已下发后不要重复确认下单。用户说退下、推下、退出时只是结束本轮交互，不代表退单；裸词‘退单/退款’、否定或讨论退单也不能执行。只有用户明确说出‘我要退单/退款’‘帮我取消订单’这类完整请求时，才说明已处理退单/取消并结束本轮对话。".to_string(),
            analysis_prompt: "分析用户是否有购买意图，抽取商品、数量、规格，并输出结构化结果。".to_string(),
            order_mcp_url: std::env::var("MJY_ORDER_MCP_URL")
                .unwrap_or_else(|_| default_order_mcp_url()),
            order_mcp_enabled: order_mcp_enabled_from_env_value(
                std::env::var("MJY_ORDER_MCP_ENABLED").ok().as_deref(),
            ),
            order_mcp_token: std::env::var("MCP_HTTP_TOKEN").unwrap_or_default(),
            order_context: default_order_context(),
            order_mcp_tools: default_order_mcp_tools(),
            mock_providers: mock_providers_from_env_value(std::env::var("MOCK_PROVIDERS").ok().as_deref()),
        }
    }

    pub fn to_public(&self) -> PublicAppConfig {
        PublicAppConfig {
            app_id: self.app_id.clone(),
            api_key_masked: mask_secret(&self.api_key),
            api_secret_masked: mask_secret(&self.api_secret),
            iat_endpoint: self.iat_endpoint.clone(),
            iat_provider: self.iat_provider.clone(),
            tts_provider: self.tts_provider.clone(),
            tts_endpoint: self.tts_endpoint.clone(),
            tts_standard_endpoint: self.tts_standard_endpoint.clone(),
            tts_standard_voice: self.tts_standard_voice.clone(),
            tts_voice_name: self.tts_voice_name.clone(),
            tts_voice: self.tts_voice.clone(),
            tts_no_interrupt: self.tts_no_interrupt,
            tts_interrupt_word: self.tts_interrupt_word.clone(),
            available_super_smart_voices: available_super_smart_voice_options(),
            llm_endpoint: self.llm_endpoint.clone(),
            llm_model: self.llm_model.clone(),
            available_models: available_models(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            role_prompt: self.role_prompt.clone(),
            analysis_prompt: self.analysis_prompt.clone(),
            order_mcp_url: self.order_mcp_url.clone(),
            order_mcp_enabled: self.order_mcp_enabled,
            order_mcp_token_masked: mask_secret(&self.order_mcp_token),
            order_context: self.order_context.clone(),
            order_mcp_tools: self.order_mcp_tools.clone(),
            mock_providers: self.mock_providers,
        }
    }

    pub fn normalize_voice(mut self) -> Self {
        if let Some(voice) = available_super_smart_voice_options()
            .into_iter()
            .find(|voice| voice.code == self.tts_voice)
        {
            self.tts_voice_name = voice.name;
            return self;
        }
        self.tts_voice_name = default_tts_voice_name();
        self.tts_voice = default_tts_voice();
        self
    }
}

fn default_order_mcp_url() -> String {
    "http://127.0.0.1:8765/mcp".to_string()
}

fn default_order_mcp_enabled() -> bool {
    false
}

pub fn order_mcp_enabled_from_env_value(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

pub fn default_order_context() -> Value {
    json!({
        "deviceId": "DOLL-0001",
        "deptId": 999006940,
        "storeId": "999006940",
        "storeName": "美宜佳科技园店",
        "memberId": "demo-member",
        "operatorId": "voice-shop-demo",
        "longitude": 113.9419,
        "latitude": 22.5431,
        "delivery": "pick",
        "xUserId": "2088602924355011",
        "xUserPhone": "13912345678"
    })
}

pub fn default_order_mcp_tools() -> Value {
    json!({
        "resolve_context": "resolveUserContext",
        "authorize_member": "authorizeMember",
        "preview_order": "previewOrder",
        "create_order": "createOrder",
        "list_orders": "listUserOrders",
        "get_order_detail": "queryOrderDetailInfo",
        "refund_order": "refundOrder"
    })
}

fn default_iat_provider() -> String {
    "super_smart".to_string()
}

fn default_tts_provider() -> String {
    "super_smart".to_string()
}

fn default_standard_tts_endpoint() -> String {
    "wss://tts-api.xfyun.cn/v2/tts".to_string()
}

fn default_standard_tts_voice() -> String {
    "x4_lingxiaolu_em_v2".to_string()
}

fn default_tts_voice_name() -> String {
    "聆小璇".to_string()
}

fn default_tts_voice() -> String {
    "x6_lingxiaoxuan_pro".to_string()
}

fn default_tts_no_interrupt() -> bool {
    true
}

fn default_tts_interrupt_word() -> String {
    "停一下".to_string()
}

pub fn available_super_smart_voices() -> Vec<(String, String)> {
    available_super_smart_voice_options()
        .into_iter()
        .map(|voice| (voice.name, voice.code))
        .collect()
}

fn available_super_smart_voice_options() -> Vec<VoiceOption> {
    vec![
        VoiceOption {
            name: "聆小璇".to_string(),
            code: "x6_lingxiaoxuan_pro".to_string(),
        },
        VoiceOption {
            name: "聆飞瀚".to_string(),
            code: "x6_lingfeihan_pro".to_string(),
        },
    ]
}

pub fn mock_providers_from_env_value(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

pub fn available_models() -> Vec<String> {
    vec![
        "xopdeepseekv4pro".to_string(),
        "xopdeepseekv4flash".to_string(),
        "xopdsv32exp".to_string(),
    ]
}

fn mask_secret(value: &str) -> String {
    if value.is_empty() {
        return "".to_string();
    }
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= 8 {
        return "****".to_string();
    }
    format!(
        "{}****{}",
        chars.iter().take(4).collect::<String>(),
        chars
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>()
    )
}
