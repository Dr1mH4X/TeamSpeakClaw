use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct NapCatConfig {
    pub enabled: bool,
    pub ws_url: String,
    pub access_token: String,
    pub listen_groups: Vec<i64>,
    pub trigger_prefixes: Vec<String>,
    pub trusted_groups: Vec<i64>,
    pub trusted_users: Vec<i64>,
}

impl Default for NapCatConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ws_url: "ws://127.0.0.1:3001".to_string(),
            access_token: String::new(),
            listen_groups: vec![],
            trigger_prefixes: vec!["!claw".to_string(), "!bot".to_string()],
            trusted_groups: vec![],
            trusted_users: vec![],
        }
    }
}
