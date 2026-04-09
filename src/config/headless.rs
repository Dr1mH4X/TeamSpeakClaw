use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HeadlessConfig {
    pub enabled: bool,
    pub ts3_host: String,
    pub ts3_port: u16,
    pub nickname: String,
    pub server_password: String,
    pub channel_password: String,
    pub channel_path: String,
    pub channel_id: String,
    pub identity: String,
    pub identity_file: String,
    pub avatar_dir: String,
    pub voice_state_file: String,
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ts3_host: "127.0.0.1".to_string(),
            ts3_port: 9987,
            nickname: "tsbot".to_string(),
            server_password: String::new(),
            channel_password: String::new(),
            channel_path: String::new(),
            channel_id: String::new(),
            identity: String::new(),
            identity_file: "./logs/identity.json".to_string(),
            avatar_dir: String::new(),
            voice_state_file: "./logs/voice_state.json".to_string(),
        }
    }
}
