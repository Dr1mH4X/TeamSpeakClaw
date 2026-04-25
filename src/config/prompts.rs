use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PromptsConfig {
    pub system: SystemPrompts,
    pub error: ErrorPrompts,
    pub tts: TtsPrompts,
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self {
            system: SystemPrompts::default(),
            error: ErrorPrompts::default(),
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

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ErrorPrompts {
    pub permission_denied: String,
    pub llm_error: String,
    pub ts_error: String,
    pub skill_error: String,
    pub skill_not_found: String,
    pub self_target: String,
    pub target_permission: String,
    pub empty_message: String,
    pub missing_parameter: String,
    pub invalid_mode: String,
    pub client_offline: String,
}

impl Default for ErrorPrompts {
    fn default() -> Self {
        Self {
            permission_denied: "你没有权限使用此命令。".to_string(),
            llm_error: "AI 后端当前不可用。请稍后再试。".to_string(),
            ts_error: "TeamSpeak 命令执行失败: {detail}".to_string(),
            skill_error: "技能执行失败: {detail}".to_string(),
            skill_not_found: "未找到指定的技能".to_string(),
            self_target: "不能对自己执行此操作".to_string(),
            target_permission: "无权对该用户执行此操作".to_string(),
            empty_message: "消息内容不能为空".to_string(),
            missing_parameter: "缺少必要参数: {param}".to_string(),
            invalid_mode: "无效的模式，必须是 {allowed}".to_string(),
            client_offline: "客户端 {clid} 不在线或不存在".to_string(),
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
