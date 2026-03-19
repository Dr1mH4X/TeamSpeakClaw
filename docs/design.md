方案：

**权限系统**

设计分两层：

第一层在 `permission_gate` 里：用户发指令时，先查询他的 TS Server Group，对照 `config/acl.toml` 里的规则决定是否允许进入 LLM 流程。第二层在 `llm_engine` 里：构建发给 LLM 的工具列表时，只暴露该用户有权限调用的 skill schema，LLM 根本"看不见"他无权使用的工具，从根本上杜绝越权。

**Rust 的正确打开方式**

- `TsCommand` 是一个 trait，每个命令都实现它，编译期就能验证参数类型
- Skill 注册表用 `Box<dyn Skill>` trait object，新增技能零改动核心代码
- `DashMap` 替代文件缓存，并发安全、零锁竞争
- `governor` crate 做 token bucket 限流，防止用户刷指令

**目录结构（Rust 风格）**

```
teamspeakclaw/
├── Cargo.toml
├── config/
│   ├── settings.toml        # 主配置（TS连接、LLM API Key）
│   ├── acl.toml             # 权限控制表 ← 新增
│   └── prompts.toml         # 系统提示词
├── src/
│   ├── main.rs              # 启动：读config，初始化各层，tokio::main
│   ├── adapter/
│   │   ├── mod.rs
│   │   ├── connection.rs    # Tokio TCP/SSH连接，keepalive心跳，断线重连backoff
│   │   ├── command.rs       # TsCommand trait + 所有原语命令实现
│   │   └── event.rs         # 原始行解析 → TsEvent 枚举（强类型）
│   ├── permission/
│   │   ├── mod.rs
│   │   ├── acl.rs           # ACL规则结构体，从TOML加载
│   │   └── gate.rs          # 查询caller的server group → 判断allow/deny
│   ├── llm/
│   │   ├── mod.rs
│   │   ├── provider.rs      # LlmProvider trait（OpenAI/Anthropic/Ollama 实现）
│   │   ├── engine.rs        # 构造payload，过滤tool schema，解析tool_call响应
│   │   └── schema.rs        # 从Skill trait自动生成JSON Schema
│   ├── skills/
│   │   ├── mod.rs           # Skill trait定义 + 注册表
│   │   ├── communication.rs # poke_client, send_private_msg, send_channel_msg
│   │   ├── moderation.rs    # ban_client, kick_client, move_client
│   │   └── information.rs   # get_client_info, get_server_info, list_clients
│   ├── cache/
│   │   └── client_cache.rs  # DashMap<String, ClientInfo>，后台定时刷新task
│   ├── audit/
│   │   └── log.rs           # 结构化JSON-lines审计日志
│   └── router.rs            # 事件路由：broadcast channel分发给各处理器
└── logs/
    └── audit.json           # 审计日志文件
```

**ACL 配置示例（`config/acl.toml`）**

```toml
# 权限按 TS Server Group ID 分配
# 越靠前优先级越高

[[rules]]
name = "superadmin"
server_group_ids = [6]          # SA 组
allowed_skills = ["*"]          # 所有技能
can_target_admins = true        # 可对其他管理员执行操作

[[rules]]
name = "admin"
server_group_ids = [8, 9]       # 管理员组
allowed_skills = [
  "poke_client",
  "send_private_msg",
  "ban_client",
  "kick_client",
  "move_client",
  "get_client_info",
  "list_clients",
]
can_target_admins = false       # 不能封禁其他管理员

[[rules]]
name = "vip"
server_group_ids = [10]
allowed_skills = [
  "poke_client",
  "get_server_info",
  "list_clients",
]

[[rules]]
name = "default"               # 无匹配时的兜底
server_group_ids = []
allowed_skills = []            # 普通用户无权使用任何技能
```

**Skill Trait 设计**

```rust
// src/skills/mod.rs
#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;  // 这段直接进 LLM 的 tool schema
    fn parameters_schema(&self) -> serde_json::Value;  // JSON Schema
    
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,  // 包含 caller 信息、adapter 引用
    ) -> Result<SkillResult, SkillError>;
}

pub struct ExecutionContext {
    pub caller_cldbid: u32,
    pub caller_name: String,
    pub allowed_skills: Vec<String>,  // 从 permission_gate 传入
    pub adapter: Arc<TsAdapter>,
    pub cache: Arc<ClientCache>,
    pub audit: Arc<AuditLog>,
}
```

**审计日志格式**

每次 skill 执行都写一行 JSON：

```json
{"ts":"2025-03-19T10:23:11Z","caller_cldbid":42,"caller_name":"Admin_Tom","skill":"ban_client","args":{"target_name":"Spammer123","duration_secs":3600,"reason":"扰民"},"result":"ok","target_cldbid":87,"server_id":1}
```

---

**依赖清单（`Cargo.toml` 关键 crates）**

| crate | 用途 |
|---|---|
| `tokio` | 异步运行时 |
| `reqwest` | HTTP client（LLM API调用） |
| `dashmap` | 并发安全 HashMap（client cache） |
| `governor` | token bucket 限流 |
| `serde` / `serde_json` | JSON序列化 |
| `toml` | 配置文件解析 |
| `notify` | 文件变更监听（热重载config） |
| `tracing` | 结构化日志 |
| `async-trait` | trait 里的 async fn |
| `thiserror` | 自定义错误类型 |

---

以下为输出完整的项目方案

---

## 1. 项目根文件

**`Cargo.toml`**
```toml
[package]
name = "teamspeakclaw"
version = "0.0.0"
edition = "2021"
authors = ["Your Name <you@example.com>"]
description = "LLM-powered TeamSpeak ServerQuery bot"
license = "MIT"
repository = "https://github.com/yourorg/teamspeakclaw"

[[bin]]
name = "teamspeakclaw"
path = "src/main.rs"

[dependencies]
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
async-trait = "0.1"
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
dashmap = "6"
governor = "0.6"
nonzero_ext = "0.3"
notify = { version = "6", features = ["macos_kqueue"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
tracing-appender = "0.2"
thiserror = "1"
anyhow = "1"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4"] }
regex = "1"
once_cell = "1"
arc-swap = "1"
futures = "0.3"
bytes = "1"

[dev-dependencies]
tokio-test = "0.4"
mockall = "0.12"
wiremock = "0.6"

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true
```

**`.env.example`**
```dotenv
# TeamSpeak ServerQuery
TS_HOST=127.0.0.1
TS_PORT=10011
TS_SSH_PORT=10022
TS_USE_SSH=false
TS_LOGIN_NAME=serveradmin
TS_LOGIN_PASS=your_serverquery_password
TS_SERVER_ID=1
TS_BOT_NICKNAME=TSClaw

# LLM
LLM_PROVIDER=openai           # openai | anthropic | ollama
LLM_API_KEY=sk-...
LLM_BASE_URL=https://api.openai.com/v1
LLM_MODEL=gpt-4o
LLM_TIMEOUT_SECS=30

# Logging
RUST_LOG=info
LOG_DIR=./logs
```

**`.gitignore`**
```gitignore
/target
.env
*.env.local
logs/*.jsonl
logs/*.log
config/secrets.toml
```

---

## 2. 配置文件

**`config/settings.toml`**
```toml
[teamspeak]
host = "127.0.0.1"
port = 10011
ssh_port = 10022
use_ssh = false
login_name = "serveradmin"
login_pass = ""           # override via env TS_LOGIN_PASS
server_id = 1
bot_nickname = "TSClaw"
keepalive_interval_secs = 180
reconnect_max_retries = 10
reconnect_base_delay_ms = 1000

[llm]
provider = "openai"       # openai | anthropic | ollama
api_key = ""              # override via env LLM_API_KEY
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
max_tokens = 1024
timeout_secs = 30
retry_max = 3
retry_delay_ms = 500

[bot]
# Prefixes that trigger the bot in channel/server chat
trigger_prefixes = ["!tsclaw", "!bot", "@TSClaw"]
# Private messages always trigger the bot
respond_to_private = true
# Max concurrent LLM requests
max_concurrent_requests = 4

[rate_limit]
# Per-user token bucket
requests_per_minute = 10
burst_size = 3

[audit]
enabled = true
log_dir = "./logs"
log_file = "audit.jsonl"

[cache]
# Client list refresh interval
refresh_interval_secs = 30
# TTL for individual entries after they leave
entry_ttl_secs = 300
```

