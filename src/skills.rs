pub mod communication;
pub mod information;
pub mod moderation;
pub mod music;
pub mod web_search;

use crate::adapter::napcat::NapCatAdapter;
use crate::adapter::TsAdapter;
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

// ─────────────────────────────────────────────
// 平台类型枚举
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    TeamSpeak,
    NapCat,
}

// ─────────────────────────────────────────────
// TeamSpeak 执行上下文
// ─────────────────────────────────────────────

pub struct ExecutionContext {
    pub adapter: Arc<TsAdapter>,
    pub caller_id: u32,
    pub caller_name: String,
    pub caller_groups: Vec<u32>,
    pub caller_channel_group_id: u32,
    pub gate: Arc<PermissionGate>,
    pub config: Arc<AppConfig>,
}

// ─────────────────────────────────────────────
// NapCat / QQ 执行上下文
// ─────────────────────────────────────────────

pub struct NcExecutionContext {
    pub adapter: Arc<NapCatAdapter>,
    pub caller_id: i64,
    pub caller_name: String,
    pub caller_groups: Vec<u32>,
    pub caller_group_id: Option<i64>,
    pub gate: Arc<PermissionGate>,
    pub config: Arc<AppConfig>,
}

// ─────────────────────────────────────────────
// 统一执行上下文（跨平台）
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

impl UnifiedExecutionContext {
    pub fn from_ts(ctx: &ExecutionContext) -> Self {
        Self {
            platform: Platform::TeamSpeak,
            ts_adapter: Some(ctx.adapter.clone()),
            nc_adapter: None,
            caller_id: ctx.caller_id,
            caller_id_nc: 0,
            caller_name: ctx.caller_name.clone(),
            caller_groups: ctx.caller_groups.clone(),
            caller_channel_group_id: ctx.caller_channel_group_id,
            nc_group_id: None,
            gate: ctx.gate.clone(),
            config: ctx.config.clone(),
        }
    }

    pub fn from_nc(ctx: &NcExecutionContext) -> Self {
        Self {
            platform: Platform::NapCat,
            ts_adapter: None,
            nc_adapter: Some(ctx.adapter.clone()),
            caller_id: 0,
            caller_id_nc: ctx.caller_id,
            caller_name: ctx.caller_name.clone(),
            caller_groups: ctx.caller_groups.clone(),
            caller_channel_group_id: 0,
            nc_group_id: ctx.caller_group_id,
            gate: ctx.gate.clone(),
            config: ctx.config.clone(),
        }
    }

    pub fn with_cross_adapters(
        mut self,
        ts_adapter: Option<Arc<TsAdapter>>,
        nc_adapter: Option<Arc<NapCatAdapter>>,
    ) -> Self {
        self.ts_adapter = ts_adapter;
        self.nc_adapter = nc_adapter;
        self
    }

    /// 从统一上下文还原 TeamSpeak 执行上下文
    pub fn to_ts_ctx(&self) -> Result<ExecutionContext> {
        Ok(ExecutionContext {
            adapter: self
                .ts_adapter
                .clone()
                .ok_or_else(|| anyhow::anyhow!("TeamSpeak adapter not available"))?,
            caller_id: self.caller_id,
            caller_name: self.caller_name.clone(),
            caller_groups: self.caller_groups.clone(),
            caller_channel_group_id: self.caller_channel_group_id,
            gate: self.gate.clone(),
            config: self.config.clone(),
        })
    }

    pub fn to_nc_ctx(&self) -> Result<NcExecutionContext> {
        Ok(NcExecutionContext {
            adapter: self
                .nc_adapter
                .clone()
                .ok_or_else(|| anyhow::anyhow!("NapCat adapter not available"))?,
            caller_id: self.caller_id_nc,
            caller_name: self.caller_name.clone(),
            caller_groups: self.caller_groups.clone(),
            caller_group_id: self.nc_group_id,
            gate: self.gate.clone(),
            config: self.config.clone(),
        })
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

    /// TeamSpeak 执行（原有）
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value>;

    /// NapCat/QQ 执行（默认返回"不支持"，各 Skill 按需覆盖）
    async fn execute_nc(&self, args: Value, _ctx: &NcExecutionContext) -> Result<Value> {
        let _ = args;
        Err(anyhow::anyhow!(
            "Skill '{}' does not support the NapCat platform",
            self.name()
        ))
    }

    /// 统一执行，默认分派到当前平台的原生实现
    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        match ctx.platform {
            Platform::TeamSpeak => self.execute(args, &ctx.to_ts_ctx()?).await,
            Platform::NapCat => self.execute_nc(args, &ctx.to_nc_ctx()?).await,
        }
    }

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

    pub async fn execute_skill(
        &self,
        call: &ToolCall,
        exec_ctx: ExecutionContext,
        allowed_skills: &[String],
        nc_adapter: Option<Arc<NapCatAdapter>>,
    ) -> String {
        if !is_skill_allowed(&call.name, allowed_skills) {
            warn!(skill = %call.name, "Skill execution denied by ACL");
            return "Skill execution denied".to_string();
        }

        if let Some(skill) = self.get(&call.name) {
            let ts_adapter = Some(exec_ctx.adapter.clone());
            let unified_ctx = UnifiedExecutionContext::from_ts(&exec_ctx)
                .with_cross_adapters(ts_adapter, nc_adapter);

            match skill
                .execute_unified(call.arguments.clone(), &unified_ctx)
                .await
            {
                Ok(val) => val.to_string(),
                Err(e) => {
                    error!(skill = %call.name, error = %e, "Skill execution failed");
                    format!("Skill execution failed: {}", e)
                }
            }
        } else {
            warn!(skill = %call.name, "Skill not found");
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

        async fn execute(&self, _args: Value, _ctx: &ExecutionContext) -> Result<Value> {
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
    async fn unified_execution_uses_platform_native_context() {
        let skill = TestSkill("test");

        let ts_error = skill
            .execute_unified(json!({}), &unified_context(Platform::TeamSpeak))
            .await
            .unwrap_err();
        assert_eq!(ts_error.to_string(), "TeamSpeak adapter not available");

        let nc_error = skill
            .execute_unified(json!({}), &unified_context(Platform::NapCat))
            .await
            .unwrap_err();
        assert_eq!(nc_error.to_string(), "NapCat adapter not available");
    }
}
