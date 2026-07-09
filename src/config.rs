pub mod acl;
pub mod bot;
pub mod headless;
pub mod llm;
pub mod logging;
pub mod music_backend;
pub mod napcat;
pub mod prompts;
pub use acl::AclConfig;
pub use bot::BotConfig;
pub use headless::HeadlessConfig;
pub use llm::LlmConfig;
pub use logging::LogConfig;
pub use music_backend::MusicBackendConfig;
pub use napcat::NapCatConfig;
pub use prompts::PromptsConfig;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    exe_dir().join("config")
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    pub llm: LlmConfig,
    pub bot: BotConfig,
    pub music_backend: Option<MusicBackendConfig>,
    pub napcat: NapCatConfig,
    pub headless: HeadlessConfig,
    pub logging: LogConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            llm: LlmConfig::default(),
            bot: BotConfig::default(),
            music_backend: None,
            napcat: NapCatConfig::default(),
            headless: HeadlessConfig::default(),
            logging: LogConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load_all() -> Result<(Self, AclConfig, PromptsConfig)> {
        let dir = config_dir();
        Ok((
            Self::load(dir.join("settings.toml"))?,
            AclConfig::load(dir.join("acl.toml"))?,
            PromptsConfig::load(dir.join("prompts.toml"))?,
        ))
    }

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
