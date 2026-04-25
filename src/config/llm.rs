use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub stream_output: bool,
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
    pub max_concurrent_requests: u32,
    /// 最大工具调用轮数
    #[serde(default = "default_max_tool_turns")]
    pub max_tool_turns: u32,
}

fn default_max_context_sessions() -> usize {
    1000
}

fn default_max_concurrent_requests() -> u32 {
    4
}

fn default_max_tool_turns() -> u32 {
    3
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            stream_output: false,
            omni_model: false,
            max_context_turns: 0,
            max_context_sessions: 1000,
            max_concurrent_requests: 4,
            max_tool_turns: 3,
        }
    }
}
