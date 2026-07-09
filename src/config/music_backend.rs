use serde::{Deserialize, Serialize};

pub const VALID_BACKENDS: &[&str] = &["ts3audiobot", "tsmusicbot", "tsbot_backend"];

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MusicBackendConfig {
    pub backend: String,
    pub base_url: String,
    pub musicbot_name: String,
}
