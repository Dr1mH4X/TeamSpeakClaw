use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct HeadlessConfig {
    pub enabled: bool,
    pub ts3_host: String,
    pub ts3_port: u16,
    pub server_password: String,
    pub channel_password: String,
    pub channel_path: String,
    pub channel_id: String,
    pub stt: HeadlessSttConfig,
    pub tts: HeadlessTtsConfig,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct HeadlessSttConfig {
    pub enabled: bool,
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub language: String,
    pub wake_words: Vec<String>,
    pub wake_word_required: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct HeadlessTtsConfig {
    pub enabled: bool,
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub voice: String,
    pub always_tts: bool,
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            ts3_host: "127.0.0.1".to_string(),
            ts3_port: 9987,
            server_password: String::new(),
            channel_password: String::new(),
            channel_path: String::new(),
            channel_id: String::new(),
            stt: HeadlessSttConfig::default(),
            tts: HeadlessTtsConfig::default(),
        }
    }
}

impl Default for HeadlessSttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "openai-compatibility".to_string(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            language: "zh".to_string(),
            wake_words: vec!["tsclaw".to_string()],
            wake_word_required: false,
        }
    }
}

impl Default for HeadlessTtsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "openai-compatibility".to_string(),
            base_url: String::new(),
            api_key: String::new(),
            model: "gpt-4o-mini-tts".to_string(),
            voice: "alloy".to_string(),
            always_tts: false,
        }
    }
}
