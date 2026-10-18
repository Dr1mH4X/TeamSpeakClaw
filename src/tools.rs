pub mod communication;
pub mod information;
pub mod moderation;
pub mod music;
pub mod voice_audio;
pub mod voice_replay;
pub mod web_fetch;
pub mod web_search;

mod http;

use crate::adapter::headless::{parse_server_groups, TsAdapter};
use crate::adapter::napcat::NapCatAdapter;
use crate::config::AppConfig;
use crate::config::MusicBackendConfig;
use crate::llm::ToolCall;
use crate::permission::PermissionGate;
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;
use std::sync::Arc;
use tracing::{error, info, warn};

pub use voice_audio::VoiceAudioHandles;

pub(crate) fn required_u32(args: &Value, name: &str) -> Result<u32> {
    let value = args
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("Parameter '{}' must be a non-negative integer", name))?;
    u32::try_from(value)
        .map_err(|_| anyhow::anyhow!("Parameter '{}' exceeds the supported range", name))
}

pub(crate) fn is_tool_allowed(name: &str, allowed_tools: &[String]) -> bool {
    allowed_tools
        .iter()
        .any(|allowed| allowed == "*" || allowed == name)
}

/// 统一上下文中取 TS 适配器（NapCat 跨适配器分支共用）
pub(crate) fn unified_ts_adapter(ctx: &UnifiedExecutionContext) -> Result<Arc<TsAdapter>> {
    ctx.ts_adapter
        .clone()
        .ok_or_else(|| anyhow::anyhow!("TeamSpeak adapter not available"))
}

/// 按 clid 在 TS 在线列表中解析目标客户端及其服务器组（NapCat 跨适配器分支共用）
pub(crate) async fn resolve_ts_client(
    ctx: &UnifiedExecutionContext,
    clid: u32,
) -> Result<(tsclient_rs::ClientInfo, Vec<u32>)> {
    let ts_adapter = unified_ts_adapter(ctx)?;
    let clients = ts_adapter.list_clients().await?;
    let client = clients
        .into_iter()
        .find(|c| u32::try_from(c.id).ok() == Some(clid))
        .ok_or_else(|| anyhow::anyhow!("Client {} is not online or does not exist", clid))?;
    let groups = parse_server_groups(&client.server_groups);
    Ok((client, groups))
}

// ─────────────────────────────────────────────
// 平台类型枚举
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    TeamSpeak,
    NapCat,
}

// ─────────────────────────────────────────────
// 统一执行上下文
// ─────────────────────────────────────────────

pub struct UnifiedExecutionContext {
    pub platform: Platform,
    pub ts_adapter: Option<Arc<TsAdapter>>,
    pub nc_adapter: Option<Arc<NapCatAdapter>>,
    pub caller_id: u32,
    pub caller_id_nc: i64,
    pub caller_name: String,
    pub caller_groups: Vec<u32>,
    pub caller_channel_group_id: u32,
    pub nc_group_id: Option<i64>,
    pub gate: Arc<PermissionGate>,
    pub config: Arc<AppConfig>,
}

/// TeamSpeak 调用者信息
pub struct TsCaller {
    pub adapter: Arc<TsAdapter>,
    pub caller_id: u32,
    pub caller_name: String,
    pub caller_groups: Vec<u32>,
    pub caller_channel_group_id: u32,
    pub nc_adapter: Option<Arc<NapCatAdapter>>,
}

/// NapCat 调用者信息
pub struct NcCaller {
    pub adapter: Arc<NapCatAdapter>,
    pub caller_id: i64,
    pub caller_name: String,
    pub caller_groups: Vec<u32>,
    pub group_id: Option<i64>,
    pub ts_adapter: Option<Arc<TsAdapter>>,
}

impl UnifiedExecutionContext {
    pub fn for_ts(caller: TsCaller, gate: Arc<PermissionGate>, config: Arc<AppConfig>) -> Self {
        Self {
            platform: Platform::TeamSpeak,
            ts_adapter: Some(caller.adapter),
            nc_adapter: caller.nc_adapter,
            caller_id: caller.caller_id,
            caller_id_nc: 0,
            caller_name: caller.caller_name,
            caller_groups: caller.caller_groups,
            caller_channel_group_id: caller.caller_channel_group_id,
            nc_group_id: None,
            gate,
            config,
        }
    }

