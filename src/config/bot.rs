use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BotConfig {
    #[serde(default = "default_bot_nickname")]
    pub nickname: String,
    pub trigger_prefixes: Vec<String>,
    pub respond_to_private: bool,
    pub max_concurrent_requests: u32,
    pub default_reply_mode: String,
    pub max_tool_turns: u32,
}

fn default_bot_nickname() -> String {
    "TSClaw".to_string()
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            nickname: default_bot_nickname(),
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
