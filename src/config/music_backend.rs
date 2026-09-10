use serde::{Deserialize, Serialize};

pub const VALID_BACKENDS: &[&str] = &["ts3audiobot", "tsmusicbot", "tsbot_backend"];

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MusicBackendConfig {
    pub backend: String,
    /// 仅 `tsbot_backend` 需要；聊天后端可省略
    #[serde(default)]
    pub base_url: String,
    pub musicbot_name: String,
}