**`config/acl.toml`**
```toml
# Permission rules evaluated top-to-bottom; first match wins.
# server_group_ids: TeamSpeak server group IDs (integer)
# allowed_skills: list of skill names, or ["*"] for all
# can_target_admins: whether this role can perform actions ON admin-group members
# rate_limit_override: optional per-role requests_per_minute (overrides global)

[[rules]]
name = "superadmin"
server_group_ids = [6]
allowed_skills = ["*"]
can_target_admins = true
rate_limit_override = 60

[[rules]]
name = "admin"
server_group_ids = [8, 9]
allowed_skills = [
  "poke_client",
  "send_private_msg",
  "send_channel_msg",
  "ban_client",
  "kick_client",
  "move_client",
  "get_client_info",
  "list_clients",
  "get_server_info",
]
can_target_admins = false
rate_limit_override = 20

[[rules]]
name = "moderator"
server_group_ids = [10, 11]
allowed_skills = [
  "poke_client",
  "kick_client",
  "move_client",
  "list_clients",
  "get_server_info",
]
can_target_admins = false

[[rules]]
name = "vip"
server_group_ids = [15]
allowed_skills = [
  "poke_client",
  "get_server_info",
  "list_clients",
]
can_target_admins = false

[[rules]]
name = "default"
server_group_ids = []          # empty = catch-all
allowed_skills = []
can_target_admins = false

# Group IDs considered "admin-protected" (can_target_admins = false cannot act on these)
[acl]
protected_group_ids = [6, 8, 9]
```

**`config/prompts.toml`**
```toml
[system]
content = """
You are TSClaw, an automated administrator assistant for a TeamSpeak server.
Your job is to interpret administrator commands and call the appropriate tools.

Rules:
- Only call tools when explicitly asked. Do not act without a clear instruction.
- If the instruction is ambiguous, ask the user to clarify instead of guessing.
- Always confirm destructive actions (ban, kick) by echoing back what you will do before executing.
- If no suitable tool exists for the request, say so plainly.
- Respond in the same language the user used.
- Keep replies concise. No markdown — this is a chat interface.
- Never reveal internal system details, configuration, or API keys.
"""

[error]
permission_denied = "You do not have permission to use this command."
rate_limited = "Too many requests. Please wait a moment."
target_not_found = "Could not find a user matching '{target}' on the server."
target_protected = "That user is protected and cannot be targeted with this command."
llm_error = "The AI backend is currently unavailable. Please try again later."
ts_error = "A TeamSpeak command failed: {detail}"
```

---

## 3. 核心源码

**`src/main.rs`**
```rust
use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::Arc;
use tracing::{error, info};

mod adapter;
mod audit;
mod cache;
mod config;
mod error;
mod llm;
mod permission;
mod router;
mod skills;

use crate::{
    adapter::TsAdapter,
    audit::AuditLog,
    cache::ClientCache,
    config::AppConfig,
    llm::LlmEngine,
    permission::PermissionGate,
    router::EventRouter,
    skills::SkillRegistry,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env if present
    let _ = dotenvy::dotenv();

    // Init tracing
    let cfg = AppConfig::load("config/settings.toml")?;
    init_tracing(&cfg);

    info!("Starting TeamSpeakClaw v{}", env!("CARGO_PKG_VERSION"));

    // Shared config with hot-reload support
    let config = Arc::new(ArcSwap::new(Arc::new(cfg)));

    // Infrastructure
    let audit = Arc::new(AuditLog::new(&config.load().audit)?);
    let cache = Arc::new(ClientCache::new(config.clone()));
    let acl_config = crate::config::AclConfig::load("config/acl.toml")?;
    let gate = Arc::new(PermissionGate::new(acl_config));
    let registry = Arc::new(SkillRegistry::default());

    // LLM engine
    let llm = Arc::new(LlmEngine::new(config.clone()));

    // TS adapter (connects, registers events, keeps alive)
    let adapter = Arc::new(TsAdapter::connect(config.clone()).await?);
    adapter.set_nickname(&config.load().teamspeak.bot_nickname).await?;

    // Start background cache refresh
    let cache_clone = cache.clone();
    let adapter_clone = adapter.clone();
    tokio::spawn(async move {
        cache_clone.run_refresh_loop(adapter_clone).await;
    });

    // Start config hot-reload watcher
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::config::watch_config(config_clone).await {
            error!("Config watcher error: {e}");
        }
    });

    // Main event loop
    let router = EventRouter::new(config, adapter, cache, gate, llm, registry, audit);
    info!("Bot ready. Listening for events.");
    router.run().await?;

    Ok(())
}

fn init_tracing(cfg: &AppConfig) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .compact()
        .init();
}
```

**`src/error.rs`**
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("TeamSpeak error {code}: {message}")]
    TsError { code: u32, message: String },

    #[error("Permission denied: {reason}")]
    PermissionDenied { reason: String },

    #[error("Rate limited")]
    RateLimited,

    #[error("Target not found: {name}")]
    TargetNotFound { name: String },

    #[error("Target is protected")]
    TargetProtected,

    #[error("LLM backend error: {0}")]
    LlmError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Skill error: {0}")]
    SkillError(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, AppError>;
```

**`src/config.rs`**
```rust
use anyhow::Result;
use arc_swap::ArcSwap;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::{path::Path, sync::Arc, time::Duration};
use tracing::{info, warn};

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub teamspeak: TsConfig,
    pub llm: LlmConfig,
    pub bot: BotConfig,
    pub rate_limit: RateLimitConfig,
    pub audit: AuditConfig,
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    pub trigger_prefixes: Vec<String>,
    pub respond_to_private: bool,
    pub max_concurrent_requests: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
    pub burst_size: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuditConfig {
    pub enabled: bool,
    pub log_dir: String,
    pub log_file: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    pub refresh_interval_secs: u64,
    pub entry_ttl_secs: u64,
}

impl AppConfig {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let mut cfg: AppConfig = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("Config parse error: {e}"))?;
        // Override from env
        if let Ok(v) = std::env::var("TS_LOGIN_PASS") { cfg.teamspeak.login_pass = v; }
        if let Ok(v) = std::env::var("LLM_API_KEY") { cfg.llm.api_key = v; }
        if let Ok(v) = std::env::var("LLM_MODEL") { cfg.llm.model = v; }
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AclConfig {
    pub rules: Vec<AclRule>,
    pub acl: AclGlobal,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AclRule {
    pub name: String,
    pub server_group_ids: Vec<u32>,
    pub allowed_skills: Vec<String>,
    pub can_target_admins: bool,
    pub rate_limit_override: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AclGlobal {
    pub protected_group_ids: Vec<u32>,
}

impl AclConfig {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        toml::from_str(&raw).map_err(|e| anyhow::anyhow!("ACL parse error: {e}"))
    }
}

pub async fn watch_config(config: Arc<ArcSwap<AppConfig>>) -> Result<()> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| {
            if res.is_ok() { let _ = tx.blocking_send(()); }
        },
        notify::Config::default().with_poll_interval(Duration::from_secs(5)),
    )?;
    watcher.watch(Path::new("config"), RecursiveMode::Recursive)?;

    while rx.recv().await.is_some() {
        tokio::time::sleep(Duration::from_millis(200)).await; // debounce
        match AppConfig::load("config/settings.toml") {
            Ok(new_cfg) => {
                config.store(Arc::new(new_cfg));
                info!("Config hot-reloaded");
            }
            Err(e) => warn!("Config reload failed, keeping old: {e}"),
        }
    }
    Ok(())
}
```

**`src/adapter/mod.rs`**
```rust
pub mod command;
pub mod connection;
pub mod event;

pub use connection::TsAdapter;
pub use event::{TsEvent, TextMessageTarget};
```

**`src/adapter/event.rs`**
```rust
use serde::Deserialize;

#[derive(Debug, Clone)]
pub enum TsEvent {
    TextMessage(TextMessageEvent),
    ClientEnterView(ClientEnterEvent),
    ClientLeftView(ClientLeftEvent),
    Unknown,
}

