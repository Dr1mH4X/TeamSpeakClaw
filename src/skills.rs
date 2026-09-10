pub mod communication;
pub mod information;
pub mod moderation;
pub mod music;
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
use tracing::{debug, error, info, warn};

pub(crate) fn required_u32(args: &Value, name: &str) -> Result<u32> {
    let value = args
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("Parameter '{}' must be a non-negative integer", name))?;
    u32::try_from(value)
        .map_err(|_| anyhow::anyhow!("Parameter '{}' exceeds the supported range", name))
}

pub(crate) fn is_skill_allowed(name: &str, allowed_skills: &[String]) -> bool {
    allowed_skills
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
// SkillContext：skill 构造依赖的统一来源
// ─────────────────────────────────────────────

pub struct SkillContext {
    pub config: Arc<AppConfig>,
}

impl SkillContext {
    pub fn music_backend_config(&self) -> Option<&MusicBackendConfig> {
        self.config.music_backend.as_ref()
    }
}

/// Skill 工厂函数类型
pub type SkillFactory = fn(&SkillContext) -> Box<dyn Skill>;

// ─────────────────────────────────────────────
// Skill trait
// ─────────────────────────────────────────────

#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;

    async fn execute(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value>;

    /// 是否应该注册此 skill，默认 true。覆盖返回 false 可阻止注册。
    fn should_register(&self) -> bool {
        true
    }
}

// ─────────────────────────────────────────────
// SkillRegistry
// ─────────────────────────────────────────────

#[derive(Default)]
pub struct SkillRegistry {
    skills: DashMap<String, Box<dyn Skill>>,
}

impl SkillRegistry {
    pub fn with_defaults(config: Arc<AppConfig>) -> Self {
        let ctx = SkillContext { config };
        let reg = Self::default();
        for (name, factory) in DEFAULT_SKILLS.iter() {
            debug!(skill = name, "constructing");
            reg.register(factory(&ctx));
        }
        info!("Skills registered: {:?}", reg.list_skills());
        reg
    }

    pub fn register(&self, skill: Box<dyn Skill>) {
        if !skill.should_register() {
            info!("Skill '{}' disabled, skipping", skill.name());
            return;
        }
        self.skills.insert(skill.name().to_string(), skill);
    }

    pub fn get(&self, name: &str) -> Option<impl std::ops::Deref<Target = Box<dyn Skill>> + '_> {
        self.skills.get(name)
    }

    pub fn list_skills(&self) -> Vec<String> {
        let mut skills: Vec<_> = self
            .skills
            .iter()
            .map(|skill| skill.key().clone())
            .collect();
        skills.sort_unstable();
        skills
    }

    /// 统一技能执行入口：ACL 检查 → 取技能 → 执行 → 结果/错误格式化。
    pub async fn execute_skill(
        &self,
        call: &ToolCall,
        ctx: UnifiedExecutionContext,
        allowed_skills: &[String],
    ) -> String {
        if !is_skill_allowed(&call.name, allowed_skills) {
            warn!(skill = %call.name, platform = ?ctx.platform, "Skill execution denied by ACL");
            return "Skill execution denied".to_string();
        }

        if let Some(skill) = self.get(&call.name) {
            match skill.execute(call.arguments.clone(), &ctx).await {
                Ok(val) => {
                    info!(
                        skill = %call.name,
                        platform = ?ctx.platform,
                        caller = %ctx.caller_name,
                        "Skill executed"
                    );
                    val.to_string()
                }
                Err(e) => {
                    error!(skill = %call.name, platform = ?ctx.platform, error = %e, "Skill failed");
                    format!("Skill execution failed: {}", e)
                }
            }
        } else {
            warn!(skill = %call.name, platform = ?ctx.platform, "Skill not found");
            "Skill not found".to_string()
        }
    }

    pub fn to_tool_schemas(&self, allowed_skills: &[String]) -> Vec<Value> {
        let mut schemas: Vec<_> = self
            .skills
            .iter()
            .filter(|skill| is_skill_allowed(skill.key(), allowed_skills))
            .map(|skill| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": skill.name(),
                        "description": skill.description(),
                        "parameters": skill.parameters()
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
// 声明式工厂表：新增 skill = 加一行
// ─────────────────────────────────────────────

static DEFAULT_SKILLS: &[(&str, SkillFactory)] = &[
    ("poke_client", |_| {
        Box::new(communication::PokeClient) as Box<dyn Skill>
    }),
    ("send_message", |_| {
        Box::new(communication::SendMessage) as Box<dyn Skill>
    }),
    ("kick_client", |_| {
        Box::new(moderation::KickClient) as Box<dyn Skill>
    }),
    ("ban_client", |_| {
        Box::new(moderation::BanClient) as Box<dyn Skill>
    }),
    ("move_client", |_| {
        Box::new(moderation::MoveClient) as Box<dyn Skill>
    }),
    ("get_client_info", |_| {
        Box::new(information::GetClientInfo) as Box<dyn Skill>
    }),
    ("web_search", |_| {
        Box::new(web_search::WebSearch) as Box<dyn Skill>
    }),
    ("music_control", |ctx| {
        Box::new(music::MusicControl::new(ctx.music_backend_config())) as Box<dyn Skill>
    }),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AclConfig;
    use serde_json::json;

    struct TestSkill(&'static str);

    #[async_trait]
    impl Skill for TestSkill {
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
        let registry = SkillRegistry::default();
        registry.register(Box::new(TestSkill("zeta")));
        registry.register(Box::new(TestSkill("alpha")));

        let schemas = registry.to_tool_schemas(&["*".to_string()]);
        let names: Vec<_> = schemas
            .iter()
            .map(|schema| schema["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);

        let schemas = registry.to_tool_schemas(&["zeta".to_string()]);
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0]["function"]["name"], "zeta");
        assert!(is_skill_allowed("zeta", &["zeta".to_string()]));
        assert!(!is_skill_allowed("alpha", &["zeta".to_string()]));
    }

    #[tokio::test]
    async fn skill_execution_receives_unified_context() {
        let skill = TestSkill("test");

        let ts_ok = skill
            .execute(json!({}), &unified_context(Platform::TeamSpeak))
            .await
            .unwrap();
        assert_eq!(ts_ok, json!("ts"));

        let nc_ok = skill
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
