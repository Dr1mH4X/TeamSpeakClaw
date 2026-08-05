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

use crate::config::music_backend::VALID_BACKENDS;

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
#[serde(default)]
#[derive(Default)]
pub struct AppConfig {
    pub llm: LlmConfig,
    pub bot: BotConfig,
    pub music_backend: Option<MusicBackendConfig>,
    pub napcat: NapCatConfig,
    pub headless: HeadlessConfig,
    pub logging: LogConfig,
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

    pub fn validate(&self) -> Result<()> {
        if self.llm.model.trim().is_empty() {
            anyhow::bail!("llm.model must not be empty");
        }

        let llm_base_url =
            url::Url::parse(&self.llm.base_url).context("llm.base_url must be a valid URL")?;
        if !matches!(llm_base_url.scheme(), "http" | "https") {
            anyhow::bail!("llm.base_url must use http or https");
        }

        if !matches!(
            self.bot.default_reply_mode.as_str(),
            "private" | "channel" | "server"
        ) {
            anyhow::bail!("bot.default_reply_mode must be private, channel, or server");
        }

        if let Some(ref mc) = self.music_backend {
            if !VALID_BACKENDS.contains(&mc.backend.as_str()) {
                anyhow::bail!(
                    "Unsupported music backend '{}'. Supported: {}",
                    mc.backend,
                    VALID_BACKENDS.join(", "),
                );
            }
        }
        Ok(())
    }

    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).context(format!(
            "Config file not found: {}. Please copy examples/config/settings.toml to config/",
            path.display()
        ))?;
        let config: AppConfig = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_llm_section_uses_new_concurrency_default() {
        let config: AppConfig = toml::from_str(
            r#"
[llm]
api_key = "legacy-key"
base_url = "http://127.0.0.1:11434/v1"
model = "legacy-model"
omni_model = true
max_context_turns = 3
"#,
        )
        .unwrap();

        assert_eq!(config.llm.api_key, "legacy-key");
        assert_eq!(config.llm.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(config.llm.model, "legacy-model");
        assert!(config.llm.omni_model);
        assert_eq!(config.llm.max_context_turns, 3);
        assert_eq!(config.bot.default_reply_mode, "private");
        assert!(!config.napcat.enabled);
        assert_eq!(config.logging.max_log_days, 7);
        config.validate().unwrap();
    }

    #[test]
    fn rejects_invalid_reply_mode() {
        let mut config = AppConfig::default();
        config.bot.default_reply_mode = "invalid".to_string();

        let error = config.validate().unwrap_err();

        assert!(error.to_string().contains("default_reply_mode"));
    }
}
