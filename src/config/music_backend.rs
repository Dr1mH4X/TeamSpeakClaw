use super::toml_value;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MusicBackendConfig {
    pub backend: String,
    pub base_url: String,
}

impl Default for MusicBackendConfig {
    fn default() -> Self {
        Self {
            backend: "ts3audiobot".to_string(),
            base_url: "http://127.0.0.1:8009".to_string(),
        }
    }
}

impl MusicBackendConfig {
    pub fn to_toml(&self) -> String {
        let mut output = String::new();
        output.push_str("[music_backend]\n");
        output.push_str(&format!("backend = {}\n", toml_value(&self.backend)));
        output.push_str(&format!("base_url = {}\n", toml_value(&self.base_url)));
        output
    }
}
