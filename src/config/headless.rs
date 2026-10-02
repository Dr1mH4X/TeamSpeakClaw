use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct HeadlessConfig {
    pub server_address: String,
    pub server_port: u16,
    pub server_password: String,
    pub channel_password: String,
    pub channel_id: String,
    pub stt: HeadlessSttConfig,
    pub tts: HeadlessTtsConfig,
    pub wakeword: HeadlessWakewordConfig,
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
}

/// OpenWakeWord 语音唤醒：`enabled` 时从 `models_dir()` 加载前端模型
/// 需要 `stt.enabled` 或 `llm.omni_model` 提供语音输入
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct HeadlessWakewordConfig {
    pub enabled: bool,
    pub model: String,
    pub window_secs: u32,
    /// 窗口内再次命中唤醒词时打断同说话人正在产出的回合（LLM 流 + TTS 播放）
    pub barge_in: bool,
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            server_address: "127.0.0.1".to_string(),
            server_port: 9987,
            server_password: String::new(),
            channel_password: String::new(),
            channel_id: String::new(),
            stt: HeadlessSttConfig::default(),
            tts: HeadlessTtsConfig::default(),
            wakeword: HeadlessWakewordConfig::default(),
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
        }
    }
}

impl Default for HeadlessWakewordConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: String::new(),
            window_secs: 15,
            barge_in: true,
        }
    }
}