#[derive(Debug, Clone)]
pub struct TextMessageEvent {
    pub target_mode: TextMessageTarget,
    pub invoker_name: String,
    pub invoker_uid: String,
    pub invoker_id: u32,      // clid (session)
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TextMessageTarget {
    Private,   // targetmode=1
    Channel,   // targetmode=2
    Server,    // targetmode=3
}

#[derive(Debug, Clone)]
pub struct ClientEnterEvent {
    pub clid: u32,
    pub cldbid: u32,
    pub client_nickname: String,
    pub client_server_groups: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct ClientLeftEvent {
    pub clid: u32,
}

/// Parse a raw ServerQuery notification line into a TsEvent.
pub fn parse_event(line: &str) -> TsEvent {
    if line.starts_with("notifytextmessage") {
        parse_text_message(line)
    } else if line.starts_with("notifycliententerview") {
        parse_client_enter(line)
    } else if line.starts_with("notifyclientleftview") {
        parse_client_left(line)
    } else {
        TsEvent::Unknown
    }
}

fn kv(line: &str, key: &str) -> Option<String> {
    line.split_whitespace()
        .find(|s| s.starts_with(&format!("{key}=")))
        .map(|s| {
            let v = &s[key.len() + 1..];
            ts_unescape(v)
        })
}

fn ts_unescape(s: &str) -> String {
    s.replace("\\s", " ")
        .replace("\\p", "|")
        .replace("\\n", "\n")
        .replace("\\\\", "\\")
        .replace("\\/", "/")
}

fn parse_text_message(line: &str) -> TsEvent {
    let target_mode = match kv(line, "targetmode").as_deref() {
        Some("1") => TextMessageTarget::Private,
        Some("2") => TextMessageTarget::Channel,
        Some("3") => TextMessageTarget::Server,
        _ => return TsEvent::Unknown,
    };
    let invoker_name = kv(line, "invokername").unwrap_or_default();
    let invoker_uid = kv(line, "invokeruid").unwrap_or_default();
    let invoker_id = kv(line, "invokerid")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let message = kv(line, "msg").unwrap_or_default();

    TsEvent::TextMessage(TextMessageEvent {
        target_mode,
        invoker_name,
        invoker_uid,
        invoker_id,
        message,
    })
}

fn parse_client_enter(line: &str) -> TsEvent {
    let clid = kv(line, "clid").and_then(|v| v.parse().ok()).unwrap_or(0);
    let cldbid = kv(line, "client_database_id").and_then(|v| v.parse().ok()).unwrap_or(0);
    let client_nickname = kv(line, "client_nickname").unwrap_or_default();
    let groups = kv(line, "client_servergroups")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.parse().ok())
        .collect();

    TsEvent::ClientEnterView(ClientEnterEvent { clid, cldbid, client_nickname, client_server_groups: groups })
}

fn parse_client_left(line: &str) -> TsEvent {
    let clid = kv(line, "clid").and_then(|v| v.parse().ok()).unwrap_or(0);
    TsEvent::ClientLeftView(ClientLeftEvent { clid })
}
```

**`src/adapter/command.rs`**
```rust
use crate::error::{AppError, Result};

/// All ServerQuery response errors have an id= field.
pub fn check_ts_error(response: &str) -> Result<()> {
    let id: u32 = response
        .split_whitespace()
        .find(|s| s.starts_with("id="))
        .and_then(|s| s[3..].parse().ok())
        .unwrap_or(0);
    if id == 0 {
        return Ok(());
    }
    let msg = response
        .split_whitespace()
        .find(|s| s.starts_with("msg="))
        .map(|s| ts_unescape(&s[4..]))
        .unwrap_or_else(|| "unknown error".into());
    Err(AppError::TsError { code: id, message: msg })
}

fn ts_unescape(s: &str) -> String {
    s.replace("\\s", " ")
        .replace("\\p", "|")
        .replace("\\\\", "\\")
}

pub fn ts_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace(' ', "\\s")
        .replace('|', "\\p")
        .replace('\n', "\\n")
        .replace('\r', "")
        .replace('/', "\\/")
}

/// High-level command builders — return the raw query string to send.
pub fn cmd_login(name: &str, pass: &str) -> String {
    format!("login {} {}", ts_escape(name), ts_escape(pass))
}
pub fn cmd_use(server_id: u32) -> String { format!("use {server_id}") }
pub fn cmd_whoami() -> String { "whoami".into() }
pub fn cmd_version() -> String { "version".into() }
pub fn cmd_clientupdate_nick(nick: &str) -> String {
    format!("clientupdate client_nickname={}", ts_escape(nick))
}
pub fn cmd_register_event(event: &str) -> String {
    format!("servernotifyregister event={event}")
}
pub fn cmd_clientlist() -> String {
    "clientlist -groups".into()
}
pub fn cmd_clientfind(pattern: &str) -> String {
    format!("clientfind pattern={}", ts_escape(pattern))
}
pub fn cmd_clientinfo(clid: u32) -> String {
    format!("clientinfo clid={clid}")
}
pub fn cmd_clientdbinfo(cldbid: u32) -> String {
    format!("clientdbinfo cldbid={cldbid}")
}
pub fn cmd_poke(clid: u32, msg: &str) -> String {
    format!("clientpoke clid={clid} msg={}", ts_escape(msg))
}
pub fn cmd_send_text(target_mode: u8, target: u32, msg: &str) -> String {
    format!(
        "sendtextmessage targetmode={target_mode} target={target} msg={}",
        ts_escape(msg)
    )
}
pub fn cmd_kick(clid: u32, reason: &str) -> String {
    format!("clientkick clid={clid} reasonid=5 reasonmsg={}", ts_escape(reason))
}
pub fn cmd_ban(clid: u32, time_secs: u64, reason: &str) -> String {
    format!(
        "banclient clid={clid} time={time_secs} banreason={}",
        ts_escape(reason)
    )
}
pub fn cmd_move(clid: u32, channel_id: u32) -> String {
    format!("clientmove clid={clid} cid={channel_id}")
}
pub fn cmd_serverinfo() -> String { "serverinfo".into() }
pub fn cmd_channellist() -> String { "channellist".into() }
```

**`src/adapter/connection.rs`**
```rust
use crate::{
    adapter::{
        command::{check_ts_error, cmd_login, cmd_register_event, cmd_use, cmd_version, cmd_clientupdate_nick},
        event::{parse_event, TsEvent},
    },
    config::{AppConfig, TsConfig},
    error::{AppError, Result},
};
use arc_swap::ArcSwap;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::{broadcast, Mutex},
    time::sleep,
};
use tracing::{debug, error, info, warn};

pub struct TsAdapter {
    writer: Mutex<tokio::io::WriteHalf<TcpStream>>,
    event_tx: broadcast::Sender<TsEvent>,
    config: Arc<ArcSwap<AppConfig>>,
}

impl TsAdapter {
    pub async fn connect(config: Arc<ArcSwap<AppConfig>>) -> Result<Arc<Self>> {
        let cfg = config.load();
        let addr = format!("{}:{}", cfg.teamspeak.host, cfg.teamspeak.port);
        info!("Connecting to TeamSpeak ServerQuery at {addr}");

        let stream = Self::connect_with_retry(&cfg.teamspeak).await?;
        let (reader, writer) = tokio::io::split(stream);
        let (tx, _) = broadcast::channel::<TsEvent>(256);

        let adapter = Arc::new(Self {
            writer: Mutex::new(writer),
            event_tx: tx,
            config,
        });

        // Init: login, use, register events
        adapter.init(&cfg.teamspeak).await?;

        // Spawn reader task
        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.reader_loop(BufReader::new(reader)).await;
        });

        // Spawn keepalive task
        let adapter_clone = adapter.clone();
        tokio::spawn(async move {
            adapter_clone.keepalive_loop().await;
        });

        Ok(adapter)
    }

    async fn connect_with_retry(cfg: &TsConfig) -> Result<TcpStream> {
        let addr = format!("{}:{}", cfg.host, cfg.port);
        let mut delay = Duration::from_millis(cfg.reconnect_base_delay_ms);
        for attempt in 0..cfg.reconnect_max_retries {
            match TcpStream::connect(&addr).await {
                Ok(s) => {
                    // Skip TS welcome banner (2 lines)
                    let _ = s.readable().await;
                    return Ok(s);
                }
                Err(e) => {
                    warn!("Connect attempt {attempt} failed: {e}. Retrying in {delay:?}");
                    sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(60));
                }
            }
        }
        Err(AppError::TsError {
            code: 999,
            message: "Max reconnect attempts reached".into(),
        })
    }

    async fn init(&self, cfg: &TsConfig) -> Result<()> {
        // Read banner
        sleep(Duration::from_millis(300)).await;
        self.send_raw(&cmd_login(&cfg.login_name, &cfg.login_pass)).await?;
        self.send_raw(&cmd_use(cfg.server_id)).await?;
        self.send_raw(&cmd_register_event("textprivate")).await?;
        self.send_raw(&cmd_register_event("textchannel")).await?;
        self.send_raw(&cmd_register_event("textserver")).await?;
        self.send_raw(&cmd_register_event("server")).await?;
        info!("ServerQuery session initialized");
        Ok(())
    }

    pub async fn set_nickname(&self, nick: &str) -> Result<()> {
        self.send_raw(&cmd_clientupdate_nick(nick)).await
    }

    pub async fn send_raw(&self, cmd: &str) -> Result<()> {
        debug!(">> {cmd}");
        let mut w = self.writer.lock().await;
        w.write_all(format!("{cmd}\n").as_bytes()).await?;
        w.flush().await?;
        // Simple synchronous response read would need a channel here;
        // for a complete implementation use a request-response queue.
        // This is simplified for clarity.
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TsEvent> {
        self.event_tx.subscribe()
    }

    async fn reader_loop(&self, mut reader: BufReader<tokio::io::ReadHalf<TcpStream>>) {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    error!("ServerQuery connection closed by remote");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    debug!("<< {trimmed}");
                    let event = parse_event(trimmed);
                    if !matches!(event, crate::adapter::event::TsEvent::Unknown) {
                        let _ = self.event_tx.send(event);
                    }
                }
                Err(e) => {
                    error!("Read error: {e}");
                    break;
                }
            }
        }
    }

    async fn keepalive_loop(&self) {
        let interval = self.config.load().teamspeak.keepalive_interval_secs;
        loop {
            sleep(Duration::from_secs(interval)).await;
            debug!("Sending keepalive");
            if let Err(e) = self.send_raw(&cmd_version()).await {
                error!("Keepalive failed: {e}");
            }
        }
    }
}
```

**`src/cache/mod.rs`**
```rust
use crate::{adapter::TsAdapter, config::AppConfig};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::{Duration, Instant}};
use tokio::time::sleep;
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub clid: u32,
    pub cldbid: u32,
    pub nickname: String,
    pub server_groups: Vec<u32>,
    pub updated_at: std::time::SystemTime,
}

