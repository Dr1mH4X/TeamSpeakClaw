use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    /// 启用全模态模型支持（直接语音输入/输出）
    #[serde(default)]
    pub omni_model: bool,
    /// 最大上下文对话轮数（0 表示禁用上下文）
    #[serde(default)]
    pub max_context_turns: usize,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            omni_model: false,
            max_context_turns: 0,
        }
    }
}
