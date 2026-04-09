use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct HeadlessConfig {
    pub enabled: bool,
    pub ts3_host: String,
    pub ts3_port: u16,
    pub server_password: String,
    pub channel_password: String,
    pub channel_path: String,
    pub channel_id: String,
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ts3_host: "127.0.0.1".to_string(),
            ts3_port: 9987,
            server_password: String::new(),
            channel_password: String::new(),
            channel_path: String::new(),
            channel_id: String::new(),
        }
    }
}
