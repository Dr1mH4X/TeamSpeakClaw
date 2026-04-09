pub mod acl;
pub mod bot;
pub mod headless;
pub mod llm;
pub mod logging;
pub mod music_backend;
pub mod napcat;
pub mod prompts;
pub mod rate_limit;
pub mod serverquery;

pub use acl::AclConfig;
pub use bot::BotConfig;
pub use headless::HeadlessConfig;
pub use llm::LlmConfig;
pub use logging::LogConfig;
pub use music_backend::MusicBackendConfig;
pub use napcat::NapCatConfig;
pub use prompts::{ErrorPrompts, PromptsConfig};
pub use rate_limit::RateLimitConfig;
pub use serverquery::SqConfig;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn config_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("config")
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    pub serverquery: SqConfig,
    pub llm: LlmConfig,
    pub bot: BotConfig,
    pub rate_limit: RateLimitConfig,
    pub music_backend: MusicBackendConfig,
    pub napcat: NapCatConfig,
    pub headless: HeadlessConfig,
    pub logging: LogConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            serverquery: SqConfig::default(),
            llm: LlmConfig::default(),
            bot: BotConfig::default(),
            rate_limit: RateLimitConfig::default(),
            music_backend: MusicBackendConfig::default(),
            napcat: NapCatConfig::default(),
            headless: HeadlessConfig::default(),
            logging: LogConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).context(format!(
            "Config file not found: {}. Please copy examples/config/settings.toml to config/",
            path.display()
        ))?;
        let config: AppConfig = toml::from_str(&content)?;
        Ok(config)
    }
}
