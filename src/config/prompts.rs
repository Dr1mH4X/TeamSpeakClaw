use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PromptsConfig {
    pub system: SystemPrompts,
    pub tts: TtsPrompts,
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self {
            system: SystemPrompts::default(),
            tts: TtsPrompts::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SystemPrompts {
    pub content: String,
}

impl Default for SystemPrompts {
    fn default() -> Self {
        Self {
            content: "You are a helpful TeamSpeak assistant powered by LLM.".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TtsPrompts {
    /// MiMo TTS 风格提示，用于控制语音的语调、语速、情感等
    pub style_prompt: String,
}

impl Default for TtsPrompts {
    fn default() -> Self {
        Self {
            style_prompt: "Natural, friendly tone, moderate pace.".to_string(),
        }
    }
}

impl PromptsConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).context(format!(
            "Prompts config file not found: {}. Please copy examples/config/prompts.toml to config/",
            path.display()
        ))?;
        let config: PromptsConfig =
            toml::from_str(&content).context("Failed to parse prompts config")?;
        Ok(config)
    }
}
