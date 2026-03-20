use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    pub teamspeak: TsConfig,
    pub llm: LlmConfig,
    pub bot: BotConfig,
    pub rate_limit: RateLimitConfig,
    pub audit: AuditConfig,
    pub cache: CacheConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            teamspeak: TsConfig::default(),
            llm: LlmConfig::default(),
            bot: BotConfig::default(),
            rate_limit: RateLimitConfig::default(),
            audit: AuditConfig::default(),
            cache: CacheConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
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
    /// 连接模式: "serverquery" | "headless"
    #[cfg(feature = "headless")]
    pub connection_mode: String,
    /// 无头客户端配置
    #[cfg(feature = "headless")]
    pub headless: HeadlessConfig,
}

impl Default for TsConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 10011,
            ssh_port: 10022,
            use_ssh: false,
            login_name: "serveradmin".to_string(),
            login_pass: "".to_string(),
            server_id: 1,
            bot_nickname: "TSClaw".to_string(),
            keepalive_interval_secs: 180,
            reconnect_max_retries: 10,
            reconnect_base_delay_ms: 1000,
            #[cfg(feature = "headless")]
            connection_mode: "serverquery".to_string(),
            #[cfg(feature = "headless")]
            headless: HeadlessConfig::default(),
        }
    }
}

/// 无头客户端配置
#[cfg(feature = "headless")]
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct HeadlessConfig {
    /// TeamSpeak 服务器地址 (host:voice_port)
    pub server_address: String,
    /// 身份密钥文件路径
    pub identity_path: String,
    /// 连接超时（秒）
    pub connect_timeout_secs: u64,
    /// ffmpeg 可执行文件路径 (可选，默认 "ffmpeg")
    pub ffmpeg_path: Option<String>,
}

#[cfg(feature = "headless")]
impl Default for HeadlessConfig {
    fn default() -> Self {
        Self {
            server_address: "127.0.0.1:9987".to_string(),
            identity_path: "./identity.toml".to_string(),
            connect_timeout_secs: 30,
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
    pub timeout_secs: u64,
    pub retry_max: u32,
    pub retry_delay_ms: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_string(),
            api_key: "".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            max_tokens: 1024,
            timeout_secs: 30,
            retry_max: 3,
            retry_delay_ms: 500,
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
            trigger_prefixes: vec!["!tsclaw".to_string(), "!bot".to_string(), "@TSClaw".to_string()],
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
    pub log_file: String,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_dir: "./logs".to_string(),
            log_file: "audit.jsonl".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CacheConfig {
    pub refresh_interval_secs: u64,
    pub entry_ttl_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            refresh_interval_secs: 30,
            entry_ttl_secs: 300,
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AclConfig {
    pub rules: Vec<AclRule>,
    pub acl: AclSettings,
}

impl Default for AclConfig {
    fn default() -> Self {
        // Note: The programmatic default here is minimal. 
        // The file template (DEFAULT_ACL_TOML) contains the full default configuration.
        Self {
            rules: vec![],
            acl: AclSettings::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AclRule {
    pub name: String,
    pub server_group_ids: Vec<u32>,
    pub allowed_skills: Vec<String>,
    pub can_target_admins: bool,
    pub rate_limit_override: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AclSettings {
    pub protected_group_ids: Vec<u32>,
}

impl Default for AclSettings {
    fn default() -> Self {
        Self {
            protected_group_ids: vec![6, 8, 9],
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

// Default configuration file contents with Chinese comments
pub const DEFAULT_SETTINGS_TOML: &str = r#"[teamspeak]
host = "127.0.0.1"
port = 10011
ssh_port = 10022
use_ssh = false
login_name = "serveradmin"
login_pass = ""           # 通过环境变量 TS_LOGIN_PASS 覆盖
server_id = 1
bot_nickname = "TSClaw"
keepalive_interval_secs = 180
reconnect_max_retries = 10
reconnect_base_delay_ms = 1000
connection_mode = "serverquery"  # "serverquery" 或 "headless"

[teamspeak.headless]
server_address = "127.0.0.1:9987"
identity_path = "./identity.toml"
connect_timeout_secs = 30
# ffmpeg_path = "ffmpeg"  # 可选

[llm]
provider = "openai"       # 可选: openai | anthropic | ollama
api_key = ""              # 通过环境变量 LLM_API_KEY 覆盖
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
max_tokens = 1024
timeout_secs = 30
retry_max = 3
retry_delay_ms = 500

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
log_file = "audit.jsonl"

[cache]
# 客户端列表刷新间隔（秒）
refresh_interval_secs = 30
# 客户端离开后其缓存条目的存活时间（TTL）
entry_ttl_secs = 300
"#;

pub const DEFAULT_ACL_TOML: &str = r#"# 权限规则从上到下评估；第一个匹配的生效。
# server_group_ids: TeamSpeak 服务器组 ID（整数）
# allowed_skills: 技能名称列表，或者 ["*"] 表示全部
# can_target_admins: 此角色是否可以对管理员组成员执行操作
# rate_limit_override: 可选的每角色每分钟请求数（覆盖全局设置）

[[rules]]
name = "superadmin"
server_group_ids = [6]
allowed_skills = ["*"]
can_target_admins = true
rate_limit_override = 60

[[rules]]
name = "default_user"
server_group_ids = [8]
allowed_skills = [
  "poke_client",
  "send_private_msg",
  "send_channel_msg",
  "get_client_info",
  "list_clients",
  "get_server_info",
  "music_control"
]
can_target_admins = false
rate_limit_override = 20

[[rules]]
name = "default"
server_group_ids = []          # 空数组 = 捕获所有剩余情况
allowed_skills = ["music_control"]
can_target_admins = false

# 被视为“受管理员保护”的组 ID（can_target_admins = false 不能对这些组执行操作）
[acl]
protected_group_ids = [6, 8, 9]
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

impl AppConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, DEFAULT_SETTINGS_TOML)?;
            println!("Created default AppConfig at {:?}", path);
        }
        
        let content = std::fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&content)?;
        Ok(config)
    }
}

impl AclConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, DEFAULT_ACL_TOML)?;
            println!("Created default AclConfig at {:?}", path);
        }
        
        let content = std::fs::read_to_string(path)?;
        let config: AclConfig = toml::from_str(&content)?;
        Ok(config)
    }
}

impl PromptsConfig {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
             if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, DEFAULT_PROMPTS_TOML)?;
            println!("Created default PromptsConfig at {:?}", path);
        }

        let content = std::fs::read_to_string(path)?;
        let config: PromptsConfig = match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                println!("Failed to parse prompts config from {:?}: {}", path, e);
                println!("Content preview:\n{}", &content.chars().take(200).collect::<String>());
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
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(
        move |res: std::result::Result<Event, notify::Error>| {
            if let Ok(event) = res {
                if event.kind.is_modify() {
                    let _ = tx.blocking_send(());
                }
            }
        },
    )?;

    watcher.watch(Path::new("config/settings.toml"), RecursiveMode::NonRecursive)?;

    while rx.recv().await.is_some() {
        match AppConfig::load("config/settings.toml") {
            Ok(new_config) => {
                config.store(std::sync::Arc::new(new_config));
                tracing::info!("Config reloaded");
            }
            Err(e) => tracing::warn!("Config reload failed: {e}"),
        }
    }

    Ok(())
}
