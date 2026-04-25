use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BotConfig {
    #[serde(default = "default_bot_nickname")]
    pub nickname: String,
    pub trigger_prefixes: Vec<String>,
    pub respond_to_private: bool,
    pub default_reply_mode: String,
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
            default_reply_mode: "private".to_string(),
        }
    }
}
