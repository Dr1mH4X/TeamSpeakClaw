use super::toml_value;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NapCatConfig {
    pub enabled: bool,
    pub ws_url: String,
    pub access_token: String,
    pub respond_to_private: bool,
    pub listen_groups: Vec<i64>,
    pub trigger_prefixes: Vec<String>,
}

impl Default for NapCatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ws_url: "ws://127.0.0.1:3001".to_string(),
            access_token: String::new(),
            respond_to_private: true,
            listen_groups: vec![],
            trigger_prefixes: vec!["!claw".to_string(), "!bot".to_string()],
        }
    }
}

impl NapCatConfig {
    pub fn to_toml(&self) -> String {
        let mut output = String::new();
        output.push_str("[napcat]\n");
        output.push_str(&format!("enabled = {}\n", self.enabled));
        output.push_str(&format!("ws_url = {}\n", toml_value(&self.ws_url)));
        output.push_str(&format!(
            "access_token = {}\n",
            toml_value(&self.access_token)
        ));
        output.push_str(&format!(
            "respond_to_private = {}\n",
            self.respond_to_private
        ));
        output.push_str(&format!(
            "listen_groups = {}\n",
            toml_value(&self.listen_groups)
        ));
        output.push_str(&format!(
            "trigger_prefixes = {}\n",
            toml_value(&self.trigger_prefixes)
        ));
        output
    }
}