    pub fn for_nc(caller: NcCaller, gate: Arc<PermissionGate>, config: Arc<AppConfig>) -> Self {
        Self {
            platform: Platform::NapCat,
            ts_adapter: caller.ts_adapter,
            nc_adapter: Some(caller.adapter),
            caller_id: 0,
            caller_id_nc: caller.caller_id,
            caller_name: caller.caller_name,
            caller_groups: caller.caller_groups,
            caller_channel_group_id: 0,
            nc_group_id: caller.group_id,
            gate,
            config,
        }
    }
}

// ─────────────────────────────────────────────
// ToolContext：工具构造依赖的统一来源
// ─────────────────────────────────────────────

pub struct ToolContext {
    pub config: Arc<AppConfig>,
    pub voice_audio: VoiceAudioHandles,
}

impl ToolContext {
    pub fn music_backend_config(&self) -> Option<&MusicBackendConfig> {
        self.config.music_backend.as_ref()
    }
}

/// Tool 工厂函数类型
pub type ToolFactory = fn(&ToolContext) -> Box<dyn Tool>;

// ─────────────────────────────────────────────
// Tool trait
// ─────────────────────────────────────────────

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;

    async fn execute(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value>;

    /// 是否应该注册此工具，默认 true。覆盖返回 false 可阻止注册。
    fn should_register(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────
// ToolRegistry
// ─────────────────────────────────────────────

#[derive(Default)]
pub struct ToolRegistry {
    tools: DashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_defaults(config: Arc<AppConfig>, voice_audio: VoiceAudioHandles) -> Self {
        let ctx = ToolContext {
            config,
            voice_audio,
        };
        let reg = Self::default();
        for (_, factory) in DEFAULT_TOOLS.iter() {
            reg.register(factory(&ctx));
        }
        info!("Tools registered: {:?}", reg.list_tools());
        reg
    }

    pub fn register(&self, tool: Box<dyn Tool>) {
        if !tool.should_register() {
            info!("Tool '{}' disabled, skipping", tool.name());
            return;
        }
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<impl std::ops::Deref<Target = Box<dyn Tool>> + '_> {
        self.tools.get(name)
    }

    pub fn list_tools(&self) -> Vec<String> {
        let mut tools: Vec<_> = self.tools.iter().map(|tool| tool.key().clone()).collect();
        tools.sort_unstable();
        tools
    }

    /// 统一工具执行入口：ACL 检查 → 取工具 → 执行 → 结果/错误格式化。
    pub async fn execute_tool(
        &self,
        call: &ToolCall,
        ctx: UnifiedExecutionContext,
        allowed_tools: &[String],
    ) -> String {
        if !is_tool_allowed(&call.name, allowed_tools) {
            warn!(tool = %call.name, platform = ?ctx.platform, "Tool execution denied by ACL");
            return "Tool execution denied".to_string();
        }

        if let Some(tool) = self.get(&call.name) {
            match tool.execute(call.arguments.clone(), &ctx).await {
                Ok(val) => {
                    info!(
                        tool = %call.name,
                        platform = ?ctx.platform,
                        caller = %ctx.caller_name,
                        "Tool executed"
                    );
                    val.to_string()
                }
                Err(e) => {
                    error!(tool = %call.name, platform = ?ctx.platform, error = %e, "Tool failed");
                    format!("Tool execution failed: {}", e)
                }
            }
        } else {
            warn!(tool = %call.name, platform = ?ctx.platform, "Tool not found");
            "Tool not found".to_string()
        }
    }

    pub fn to_tool_schemas(&self, allowed_tools: &[String]) -> Vec<Value> {
        let mut schemas: Vec<_> = self
            .tools
            .iter()
            .filter(|tool| is_tool_allowed(tool.key(), allowed_tools))
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters": tool.parameters()
                    }
                })
            })
            .collect();
        schemas.sort_unstable_by(|left, right| {
            left["function"]["name"]
                .as_str()
                .cmp(&right["function"]["name"].as_str())
        });
        schemas
    }
}

