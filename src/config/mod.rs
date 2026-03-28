use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AppConfig {
    pub teamspeak: TsConfig,
    pub llm: LlmConfig,
    pub bot: BotConfig,
    pub rate_limit: RateLimitConfig,
    pub music_backend: MusicBackendConfig,
    pub logging: LogConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            teamspeak: TsConfig::default(),
            llm: LlmConfig::default(),
            bot: BotConfig::default(),
            rate_limit: RateLimitConfig::default(),
            music_backend: MusicBackendConfig::default(),
            logging: LogConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LogConfig {
    pub file_level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            file_level: "debug".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct TsConfig {
    pub host: String,
    pub port: u16,
    pub ssh_port: u16,
    pub method: String,
    pub login_name: String,
    pub login_pass: String,
    pub server_id: u32,
    pub bot_nickname: String,
}

impl Default for TsConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 10011,
            ssh_port: 10022,
            method: "tcp".to_string(),
            login_name: "serveradmin".to_string(),
            login_pass: "".to_string(),
            server_id: 1,
            bot_nickname: "TSClaw".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct MusicBackendConfig {
    /// 后端选择: "ts3audiobot"（默认）或 "tsbot_backend"
    pub backend: String,
    /// tsbot-backend HTTP API 地址（仅 backend = "tsbot_backend" 时使用）
    pub base_url: String,
}

impl Default for MusicBackendConfig {
    fn default() -> Self {
        Self {
            backend: "ts3audiobot".to_string(),
            base_url: "http://127.0.0.1:8000".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
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
    pub default_reply_mode: String,
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
            default_reply_mode: "private".to_string(),
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
pub struct AclConfig {
    pub rules: Vec<AclRule>,
    pub acl: AclSettings,
}

impl Default for AclConfig {
    fn default() -> Self {
        // 文件模板 (DEFAULT_ACL_TOML) 包含完整的默认配置。
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
    pub channel_group_ids: Vec<u32>,
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
    pub llm_error: String,
    pub ts_error: String,
}

impl Default for ErrorPrompts {
    fn default() -> Self {
        Self {
            permission_denied: "你没有权限使用此命令。".to_string(),
            llm_error: "AI 后端当前不可用。请稍后再试。".to_string(),
            ts_error: "TeamSpeak 命令执行失败: {detail}".to_string(),
        }
    }
}

// 包含中文注释的默认配置文件内容
pub const DEFAULT_SETTINGS_TOML: &str = r#"
[teamspeak]
host = "127.0.0.1"
port = 10011
ssh_port = 10022
method = "tcp"            # 连接方式: tcp 或 ssh
login_name = "serveradmin"
login_pass = ""           # 通过环境变量 TS_LOGIN_PASS 覆盖
server_id = 1
bot_nickname = "TSClaw"

[music_backend]
backend = "ts3audiobot"  # 音乐后端选择: "ts3audiobot"（通过 TS 私信控制）或 "tsbot_backend"（NeteaseTSBot）
base_url = "http://127.0.0.1:8000"   # 仅 backend = "tsbot_backend" 时生效

[llm]
api_key = ""              # 通过环境变量 LLM_API_KEY 覆盖
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
max_tokens = 1024

[bot]
trigger_prefixes = ["!tsclaw", "!bot", "@TSClaw"]       # 在频道/服务器聊天中触发机器人的前缀
respond_to_private = true       # 私聊消息始终触发机器人
max_concurrent_requests = 4     # 最大并发 LLM 请求数
default_reply_mode = "private"  # 默认回复模式: "private"(私聊) | "channel"(频道) | "server"(服务器广播)，仅频道/广播触发时生效

[rate_limit]
requests_per_minute = 10        # 每个用户的令牌桶限流设置
burst_size = 3
"#;

pub const DEFAULT_ACL_TOML: &str = r#"# 权限规则从上到下评估；第一个匹配的生效。
# server_group_ids: TeamSpeak 服务器组 ID（整数）
# channel_group_ids: TeamSpeak 频道组 ID（整数），空数组表示不检查频道组
# allowed_skills: 技能名称列表，或者 ["*"] 表示全部
# can_target_admins: 此角色是否可以对管理员组成员执行操作
# rate_limit_override: 可选的每角色每分钟请求数（覆盖全局设置）
#
# 规则匹配逻辑：server_group_ids 和 channel_group_ids 只要有一个匹配即视为匹配
# 如果两者都为空数组，则该规则匹配所有用户

[[rules]]
name = "superadmin"
server_group_ids = [6]
channel_group_ids = []
allowed_skills = ["*"]
can_target_admins = true
rate_limit_override = 60

[[rules]]
name = "channel_admin"
server_group_ids = []
channel_group_ids = [5]
allowed_skills = [
  "poke_client",
  "send_message",
  "get_client_info",
  "get_client_list",
  "get_server_info",
  "music_control",
  "kick_client"
]
can_target_admins = false
rate_limit_override = 20

[[rules]]
name = "default_user"
server_group_ids = [8]
channel_group_ids = []
allowed_skills = [
  "poke_client",
  "send_message",
  "get_client_info",
  "get_client_list",
  "get_server_info",
  "music_control"
]
can_target_admins = false
rate_limit_override = 20

[[rules]]
name = "default"
server_group_ids = []
channel_group_ids = []
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
