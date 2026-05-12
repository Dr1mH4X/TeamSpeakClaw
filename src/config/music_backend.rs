use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct MusicBackendConfig {
    pub backend: String,
    pub base_url: String,
    pub musicbot_name: String,
}

impl Default for MusicBackendConfig {
    fn default() -> Self {
        Self {
            backend: "ts3audiobot".to_string(),
            base_url: "http://127.0.0.1:8009".to_string(),
            musicbot_name: "TS3AudioBot".to_string(),
        }
    }
}
