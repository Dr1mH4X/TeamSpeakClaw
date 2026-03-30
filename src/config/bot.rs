use super::toml_value;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BotConfig {
    pub trigger_prefixes: Vec<String>,
    pub respond_to_private: bool,
    pub max_concurrent_requests: u32,
    pub default_reply_mode: String,
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
        }
    }
}

impl BotConfig {
    pub fn to_toml(&self) -> String {
        let mut output = String::new();
        output.push_str("[bot]\n");
        output.push_str(&format!(
            "trigger_prefixes = {}\n",
            toml_value(&self.trigger_prefixes)
        ));
        output.push_str(&format!(
            "respond_to_private = {}\n",
            self.respond_to_private
        ));
        output.push_str(&format!(
            "max_concurrent_requests = {}\n",
            self.max_concurrent_requests
        ));
        output.push_str(&format!(
            "default_reply_mode = {}\n",
            toml_value(&self.default_reply_mode)
        ));
        output
    }
}
