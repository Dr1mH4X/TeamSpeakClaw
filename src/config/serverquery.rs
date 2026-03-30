use super::toml_value;
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

impl SqConfig {
    pub fn to_toml(&self) -> String {
        let mut output = String::new();
        output.push_str("[serverquery]\n");
        output.push_str(&format!("host = {}\n", toml_value(&self.host)));
        output.push_str(&format!("port = {}\n", self.port));
        output.push_str(&format!("ssh_port = {}\n", self.ssh_port));
        output.push_str(&format!("method = {}\n", toml_value(&self.method)));
        output.push_str(&format!("login_name = {}\n", toml_value(&self.login_name)));
        output.push_str(&format!("login_pass = {}\n", toml_value(&self.login_pass)));
        output.push_str(&format!("server_id = {}\n", self.server_id));
        output.push_str(&format!(
            "bot_nickname = {}\n",
            toml_value(&self.bot_nickname)
        ));
        output
    }
}
