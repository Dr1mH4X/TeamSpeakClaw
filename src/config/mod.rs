#![allow(dead_code)]

#![allow(dead_code)]

#![allow(dead_code)]

#![allow(dead_code)]

#![allow(dead_code)]

#![allow(dead_code)]

#![allow(dead_code)]

#![allow(dead_code)]

#![allow(dead_code)]

use anyhow::Result;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub teamspeak: TsConfig,
    pub llm: LlmConfig,
    pub bot: BotConfig,
    pub rate_limit: RateLimitConfig,
    pub audit: AuditConfig,
    pub cache: CacheConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TsConfig {
    pub host: String,
    pub port: u16,
    pub ssh_port: u16,
    pub use_ssh: bool,
    pub login_name: String,
    pub login_pass: String,
    pub server_id: u32,
    pub bot_nickname: String,
    pub keepalive_interval_secs: u64,
    pub reconnect_max_retries: u32,
    pub reconnect_base_delay_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LlmConfig {
    pub provider: String,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    pub retry_max: u32,
    pub retry_delay_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BotConfig {
    pub trigger_prefixes: Vec<String>,
    pub respond_to_private: bool,
    pub max_concurrent_requests: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
    pub burst_size: u32,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuditConfig {
    pub enabled: bool,
    pub log_dir: String,
    pub log_file: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CacheConfig {
    pub refresh_interval_secs: u64,
    pub entry_ttl_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AclConfig {
    pub rules: Vec<AclRule>,
    pub acl: AclSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AclRule {
    pub name: String,
    pub server_group_ids: Vec<u32>,
    pub allowed_skills: Vec<String>,
    pub can_target_admins: bool,
    pub rate_limit_override: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AclSettings {
    pub protected_group_ids: Vec<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PromptsConfig {
    pub system: SystemPrompts,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SystemPrompts {
    pub content: String,
}

impl AppConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&content)?;
        Ok(config)
    }
}

impl AclConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: AclConfig = toml::from_str(&content)?;
        Ok(config)
    }
}

impl PromptsConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        // Debug: print content length
        // println!("Loading prompts from {:?}, length: {}", path.as_ref(), content.len());
        let content = std::fs::read_to_string(&path)?;
        let config: PromptsConfig = match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                println!("Failed to parse prompts config from {:?}: {}", path.as_ref(), e);
                println!("Content preview:\n{}", &content.chars().take(200).collect::<String>());
                return Err(e.into());
            }
        };
        Ok(config)
    }
}

pub async fn watch_config(config: std::sync::Arc<arc_swap::ArcSwap<AppConfig>>) -> Result<()> {
    // TODO: Implement file watcher
    let _ = config;
    Ok(())
}
