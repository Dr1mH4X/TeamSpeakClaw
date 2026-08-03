use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    /// Enable omni-modal model support (voice input/output directly)
    #[serde(default)]
    pub omni_model: bool,
    /// 最大上下文对话轮数（0 表示禁用上下文）
    #[serde(default)]
    pub max_context_turns: usize,
    /// 最大会话数（0 表示不限制）
    #[serde(default = "default_max_context_sessions")]
    pub max_context_sessions: usize,
    /// 最大并发 LLM 请求数
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    /// 建立 LLM API 连接的超时时间（秒）
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// 流式响应连续无数据的超时时间（秒）
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
    /// 单次流式请求允许的总时间（秒）
    #[serde(default = "default_stream_total_timeout_secs")]
    pub stream_total_timeout_secs: u64,
}

fn default_max_context_sessions() -> usize {
    1000
}

fn default_max_concurrent_requests() -> usize {
    4
}

fn default_connect_timeout_secs() -> u64 {
    10
}

fn default_stream_idle_timeout_secs() -> u64 {
    30
}

fn default_stream_total_timeout_secs() -> u64 {
    300
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            omni_model: false,
            max_context_turns: 0,
            max_context_sessions: 1000,
            max_concurrent_requests: default_max_concurrent_requests(),
            connect_timeout_secs: default_connect_timeout_secs(),
            stream_idle_timeout_secs: default_stream_idle_timeout_secs(),
            stream_total_timeout_secs: default_stream_total_timeout_secs(),
        }
    }
}
