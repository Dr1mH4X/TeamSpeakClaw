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
    /// 对话上下文窗口大小（保留最近 N 轮用户对话，0 表示不保留历史）
    #[serde(default)]
    pub context_window: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            stream_output: false,
            omni_model: false,
            context_window: 0,
        }
    }
}