pub struct ClientCache {
    map: DashMap<String, ClientInfo>,    // nickname (lowercase) → info
    clid_map: DashMap<u32, String>,      // clid → nickname (for leave events)
    config: Arc<ArcSwap<AppConfig>>,
}

impl ClientCache {
    pub fn new(config: Arc<ArcSwap<AppConfig>>) -> Self {
        Self {
            map: DashMap::new(),
            clid_map: DashMap::new(),
            config,
        }
    }

    pub fn get_by_name(&self, name: &str) -> Option<ClientInfo> {
        self.map.get(&name.to_lowercase()).map(|r| r.clone())
    }

    pub fn get_by_clid(&self, clid: u32) -> Option<ClientInfo> {
        let nick = self.clid_map.get(&clid)?.clone();
        self.map.get(&nick).map(|r| r.clone())
    }

    pub fn upsert(&self, info: ClientInfo) {
        let key = info.nickname.to_lowercase();
        self.clid_map.insert(info.clid, key.clone());
        self.map.insert(key, info);
    }

    pub fn remove_clid(&self, clid: u32) {
        if let Some((_, nick)) = self.clid_map.remove(&clid) {
            self.map.remove(&nick);
        }
    }

    pub fn all_clients(&self) -> Vec<ClientInfo> {
        self.map.iter().map(|r| r.clone()).collect()
    }

    pub async fn run_refresh_loop(&self, adapter: Arc<TsAdapter>) {
        let interval = self.config.load().cache.refresh_interval_secs;
        loop {
            sleep(Duration::from_secs(interval)).await;
            if let Err(e) = self.refresh(&adapter).await {
                warn!("Cache refresh failed: {e}");
            }
        }
    }

    async fn refresh(&self, adapter: &TsAdapter) -> crate::error::Result<()> {
        // In a real impl: send clientlist -groups, parse response, upsert all
        // Simplified here — full parsing is in adapter/command.rs
        debug!("Cache refresh triggered");
        Ok(())
    }
}
```

**`src/permission/mod.rs`**
```rust
pub mod acl;
pub mod gate;

pub use gate::PermissionGate;
```

**`src/permission/gate.rs`**
```rust
use crate::{
    cache::ClientInfo,
    config::{AclConfig, AclRule},
    error::{AppError, Result},
};
use tracing::debug;

pub struct PermissionGate {
    config: AclConfig,
}

impl PermissionGate {
    pub fn new(config: AclConfig) -> Self {
        Self { config }
    }

    /// Resolve which ACL rule applies to this caller.
    pub fn resolve_rule<'a>(&'a self, caller: &ClientInfo) -> &'a AclRule {
        for rule in &self.config.rules {
            if rule.server_group_ids.is_empty() {
                // catch-all
                return rule;
            }
            if rule.server_group_ids.iter().any(|g| caller.server_groups.contains(g)) {
                return rule;
            }
        }
        // Absolute fallback (should always have a default rule)
        self.config.rules.last().unwrap()
    }

    /// Check if caller can invoke a specific skill.
    pub fn can_invoke(&self, caller: &ClientInfo, skill_name: &str) -> Result<()> {
        let rule = self.resolve_rule(caller);
        let allowed = rule.allowed_skills.contains(&"*".to_string())
            || rule.allowed_skills.iter().any(|s| s == skill_name);
        if allowed {
            debug!(
                "Permission ALLOW: {} ({}) → {skill_name}",
                caller.nickname, rule.name
            );
            Ok(())
        } else {
            Err(AppError::PermissionDenied {
                reason: format!(
                    "Role '{}' cannot invoke skill '{skill_name}'",
                    rule.name
                ),
            })
        }
    }

    /// Check if caller can act on a target (respects can_target_admins).
    pub fn can_target(&self, caller: &ClientInfo, target: &ClientInfo) -> Result<()> {
        let rule = self.resolve_rule(caller);
        if rule.can_target_admins {
            return Ok(());
        }
        let target_is_protected = target
            .server_groups
            .iter()
            .any(|g| self.config.acl.protected_group_ids.contains(g));
        if target_is_protected {
            Err(AppError::TargetProtected)
        } else {
            Ok(())
        }
    }

    /// Return the subset of skill names this caller is allowed to see.
    pub fn allowed_skills(&self, caller: &ClientInfo) -> Vec<String> {
        let rule = self.resolve_rule(caller);
        if rule.allowed_skills.contains(&"*".to_string()) {
            // Return all registered skill names (caller of this fn will intersect with registry)
            vec!["*".to_string()]
        } else {
            rule.allowed_skills.clone()
        }
    }
}
```

**`src/skills/mod.rs`**
```rust
use async_trait::async_trait;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

use crate::{
    adapter::TsAdapter,
    audit::AuditLog,
    cache::ClientCache,
    cache::ClientInfo,
    error::Result,
};

pub mod communication;
pub mod information;
pub mod moderation;

#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub caller: ClientInfo,
    pub allowed_skills: Vec<String>, // already resolved by permission gate
    pub adapter: Arc<TsAdapter>,
    pub cache: Arc<ClientCache>,
    pub audit: Arc<AuditLog>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillResult {
    pub success: bool,
    pub message: String,           // Human-readable result for LLM to relay
    pub data: Option<Value>,       // Optional structured data
}

#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters_schema(&self) -> Value;
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult>;
}

/// Central registry of all available skills.
pub struct SkillRegistry {
    skills: HashMap<String, Box<dyn Skill>>,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        let mut r = Self { skills: HashMap::new() };
        r.register(Box::new(communication::PokeClient));
        r.register(Box::new(communication::SendPrivateMsg));
        r.register(Box::new(communication::SendChannelMsg));
        r.register(Box::new(moderation::BanClient));
        r.register(Box::new(moderation::KickClient));
        r.register(Box::new(moderation::MoveClient));
        r.register(Box::new(information::GetClientInfo));
        r.register(Box::new(information::ListClients));
        r.register(Box::new(information::GetServerInfo));
        r
    }
}

impl SkillRegistry {
    pub fn register(&mut self, skill: Box<dyn Skill>) {
        self.skills.insert(skill.name().to_string(), skill);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Skill> {
        self.skills.get(name).map(|s| s.as_ref())
    }

    /// Build the tool schema list for LLM, filtered to allowed skills.
    pub fn tool_schemas(&self, allowed: &[String]) -> Vec<Value> {
        let all = allowed.contains(&"*".to_string());
        self.skills
            .values()
            .filter(|s| all || allowed.contains(&s.name().to_string()))
            .map(|s| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": s.name(),
                        "description": s.description(),
                        "parameters": s.parameters_schema(),
                    }
                })
            })
            .collect()
    }
}
```

**`src/skills/communication.rs`**
```rust
use super::{ExecutionContext, Skill, SkillResult};
use crate::{adapter::command::*, error::{AppError, Result}};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct PokeClient;

#[async_trait]
impl Skill for PokeClient {
    fn name(&self) -> &'static str { "poke_client" }
    fn description(&self) -> &'static str {
        "Send a poke notification to a connected client by their nickname."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["target_name", "message"],
            "properties": {
                "target_name": { "type": "string", "description": "Exact or partial nickname of the target client." },
                "message": { "type": "string", "description": "The poke message (max 100 chars)." }
            }
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult> {
        let target_name = args["target_name"].as_str().ok_or_else(|| AppError::SkillError("missing target_name".into()))?;
        let message = args["message"].as_str().ok_or_else(|| AppError::SkillError("missing message".into()))?;

        let target = ctx.cache.get_by_name(target_name)
            .ok_or_else(|| AppError::TargetNotFound { name: target_name.into() })?;

        ctx.adapter.send_raw(&cmd_poke(target.clid, message)).await?;
        ctx.audit.record(&ctx.caller, self.name(), &args, "ok", Some(target.cldbid)).await;

        Ok(SkillResult {
            success: true,
            message: format!("Poked {} successfully.", target.nickname),
            data: None,
        })
    }
}