// ─────────────────────────────────────────────
// 声明式工厂表：新增工具 = 加一行
// ─────────────────────────────────────────────

static DEFAULT_TOOLS: &[(&str, ToolFactory)] = &[
    ("poke_client", |_| {
        Box::new(communication::PokeClient) as Box<dyn Tool>
    }),
    ("send_message", |_| {
        Box::new(communication::SendMessage) as Box<dyn Tool>
    }),
    ("kick_client", |_| {
        Box::new(moderation::KickClient) as Box<dyn Tool>
    }),
    ("ban_client", |_| {
        Box::new(moderation::BanClient) as Box<dyn Tool>
    }),
    ("move_client", |_| {
        Box::new(moderation::MoveClient) as Box<dyn Tool>
    }),
    ("get_client_info", |_| {
        Box::new(information::GetClientInfo) as Box<dyn Tool>
    }),
    ("web_search", |ctx| {
        Box::new(web_search::WebSearch::new(ctx.config.web_search.provider)) as Box<dyn Tool>
    }),
    ("web_fetch", |_| {
        Box::new(web_fetch::WebFetch) as Box<dyn Tool>
    }),
    ("music_control", |ctx| {
        Box::new(music::MusicControl::new(ctx.music_backend_config())) as Box<dyn Tool>
    }),
    ("voice_replay", |ctx| {
        Box::new(voice_replay::VoiceReplay::new(
            ctx.config.clone(),
            ctx.voice_audio.clone(),
        )) as Box<dyn Tool>
    }),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AclConfig;
    use serde_json::json;

    struct TestTool(&'static str);

    #[async_trait]
    impl Tool for TestTool {
        fn name(&self) -> &'static str {
            self.0
        }

        fn description(&self) -> &'static str {
            "test"
        }

        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, _args: Value, _ctx: &UnifiedExecutionContext) -> Result<Value> {
            Ok(json!("ts"))
        }
    }

    fn unified_context(platform: Platform) -> UnifiedExecutionContext {
        UnifiedExecutionContext {
            platform,
            ts_adapter: None,
            nc_adapter: None,
            caller_id: 1,
            caller_id_nc: 2,
            caller_name: "test".to_string(),
            caller_groups: vec![],
            caller_channel_group_id: 0,
            nc_group_id: None,
            gate: Arc::new(PermissionGate::new(AclConfig::default())),
            config: Arc::new(AppConfig::default()),
        }
    }

    #[test]
    fn required_u32_rejects_overflow() {
        let args = json!({"id": u64::from(u32::MAX) + 1});
        assert!(required_u32(&args, "id").is_err());
    }

    #[test]
    fn tool_schemas_are_filtered_and_sorted() {
        let registry = ToolRegistry::default();
        registry.register(Box::new(TestTool("zeta")));
        registry.register(Box::new(TestTool("alpha")));

        let schemas = registry.to_tool_schemas(&["*".to_string()]);
        let names: Vec<_> = schemas
            .iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);

        let schemas = registry.to_tool_schemas(&["zeta".to_string()]);
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0]["function"]["name"], "zeta");
        assert!(is_tool_allowed("zeta", &["zeta".to_string()]));
        assert!(!is_tool_allowed("alpha", &["zeta".to_string()]));
    }

    #[tokio::test]
    async fn tool_execution_receives_unified_context() {
        let tool = TestTool("test");

        let ts_ok = tool
            .execute(json!({}), &unified_context(Platform::TeamSpeak))
            .await
            .unwrap();
        assert_eq!(ts_ok, json!("ts"));

        let nc_ok = tool
            .execute(json!({}), &unified_context(Platform::NapCat))
            .await
            .unwrap();
        assert_eq!(nc_ok, json!("ts"));
    }

    #[test]
    fn unified_ts_adapter_missing_is_error() {
        let ctx = unified_context(Platform::TeamSpeak);
        assert!(unified_ts_adapter(&ctx).is_err());
    }
}
