use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BotConfig {
    pub trigger_prefixes: Vec<String>,
    pub respond_to_private: bool,
    pub max_concurrent_requests: u32,
    pub default_reply_mode: String,
    pub max_tool_turns: u32,
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            trigger_prefixes: vec![
                "!tsclaw".to_string(),
                "!bot".to_string(),
                "@TSClaw".to_string(),
            ],
            respond_to_private: true,
            max_concurrent_requests: 4,
            default_reply_mode: "private".to_string(),
            max_tool_turns: 3,
        }
    }
}