pub struct SendPrivateMsg;

#[async_trait]
impl Skill for SendPrivateMsg {
    fn name(&self) -> &'static str { "send_private_msg" }
    fn description(&self) -> &'static str {
        "Send a private text message to a connected client."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["target_name", "message"],
            "properties": {
                "target_name": { "type": "string" },
                "message": { "type": "string" }
            }
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult> {
        let target_name = args["target_name"].as_str().ok_or_else(|| AppError::SkillError("missing target_name".into()))?;
        let message = args["message"].as_str().ok_or_else(|| AppError::SkillError("missing message".into()))?;

        let target = ctx.cache.get_by_name(target_name)
            .ok_or_else(|| AppError::TargetNotFound { name: target_name.into() })?;

        ctx.adapter.send_raw(&cmd_send_text(1, target.clid, message)).await?;
        ctx.audit.record(&ctx.caller, self.name(), &args, "ok", Some(target.cldbid)).await;

        Ok(SkillResult {
            success: true,
            message: format!("Message sent to {}.", target.nickname),
            data: None,
        })
    }
}

pub struct SendChannelMsg;

#[async_trait]
impl Skill for SendChannelMsg {
    fn name(&self) -> &'static str { "send_channel_msg" }
    fn description(&self) -> &'static str {
        "Send a text message to the current channel."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["message"],
            "properties": {
                "message": { "type": "string" }
            }
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult> {
        let message = args["message"].as_str().ok_or_else(|| AppError::SkillError("missing message".into()))?;
        ctx.adapter.send_raw(&cmd_send_text(2, 0, message)).await?;
        ctx.audit.record(&ctx.caller, self.name(), &args, "ok", None).await;
        Ok(SkillResult { success: true, message: "Channel message sent.".into(), data: None })
    }
}
```

**`src/skills/moderation.rs`**
```rust
use super::{ExecutionContext, Skill, SkillResult};
use crate::{adapter::command::*, error::{AppError, Result}, permission::PermissionGate};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct BanClient;

#[async_trait]
impl Skill for BanClient {
    fn name(&self) -> &'static str { "ban_client" }
    fn description(&self) -> &'static str {
        "Ban a client from the server for a specified duration. Use 0 for permanent."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["target_name", "duration_secs", "reason"],
            "properties": {
                "target_name": { "type": "string" },
                "duration_secs": { "type": "integer", "minimum": 0, "description": "Ban duration in seconds. 0 = permanent." },
                "reason": { "type": "string" }
            }
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult> {
        let target_name = args["target_name"].as_str().ok_or_else(|| AppError::SkillError("missing target_name".into()))?;
        let duration = args["duration_secs"].as_u64().unwrap_or(0);
        let reason = args["reason"].as_str().unwrap_or("Banned by administrator");

        let target = ctx.cache.get_by_name(target_name)
            .ok_or_else(|| AppError::TargetNotFound { name: target_name.into() })?;

        // Permission: can caller target this user?
        // We re-check here as a second defense layer
        let gate = crate::permission::PermissionGate::new(
            crate::config::AclConfig::load("config/acl.toml")
                .map_err(|e| AppError::ConfigError(e.to_string()))?
        );
        gate.can_target(&ctx.caller, &target)?;

        ctx.adapter.send_raw(&cmd_ban(target.clid, duration, reason)).await?;
        ctx.audit.record(&ctx.caller, self.name(), &args, "ok", Some(target.cldbid)).await;

        let duration_str = if duration == 0 { "permanently".into() } else { format!("for {duration}s") };
        Ok(SkillResult {
            success: true,
            message: format!("Banned {} {duration_str}. Reason: {reason}", target.nickname),
            data: None,
        })
    }
}

pub struct KickClient;

#[async_trait]
impl Skill for KickClient {
    fn name(&self) -> &'static str { "kick_client" }
    fn description(&self) -> &'static str { "Kick a client from the server." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["target_name"],
            "properties": {
                "target_name": { "type": "string" },
                "reason": { "type": "string", "default": "Kicked by administrator" }
            }
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult> {
        let target_name = args["target_name"].as_str().ok_or_else(|| AppError::SkillError("missing target_name".into()))?;
        let reason = args["reason"].as_str().unwrap_or("Kicked by administrator");
        let target = ctx.cache.get_by_name(target_name)
            .ok_or_else(|| AppError::TargetNotFound { name: target_name.into() })?;

        ctx.adapter.send_raw(&cmd_kick(target.clid, reason)).await?;
        ctx.audit.record(&ctx.caller, self.name(), &args, "ok", Some(target.cldbid)).await;
        Ok(SkillResult {
            success: true,
            message: format!("Kicked {}. Reason: {reason}", target.nickname),
            data: None,
        })
    }
}

pub struct MoveClient;

#[async_trait]
impl Skill for MoveClient {
    fn name(&self) -> &'static str { "move_client" }
    fn description(&self) -> &'static str { "Move a client to a different channel." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["target_name", "channel_id"],
            "properties": {
                "target_name": { "type": "string" },
                "channel_id": { "type": "integer", "description": "Target channel ID." }
            }
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult> {
        let target_name = args["target_name"].as_str().ok_or_else(|| AppError::SkillError("missing target_name".into()))?;
        let channel_id = args["channel_id"].as_u64().ok_or_else(|| AppError::SkillError("missing channel_id".into()))? as u32;
        let target = ctx.cache.get_by_name(target_name)
            .ok_or_else(|| AppError::TargetNotFound { name: target_name.into() })?;

        ctx.adapter.send_raw(&cmd_move(target.clid, channel_id)).await?;
        ctx.audit.record(&ctx.caller, self.name(), &args, "ok", Some(target.cldbid)).await;
        Ok(SkillResult {
            success: true,
            message: format!("Moved {} to channel {channel_id}.", target.nickname),
            data: None,
        })
    }
}
```

**`src/skills/information.rs`**
```rust
use super::{ExecutionContext, Skill, SkillResult};
use crate::{adapter::command::*, error::{AppError, Result}};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct GetClientInfo;

#[async_trait]
impl Skill for GetClientInfo {
    fn name(&self) -> &'static str { "get_client_info" }
    fn description(&self) -> &'static str {
        "Get detailed information about a client including their IP address, groups, and connection info."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["target_name"],
            "properties": {
                "target_name": { "type": "string" }
            }
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult> {
        let target_name = args["target_name"].as_str().ok_or_else(|| AppError::SkillError("missing target_name".into()))?;
        let target = ctx.cache.get_by_name(target_name)
            .ok_or_else(|| AppError::TargetNotFound { name: target_name.into() })?;

        ctx.adapter.send_raw(&cmd_clientinfo(target.clid)).await?;
        ctx.audit.record(&ctx.caller, self.name(), &args, "ok", Some(target.cldbid)).await;

        Ok(SkillResult {
            success: true,
            message: format!(
                "Client: {} | clid={} | cldbid={} | groups={:?}",
                target.nickname, target.clid, target.cldbid, target.server_groups
            ),
            data: Some(json!({
                "nickname": target.nickname,
                "clid": target.clid,
                "cldbid": target.cldbid,
                "server_groups": target.server_groups,
            })),
        })
    }
}

pub struct ListClients;

#[async_trait]
impl Skill for ListClients {
    fn name(&self) -> &'static str { "list_clients" }
    fn description(&self) -> &'static str { "List all currently connected clients on the server." }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult> {
        let clients = ctx.cache.all_clients();
        let names: Vec<&str> = clients.iter().map(|c| c.nickname.as_str()).collect();
        let count = clients.len();
        ctx.audit.record(&ctx.caller, self.name(), &args, "ok", None).await;
        Ok(SkillResult {
            success: true,
            message: format!("{count} clients online: {}", names.join(", ")),
            data: Some(json!(clients)),
        })
    }
}

pub struct GetServerInfo;

#[async_trait]
impl Skill for GetServerInfo {
    fn name(&self) -> &'static str { "get_server_info" }
    fn description(&self) -> &'static str { "Get general server information and statistics." }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<SkillResult> {
        ctx.adapter.send_raw(&cmd_serverinfo()).await?;
        ctx.audit.record(&ctx.caller, self.name(), &args, "ok", None).await;
        Ok(SkillResult {
            success: true,
            message: "Server info retrieved.".into(),
            data: None,
        })
    }
}
```

**`src/llm/mod.rs`**
```rust
pub mod engine;
pub mod provider;

pub use engine::LlmEngine;
```

**`src/llm/provider.rs`**
```rust
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use crate::error::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(
        &self,
        messages: Vec<LlmMessage>,
        tools: Vec<Value>,
    ) -> Result<LlmResponse>;
}

/// OpenAI-compatible provider (works for OpenAI, local Ollama with OpenAI compat, etc.)
pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
    timeout: std::time::Duration,
}

