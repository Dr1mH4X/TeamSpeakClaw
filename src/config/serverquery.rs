use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SqConfig {
    pub host: String,
    pub port: u16,
    pub ssh_port: u16,
    pub method: String,
    pub login_name: String,
    pub login_pass: String,
    pub server_id: u32,
    pub bot_nickname: String,
}

impl Default for SqConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 10011,
            ssh_port: 10022,
            method: "tcp".to_string(),
            login_name: "serveradmin".to_string(),
            login_pass: String::new(),
            server_id: 1,
            bot_nickname: "TSClaw".to_string(),
        }
    }
}
