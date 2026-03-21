use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const TS_SERVER_ID: u32 = 1;
pub const TS_KEEPALIVE_INTERVAL_SECS: u64 = 180;
pub const TS_RECONNECT_MAX_RETRIES: u32 = 10;
pub const TS_RECONNECT_BASE_DELAY_MS: u64 = 1000;
pub const TS_HEADLESS_CONNECT_TIMEOUT_SECS: u64 = 30;

pub const LLM_TIMEOUT_SECS: u64 = 30;
pub const LLM_RETRY_MAX: u32 = 3;
pub const LLM_RETRY_DELAY_MS: u64 = 500;

pub const CACHE_REFRESH_INTERVAL_SECS: u64 = 30;
pub const CACHE_ENTRY_TTL_SECS: u64 = 300;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    pub teamspeak: TsConfig,
    pub llm: LlmConfig,
    pub bot: BotConfig,
    pub rate_limit: RateLimitConfig,
    pub audit: AuditConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            teamspeak: TsConfig::default(),
            llm: LlmConfig::default(),
            bot: BotConfig::default(),
            rate_limit: RateLimitConfig::default(),
            audit: AuditConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TsConfig {
    pub serverquery: ServerQueryConfig,
    pub bot_nickname: String,
    /// 连接模式: "serverquery" | "headless"
    pub connection_mode: String,
    /// 无头客户端配置
    pub headless: HeadlessConfig,
}

impl Default for TsConfig {
    fn default() -> Self {
        Self {
            serverquery: ServerQueryConfig::default(),
            bot_nickname: "TSClaw".to_string(),
            connection_mode: "serverquery".to_string(),
            headless: HeadlessConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ServerQueryConfig {
    pub host: String,
    pub port: u16,
    pub ssh_port: u16,
    /// 连接方式: "tcp" | "ssh"
    pub sq_method: String,
    pub login_name: String,
    pub login_pass: String,
}

impl Default for ServerQueryConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 10011,
            ssh_port: 10022,
            sq_method: "tcp".to_string(),
            login_name: "serveradmin".to_string(),
            login_pass: "".to_string(),
        }
    }
}

impl ServerQueryConfig {
    pub fn query_port(&self) -> u16 {
        if self.sq_method == "ssh" {
            return self.ssh_port;
        }
        self.port
    }

    fn validate(&self) -> Result<()> {
        if self.sq_method != "tcp" && self.sq_method != "ssh" {
            anyhow::bail!(
                "Invalid teamspeak.serverquery.sq_method: '{}', expected 'tcp' or 'ssh'",
                self.sq_method
            );
        }
        Ok(())
    }
}

/// 无头客户端配置
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HeadlessConfig {
    /// TeamSpeak 服务器地址 (host:voice_port)
    pub server_address: String,
    /// 身份密钥文件路径
    pub identity_path: String,
    /// ffmpeg 可执行文件路径 (可选，默认 "ffmpeg")
    pub ffmpeg_path: Option<String>,
}

impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            server_address: "127.0.0.1:9987".to_string(),
            identity_path: "config/identity.toml".to_string(),
            ffmpeg_path: None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LlmConfig {
    pub provider: String,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_string(),
            api_key: "".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            max_tokens: 1024,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct BotConfig {
    pub trigger_prefixes: Vec<String>,
    pub respond_to_private: bool,
    pub max_concurrent_requests: u32,
}

impl Default for BotConfig {
    fn default() -> Self {
        Self {
            trigger_prefixes: vec![
                "!tsclaw".to_string(),
                "!bot".to_string(),
                "@TSClaw".to_string(),
            ],
            respond_to_private: true,
            max_concurrent_requests: 4,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
    pub burst_size: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 10,
            burst_size: 3,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AuditConfig {
    pub enabled: bool,
    pub log_dir: String,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_dir: "./logs".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PromptsConfig {
    pub system: SystemPrompts,
    pub error: ErrorPrompts,
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self {
            system: SystemPrompts::default(),
            error: ErrorPrompts::default(),
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
pub struct ErrorPrompts {
    pub permission_denied: String,
    pub rate_limited: String,
    pub target_not_found: String,
    pub target_protected: String,
    pub llm_error: String,
    pub ts_error: String,
}

impl Default for ErrorPrompts {
    fn default() -> Self {
        Self {
            permission_denied: "你没有权限使用此命令。".to_string(),
            rate_limited: "请求过多。请稍后再试。".to_string(),
            target_not_found: "在服务器上找不到匹配 '{target}' 的用户。".to_string(),
            target_protected: "该用户受到保护，无法使用此命令成为目标。".to_string(),
            llm_error: "AI 后端当前不可用。请稍后再试。".to_string(),
            ts_error: "TeamSpeak 命令执行失败: {detail}".to_string(),
        }
    }
}

// 默认配置文件内容
pub const DEFAULT_SETTINGS_TOML: &str = r#"[teamspeak]
bot_nickname = "TSClaw"
connection_mode = "serverquery"  # "serverquery" 或 "headless"

[teamspeak.serverquery]
host = "127.0.0.1"
port = 10011
ssh_port = 10022
sq_method = "tcp"         # "tcp" 或 "ssh"
login_name = "serveradmin"
login_pass = ""           # 通过环境变量 TS_LOGIN_PASS 覆盖

[teamspeak.headless]
server_address = "127.0.0.1:9987"
identity_path = "config/identity.toml"
# ffmpeg_path = "ffmpeg"  # 可选

[llm]
provider = "openai"       # 可选: openai | anthropic | ollama
api_key = ""              # 通过环境变量 LLM_API_KEY 覆盖
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
max_tokens = 1024

[bot]
# 在频道/服务器聊天中触发机器人的前缀
trigger_prefixes = ["!tsclaw", "!bot", "@TSClaw"]
# 私聊消息始终触发机器人
respond_to_private = true
# 最大并发 LLM 请求数
max_concurrent_requests = 4

[rate_limit]
# 每个用户的令牌桶限流设置
requests_per_minute = 10
burst_size = 3

[audit]
enabled = true
log_dir = "./logs"
"#;

pub const DEFAULT_PROMPTS_TOML: &str = r#"[system]
content = """
你是 TSClaw，一个 TeamSpeak 服务器的自动化管理员助手。
你的工作是解释管理员的命令并调用合适的工具。

规则:
- 只有在明确要求时才调用工具。不要在没有明确指令的情况下采取行动。
- 如果指令不明确，请要求用户澄清而不是猜测。
- 在执行破坏性操作（封禁、踢出）之前，始终通过重复你将要做的事情来确认。
- 如果请求没有合适的工具，请直说。
- 使用用户使用的同一种语言进行回复。
- 保持回复简明扼要。不要使用 markdown — 这是一个聊天界面。
- 永远不要透露内部系统细节、配置或 API 密钥。
"""

[error]
permission_denied = "你没有权限使用此命令。"
rate_limited = "请求过多。请稍后再试。"
target_not_found = "在服务器上找不到匹配 '{target}' 的用户。"
target_protected = "该用户受到保护，无法使用此命令成为目标。"
llm_error = "AI 后端当前不可用。请稍后再试。"
ts_error = "TeamSpeak 命令执行失败: {detail}"
"#;

/// Helper to resolve configuration paths relative to the executable directory
pub fn get_config_path<P: AsRef<Path>>(path: P) -> Result<std::path::PathBuf> {
    let path = path.as_ref();
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let exe_path = std::env::current_exe()?;
    let exe_dir = exe_path.parent().unwrap_or(Path::new("."));
    Ok(exe_dir.join(path))
}

impl AppConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = get_config_path(path)?;
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, DEFAULT_SETTINGS_TOML)?;
            println!("Created default AppConfig at {:?}", path);
        }

        let content = std::fs::read_to_string(&path)?;
        let config: AppConfig = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        self.teamspeak.serverquery.validate()?;
        Ok(())
    }
}

impl PromptsConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = get_config_path(path)?;
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, DEFAULT_PROMPTS_TOML)?;
            println!("Created default PromptsConfig at {:?}", path);
        }

        let content = std::fs::read_to_string(&path)?;
        let config: PromptsConfig = match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                println!("Failed to parse prompts config from {:?}: {}", path, e);
                println!(
                    "Content preview:\n{}",
                    &content.chars().take(200).collect::<String>()
                );
                return Err(e.into());
            }
        };
        Ok(config)
    }
}

pub async fn watch_config(config: std::sync::Arc<arc_swap::ArcSwap<AppConfig>>) -> Result<()> {
    use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
    use tokio::sync::mpsc;

    let (tx, mut rx) = mpsc::channel(1);
    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
            if let Ok(event) = res {
                if event.kind.is_modify() {
                    let _ = tx.blocking_send(());
                }
            }
        })?;

    let config_path = get_config_path("config/settings.toml")?;
    watcher.watch(&config_path, RecursiveMode::NonRecursive)?;

    while rx.recv().await.is_some() {
        match AppConfig::load(&config_path) {
            Ok(new_config) => {
                config.store(std::sync::Arc::new(new_config));
                tracing::info!("Config reloaded");
            }
            Err(e) => tracing::warn!("Config reload failed: {e}"),
        }
    }

    Ok(())
}