impl OpenAiProvider {
    pub fn new(cfg: &crate::config::LlmConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs))
            .build()
            .unwrap();
        Self {
            client,
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            max_tokens: cfg.max_tokens,
            timeout: std::time::Duration::from_secs(cfg.timeout_secs),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn complete(&self, messages: Vec<LlmMessage>, tools: Vec<Value>) -> Result<LlmResponse> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = serde_json::json!(tools);
            body["tool_choice"] = serde_json::json!("auto");
        }

        let resp = self.client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .json::<Value>()
            .await?;

        let choice = &resp["choices"][0]["message"];
        let text = choice["content"].as_str().map(|s| s.to_string());

        let tool_calls = choice["tool_calls"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|tc| {
                let id = tc["id"].as_str()?.to_string();
                let name = tc["function"]["name"].as_str()?.to_string();
                let args: Value = serde_json::from_str(
                    tc["function"]["arguments"].as_str().unwrap_or("{}")
                ).unwrap_or(Value::Object(Default::default()));
                Some(ToolCall { id, name, arguments: args })
            })
            .collect();

        Ok(LlmResponse { text, tool_calls })
    }
}
```

**`src/llm/engine.rs`**
```rust
use crate::{
    config::AppConfig,
    error::{AppError, Result},
    llm::provider::{LlmMessage, LlmProvider, LlmResponse, OpenAiProvider},
    skills::{ExecutionContext, SkillRegistry},
};
use arc_swap::ArcSwap;
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, warn};

pub struct LlmEngine {
    config: Arc<ArcSwap<AppConfig>>,
    provider: Box<dyn LlmProvider>,
}

impl LlmEngine {
    pub fn new(config: Arc<ArcSwap<AppConfig>>) -> Self {
        let cfg = config.load();
        let provider: Box<dyn LlmProvider> = match cfg.llm.provider.as_str() {
            "anthropic" => todo!("Implement AnthropicProvider"),
            _ => Box::new(OpenAiProvider::new(&cfg.llm)),
        };
        Self { config, provider }
    }

    pub async fn process(
        &self,
        user_message: &str,
        allowed_tools: Vec<Value>,
        system_prompt: &str,
    ) -> Result<ProcessResult> {
        let messages = vec![
            LlmMessage { role: "system".into(), content: system_prompt.into() },
            LlmMessage { role: "user".into(), content: user_message.into() },
        ];

        let cfg = self.config.load();
        let mut last_err = None;

        for attempt in 0..cfg.llm.retry_max {
            match self.provider.complete(messages.clone(), allowed_tools.clone()).await {
                Ok(resp) => {
                    return Ok(ProcessResult {
                        text_reply: resp.text,
                        tool_calls: resp.tool_calls
                            .into_iter()
                            .map(|tc| ToolCallRequest {
                                id: tc.id,
                                skill_name: tc.name,
                                arguments: tc.arguments,
                            })
                            .collect(),
                    });
                }
                Err(e) => {
                    warn!("LLM attempt {attempt} failed: {e}");
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        cfg.llm.retry_delay_ms * (attempt as u64 + 1),
                    )).await;
                }
            }
        }

        Err(AppError::LlmError(
            last_err.map(|e| e.to_string()).unwrap_or_default(),
        ))
    }
}

#[derive(Debug)]
pub struct ProcessResult {
    pub text_reply: Option<String>,
    pub tool_calls: Vec<ToolCallRequest>,
}

#[derive(Debug)]
pub struct ToolCallRequest {
    pub id: String,
    pub skill_name: String,
    pub arguments: Value,
}
```

**`src/audit/mod.rs`**
```rust
use crate::cache::ClientInfo;
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};
use tokio::{fs::OpenOptions, io::AsyncWriteExt, sync::Mutex};
use tracing::warn;

#[derive(Serialize)]
struct AuditRecord {
    ts: String,
    caller_cldbid: u32,
    caller_name: String,
    skill: String,
    args: Value,
    result: String,
    target_cldbid: Option<u32>,
}

pub struct AuditLog {
    writer: Arc<Mutex<tokio::fs::File>>,
    enabled: bool,
}

impl AuditLog {
    pub fn new(cfg: &crate::config::AuditConfig) -> std::io::Result<Self> {
        let path = PathBuf::from(&cfg.log_dir).join(&cfg.log_file);
        std::fs::create_dir_all(&cfg.log_dir)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let async_file = tokio::fs::File::from_std(file);
        Ok(Self {
            writer: Arc::new(Mutex::new(async_file)),
            enabled: cfg.enabled,
        })
    }

    pub async fn record(
        &self,
        caller: &ClientInfo,
        skill: &str,
        args: &Value,
        result: &str,
        target_cldbid: Option<u32>,
    ) {
        if !self.enabled { return; }
        let record = AuditRecord {
            ts: Utc::now().to_rfc3339(),
            caller_cldbid: caller.cldbid,
            caller_name: caller.nickname.clone(),
            skill: skill.to_string(),
            args: args.clone(),
            result: result.to_string(),
            target_cldbid,
        };
        let line = match serde_json::to_string(&record) {
            Ok(l) => format!("{l}\n"),
            Err(e) => { warn!("Audit serialize error: {e}"); return; }
        };
        let mut w = self.writer.lock().await;
        if let Err(e) = w.write_all(line.as_bytes()).await {
            warn!("Audit write error: {e}");
        }
    }
}
```

**`src/router.rs`**
```rust
use crate::{
    adapter::{TsAdapter, event::{TsEvent, TextMessageTarget}},
    audit::AuditLog,
    cache::ClientCache,
    config::AppConfig,
    llm::LlmEngine,
    permission::PermissionGate,
    skills::{ExecutionContext, SkillRegistry},
    error::AppError,
};
use arc_swap::ArcSwap;
use std::sync::Arc;
use tracing::{error, info, warn};

pub struct EventRouter {
    config: Arc<ArcSwap<AppConfig>>,
    adapter: Arc<TsAdapter>,
    cache: Arc<ClientCache>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    audit: Arc<AuditLog>,
}

impl EventRouter {
    pub fn new(
        config: Arc<ArcSwap<AppConfig>>,
        adapter: Arc<TsAdapter>,
        cache: Arc<ClientCache>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        audit: Arc<AuditLog>,
    ) -> Self {
        Self { config, adapter, cache, gate, llm, registry, audit }
    }

    pub async fn run(&self) -> crate::error::Result<()> {
        let mut rx = self.adapter.subscribe();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(
            self.config.load().bot.max_concurrent_requests,
        ));

        loop {
            let event = match rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Event queue lagged, dropped {n} events");
                    continue;
                }
                Err(_) => break,
            };

            if let TsEvent::TextMessage(msg) = event {
                let cfg = self.config.load();

                // Check if message targets the bot
                let is_private = msg.target_mode == TextMessageTarget::Private;
                let has_trigger = cfg.bot.trigger_prefixes.iter()
                    .any(|p| msg.message.starts_with(p.as_str()));

                if !is_private && !has_trigger { continue; }

                // Strip trigger prefix from message
                let user_text = cfg.bot.trigger_prefixes.iter()
                    .fold(msg.message.clone(), |m, p| {
                        m.strip_prefix(p.as_str()).unwrap_or(&m).trim().to_string()
                    });

                // Resolve caller from cache
                let caller = match self.cache.get_by_clid(msg.invoker_id) {
                    Some(c) => c,
                    None => {
                        warn!("Unknown invoker clid={}", msg.invoker_id);
                        continue;
                    }
                };

                // Clone everything for the spawned task
                let gate = self.gate.clone();
                let llm = self.llm.clone();
                let registry = self.registry.clone();
                let adapter = self.adapter.clone();
                let cache = self.cache.clone();
                let audit = self.audit.clone();
                let sem = semaphore.clone();
                let invoker_clid = msg.invoker_id;

                tokio::spawn(async move {
                    let _permit = match sem.try_acquire() {
                        Ok(p) => p,
                        Err(_) => {
                            // Send rate limit message back
                            let _ = adapter.send_raw(
                                &crate::adapter::command::cmd_send_text(1, invoker_clid, "Too many requests. Please wait.")
                            ).await;
                            return;
                        }
                    };

                    // Permission check
                    let allowed_skills = gate.allowed_skills(&caller);
                    if allowed_skills.is_empty() {
                        let _ = adapter.send_raw(
                            &crate::adapter::command::cmd_send_text(1, invoker_clid, "You do not have permission to use this bot.")
                        ).await;
                        return;
                    }

                    // Build filtered tool schemas
                    let tools = registry.tool_schemas(&allowed_skills);
                    let system_prompt = include_str!("../config/prompts.toml")
                        .lines()
                        .skip_while(|l| !l.contains("[system]"))
                        .nth(1)
                        .unwrap_or("You are a TeamSpeak admin assistant.")
                        .trim_matches('"')
                        .to_string();

                    // Call LLM
                    let ctx = ExecutionContext {
                        caller: caller.clone(),
                        allowed_skills: allowed_skills.clone(),
                        adapter: adapter.clone(),
                        cache: cache.clone(),
                        audit: audit.clone(),
                    };

                    let result = llm.process(&user_text, tools, &system_prompt).await;
                    match result {
                        Err(e) => {
                            error!("LLM error: {e}");
                            let _ = adapter.send_raw(
                                &crate::adapter::command::cmd_send_text(1, invoker_clid, "AI backend error. Please try again.")
                            ).await;
                        }
                        Ok(res) => {
                            // Execute tool calls
                            let mut replies = Vec::new();
                            for tc in res.tool_calls {
                                // Second permission check at skill level
                                if let Err(e) = gate.can_invoke(&caller, &tc.skill_name) {
                                    replies.push(format!("Permission denied: {e}"));
                                    continue;
                                }
                                match registry.get(&tc.skill_name) {
                                    None => replies.push(format!("Unknown skill: {}", tc.skill_name)),
                                    Some(skill) => {
                                        match skill.execute(tc.arguments, &ctx).await {
                                            Ok(sr) => replies.push(sr.message),
                                            Err(e) => replies.push(format!("Error: {e}")),
                                        }
                                    }
                                }
                            }
                            if let Some(text) = res.text_reply {
                                replies.push(text);
                            }
                            let reply = replies.join("\n");
                            if !reply.is_empty() {
                                let _ = adapter.send_raw(
                                    &crate::adapter::command::cmd_send_text(1, invoker_clid, &reply)
                                ).await;
                            }
                        }
                    }
                });
            }
        }
        Ok(())
    }
}
```

---

## 4. GitHub Actions 工作流

**`.github/workflows/ci.yml`**
```yaml
name: CI

on:
  push:
    branches: ["main", "develop"]
  pull_request:
    branches: ["main", "develop"]

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1

jobs:
  check:
    name: Check & Lint
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2

      - name: cargo fmt --check
        run: cargo fmt --all -- --check

      - name: cargo clippy
        run: cargo clippy --all-targets --all-features -- -D warnings

      - name: cargo check
        run: cargo check --all-targets

  test:
    name: Test Suite
    runs-on: ubuntu-latest
    needs: check
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2

      - name: Run tests
        run: cargo test --all-features -- --test-threads=4

      - name: Run doc tests
        run: cargo test --doc

  security:
    name: Security Audit
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install cargo-audit
        run: cargo install cargo-audit --locked
      - name: Audit dependencies
        run: cargo audit

  build-check:
    name: Build (all targets)
    runs-on: ubuntu-latest
    needs: check
    strategy:
      matrix:
        target:
          - x86_64-unknown-linux-gnu
          - x86_64-unknown-linux-musl
          - aarch64-unknown-linux-gnu
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust + target
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - name: Install cross (for cross-compilation)
        run: cargo install cross --locked

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
        with:
          key: ${{ matrix.target }}

      - name: Build
        run: cross build --release --target ${{ matrix.target }}
```

**`.github/workflows/pr.yml`**
```yaml
name: Pull Request Checks

on:
  pull_request:
    types: [opened, synchronize, reopened, ready_for_review]

jobs:
  label:
    name: Auto Label
    runs-on: ubuntu-latest
    permissions:
      pull-requests: write
    steps:
      - uses: actions/labeler@v5
        with:
          repo-token: "${{ secrets.GITHUB_TOKEN }}"

  size-check:
    name: PR Size Check
    runs-on: ubuntu-latest
    permissions:
      pull-requests: write
    steps:
      - uses: codelytv/pr-size-labeler@v1
        with:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          xs_max_size: 10
          s_max_size: 100
          m_max_size: 500
          l_max_size: 1000
          fail_if_xl: false

  title-check:
    name: Conventional Commit Title
    runs-on: ubuntu-latest
    steps:
      - uses: amannn/action-semantic-pull-request@v5
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        with:
          types: |
            feat
            fix
            docs
            style
            refactor
            perf
            test
            chore
            ci
            build
          requireScope: false

  ci-required:
    name: CI Required Gates
    runs-on: ubuntu-latest
    needs: []
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all -- --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test --all-features
```

**`.github/workflows/release.yml`**
```yaml
name: Release

on:
  push:
    tags:
      - "v[0-9]+.[0-9]+.[0-9]+"

permissions:
  contents: write

env:
  CARGO_TERM_COLOR: always
  BINARY_NAME: teamspeakclaw

jobs:
  validate-tag:
    name: Validate Tag
    runs-on: ubuntu-latest
    outputs:
      version: ${{ steps.version.outputs.version }}
    steps:
      - uses: actions/checkout@v4
      - name: Extract version from tag
        id: version
        run: echo "version=${GITHUB_REF_NAME#v}" >> "$GITHUB_OUTPUT"
      - name: Verify Cargo.toml version matches tag
        run: |
          CARGO_VERSION=$(grep '^version' Cargo.toml | head -1 | sed 's/version = "//;s/"//')
          if [ "$CARGO_VERSION" != "${{ steps.version.outputs.version }}" ]; then
            echo "Tag version ${{ steps.version.outputs.version }} does not match Cargo.toml $CARGO_VERSION"
            exit 1
          fi

  build-release:
    name: Build Release Binaries
    needs: validate-tag
    runs-on: ${{ matrix.runner }}
    strategy:
      fail-fast: false
      matrix:
        include:
          - target: x86_64-unknown-linux-gnu
            runner: ubuntu-latest
            archive_ext: tar.gz
          - target: x86_64-unknown-linux-musl
            runner: ubuntu-latest
            archive_ext: tar.gz
          - target: aarch64-unknown-linux-gnu
            runner: ubuntu-latest
            archive_ext: tar.gz
          - target: x86_64-apple-darwin
            runner: macos-latest
            archive_ext: tar.gz
          - target: aarch64-apple-darwin
            runner: macos-latest
            archive_ext: tar.gz
          - target: x86_64-pc-windows-msvc
            runner: windows-latest
            archive_ext: zip

    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - name: Install cross (Linux cross-compile)
        if: runner.os == 'Linux'
        run: cargo install cross --locked

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
        with:
          key: release-${{ matrix.target }}

      - name: Build (Linux via cross)
        if: runner.os == 'Linux'
        run: cross build --release --target ${{ matrix.target }}

      - name: Build (macOS / Windows native)
        if: runner.os != 'Linux'
        run: cargo build --release --target ${{ matrix.target }}

      - name: Package (Unix)
        if: matrix.archive_ext == 'tar.gz'
        run: |
          BINARY="target/${{ matrix.target }}/release/${{ env.BINARY_NAME }}"
          ARCHIVE="${{ env.BINARY_NAME }}-${{ needs.validate-tag.outputs.version }}-${{ matrix.target }}.tar.gz"
          cp "$BINARY" .
          tar czf "$ARCHIVE" "${{ env.BINARY_NAME }}" README.md LICENSE config/
          echo "ASSET=$ARCHIVE" >> "$GITHUB_ENV"

      - name: Package (Windows)
        if: matrix.archive_ext == 'zip'
        shell: pwsh
        run: |
          $bin = "target/${{ matrix.target }}/release/${{ env.BINARY_NAME }}.exe"
          $archive = "${{ env.BINARY_NAME }}-${{ needs.validate-tag.outputs.version }}-${{ matrix.target }}.zip"
          Copy-Item $bin .
          Compress-Archive -Path "${{ env.BINARY_NAME }}.exe","README.md","LICENSE","config/" -DestinationPath $archive
          echo "ASSET=$archive" | Out-File -FilePath $env:GITHUB_ENV -Append

      - name: Compute checksum (Unix)
        if: runner.os != 'Windows'
        run: sha256sum "${{ env.ASSET }}" > "${{ env.ASSET }}.sha256"

      - name: Compute checksum (Windows)
        if: runner.os == 'Windows'
        shell: pwsh
        run: |
          $hash = (Get-FileHash "${{ env.ASSET }}" -Algorithm SHA256).Hash.ToLower()
          "$hash  ${{ env.ASSET }}" | Out-File -FilePath "${{ env.ASSET }}.sha256"

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: release-${{ matrix.target }}
          path: |
            ${{ env.ASSET }}
            ${{ env.ASSET }}.sha256

  publish-release:
    name: Publish GitHub Release
    needs: [validate-tag, build-release]
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0

      - name: Download all artifacts
        uses: actions/download-artifact@v4
        with:
          pattern: release-*
          merge-multiple: true
          path: dist/

      - name: Generate changelog from commits
        id: changelog
        uses: orhun/git-cliff-action@v3
        with:
          config: .github/cliff.toml
          args: --latest --strip header

      - name: Create GitHub Release
        uses: softprops/action-gh-release@v2
        with:
          name: "TeamSpeakClaw ${{ github.ref_name }}"
          body: ${{ steps.changelog.outputs.content }}
          draft: false
          prerelease: ${{ contains(github.ref_name, '-alpha') || contains(github.ref_name, '-beta') || contains(github.ref_name, '-rc') }}
          files: dist/**
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}

  publish-docker:
    name: Publish Docker Image
    needs: [validate-tag, build-release]
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: write
    steps:
      - uses: actions/checkout@v4

      - name: Download linux musl binary
        uses: actions/download-artifact@v4
        with:
          name: release-x86_64-unknown-linux-musl
          path: dist/

      - name: Extract binary
        run: |
          tar xzf dist/${{ env.BINARY_NAME }}-*-x86_64-unknown-linux-musl.tar.gz
          chmod +x ${{ env.BINARY_NAME }}

      - name: Log in to GHCR
        uses: docker/login-action@v3
        with:
          registry: ghcr.io
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}

      - name: Docker meta
        id: meta
        uses: docker/metadata-action@v5
        with:
          images: ghcr.io/${{ github.repository }}
          tags: |
            type=semver,pattern={{version}}
            type=semver,pattern={{major}}.{{minor}}
            type=raw,value=latest

      - name: Build and push Docker image
        uses: docker/build-push-action@v5
        with:
          context: .
          push: true
          tags: ${{ steps.meta.outputs.tags }}
          labels: ${{ steps.meta.outputs.labels }}
```

**`.github/workflows/dependency-update.yml`**
```yaml
name: Dependency Update

on:
  schedule:
    - cron: "0 8 * * 1"   # Every Monday 08:00 UTC
  workflow_dispatch:

jobs:
  update:
    name: Update Dependencies
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Install cargo-edit
        run: cargo install cargo-edit --locked

      - name: Update dependencies
        run: cargo update

      - name: Run tests after update
        run: cargo test --all-features

      - name: Create PR
        uses: peter-evans/create-pull-request@v6
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
          commit-message: "chore: update cargo dependencies"
          title: "chore: weekly dependency update"
          body: |
            Automated weekly `cargo update` run.
            - Please review the `Cargo.lock` diff before merging.
          branch: chore/dependency-update
          delete-branch: true
          labels: dependencies, automated
```

**`.github/labeler.yml`**
```yaml
adapter:
  - changed-files:
    - any-glob-to-any-file: "src/adapter/**"

skills:
  - changed-files:
    - any-glob-to-any-file: "src/skills/**"

llm:
  - changed-files:
    - any-glob-to-any-file: "src/llm/**"

permissions:
  - changed-files:
    - any-glob-to-any-file: "src/permission/**"

config:
  - changed-files:
    - any-glob-to-any-file: "config/**"
    - any-glob-to-any-file: "src/config.rs"

ci:
  - changed-files:
    - any-glob-to-any-file: ".github/**"

documentation:
  - changed-files:
    - any-glob-to-any-file: "*.md"
    - any-glob-to-any-file: "docs/**"
```

**`.github/cliff.toml`**
```toml
[changelog]
header = ""
body = """
{% for group, commits in commits | group_by(attribute="group") %}
### {{ group | striptags | trim | upper_first }}
{% for commit in commits %}
- {{ commit.message | upper_first }} ([{{ commit.id | truncate(length=7, end="") }}]({{ commit.remote.link }}/commit/{{ commit.id }}))
{% endfor %}
{% endfor %}
"""
trim = true

[git]
conventional_commits = true
filter_unconventional = true
commit_parsers = [
  { message = "^feat", group = "Features" },
  { message = "^fix", group = "Bug Fixes" },
  { message = "^perf", group = "Performance" },
  { message = "^refactor", group = "Refactoring" },
  { message = "^docs", group = "Documentation" },
  { message = "^test", group = "Testing" },
  { message = "^chore|^ci|^build", group = "Maintenance" },
]
filter_commits = false
tag_pattern = "v[0-9].*"
```

---

## 5. Docker 与部署

**`Dockerfile`**
```dockerfile
# Build stage
FROM rust:1.78-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
# Cache dependency compilation
RUN mkdir src && echo "fn main(){}" > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release

# Runtime stage — minimal scratch image
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
RUN useradd -r -u 1001 -g daemon claw
WORKDIR /app
COPY --from=builder /build/target/release/teamspeakclaw /usr/local/bin/teamspeakclaw
COPY config/ ./config/
RUN mkdir -p logs && chown -R claw:daemon /app
USER claw
EXPOSE 9000
ENTRYPOINT ["teamspeakclaw"]
```

**`docker-compose.yml`**
```yaml
version: "3.9"

services:
  teamspeakclaw:
    image: ghcr.io/yourorg/teamspeakclaw:latest
    container_name: teamspeakclaw
    restart: unless-stopped
    environment:
      - TS_HOST=${TS_HOST}
      - TS_PORT=${TS_PORT:-10011}
      - TS_LOGIN_NAME=${TS_LOGIN_NAME:-serveradmin}
      - TS_LOGIN_PASS=${TS_LOGIN_PASS}
      - TS_SERVER_ID=${TS_SERVER_ID:-1}
      - LLM_PROVIDER=${LLM_PROVIDER:-openai}
      - LLM_API_KEY=${LLM_API_KEY}
      - LLM_MODEL=${LLM_MODEL:-gpt-4o}
      - RUST_LOG=${RUST_LOG:-info}
    volumes:
      - ./config:/app/config:ro
      - ./logs:/app/logs
    healthcheck:
      test: ["CMD", "/usr/local/bin/teamspeakclaw", "--health-check"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 10s
```

---

## 6. 辅助文件

**`CONTRIBUTING.md`**
```markdown
# Contributing to TeamSpeakClaw

## Branch Strategy

- `main` — production releases only (protected)
- `develop` — integration branch for PRs
- `feat/*` — new features
- `fix/*` — bug fixes
- `chore/*` — maintenance, dependencies

## Commit Convention

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(skills): add server group assignment skill
fix(adapter): handle ServerQuery timeout on reconnect
perf(cache): switch to DashMap for concurrent access
docs: update ACL configuration examples
```

## Releasing

1. Bump version in `Cargo.toml`
2. Commit: `chore: bump version to x.y.z`
3. Push to `develop`, open PR to `main`
4. After merge: `git tag vx.y.z && git push origin vx.y.z`
5. GitHub Actions handles the rest automatically

## Adding a New Skill

1. Create your struct in `src/skills/your_module.rs`
2. Implement `Skill` trait (name, description, parameters_schema, execute)
3. Register it in `SkillRegistry::default()` in `src/skills/mod.rs`
4. Add the skill name to `config/acl.toml` rules as needed
5. Write a unit test in the same file

## Code Style

- `cargo fmt` before every commit
- `cargo clippy -- -D warnings` must pass
- All public items need doc comments
```

**`README.md`**
```markdown
# TeamSpeakClaw 🦀

LLM-powered TeamSpeak ServerQuery bot written in Rust.

## Features

- Natural language admin commands via LLM tool-calling
- Two-layer permission system (ACL gate + per-skill enforcement)
- Audit log (JSON-lines) for every skill execution
- DashMap-based client cache with background refresh
- Per-user rate limiting (token bucket via `governor`)
- Hot-reload config (no restart required)
- Stateless — no conversation history stored
- Pluggable LLM backend (OpenAI, Anthropic, Ollama)

## Quick Start

```bash
cp .env.example .env
# Fill in TS_LOGIN_PASS and LLM_API_KEY

cargo run --release
```

## Docker

```bash
docker compose up -d
```

## Configuration

| File | Purpose |
|---|---|
| `config/settings.toml` | TS connection, LLM settings, bot behavior |
| `config/acl.toml` | Permission rules per server group |
| `config/prompts.toml` | LLM system prompt |

## Usage

Private message the bot, or prefix a message with a trigger word:

```
!tsclaw ban Spammer123 for 1 hour, reason: flooding
!tsclaw list all online clients
!tsclaw poke JohnDoe and tell him to read the rules
```

## Adding Skills

See `CONTRIBUTING.md`.

## License

MIT
```

---

所有文件已完整输出。你可以按以下顺序创建项目：

```bash
# 初始化
cargo new teamspeakclaw
cd teamspeakclaw
mkdir -p src/{adapter,cache,permission,skills,llm,audit} config .github/workflows

# 按上面的内容逐个创建文件，然后：
cargo check    # 验证结构
cargo clippy   # 检查问题
cargo build    # 首次构建
```

要发布第一个版本，只需：
```bash
git tag v0.1.0
git push origin v0.1.0
# GitHub Actions 自动构建所有平台二进制、生成 Release、推送 Docker 镜像
```
