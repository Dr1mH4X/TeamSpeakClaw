pub mod communication;
pub mod information;
pub mod moderation;
pub mod music;
pub mod web_search;

use crate::adapter::napcat::NapCatAdapter;
use crate::adapter::TsAdapter;
use crate::config::AppConfig;
use crate::llm::ToolCall;
use crate::permission::PermissionGate;
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, error, info, warn};

// ─────────────────────────────────────────────
// 平台类型枚举
// ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    TeamSpeak,
    NapCat,
}

// ─────────────────────────────────────────────
// TeamSpeak 执行上下文（原有）
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
// NapCat / QQ 执行上下文（新增）
// ─────────────────────────────────────────────

pub struct NcExecutionContext {
    pub adapter: Arc<NapCatAdapter>,
    pub caller_id: i64,
    pub caller_name: String,
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
            caller_groups: vec![],
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
}

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

    /// 统一执行（支持跨平台，默认为 nil 表示不支持）
    async fn execute_unified(&self, args: Value, _ctx: &UnifiedExecutionContext) -> Result<Value> {
        let _ = args;
        Err(anyhow::anyhow!(
            "Skill '{}' does not support unified execution",
            self.name()
        ))
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
    pub fn with_defaults(config: &AppConfig) -> Self {
        use communication::{PokeClient, SendMessage};
        use information::GetClientInfo;
        use moderation::{BanClient, KickClient, MoveClient};
        use music::MusicControl;
        use web_search::WebSearch;

        let registry = Self::default();
        registry.register(Box::new(PokeClient));
        registry.register(Box::new(SendMessage));
        registry.register(Box::new(KickClient));
        registry.register(Box::new(BanClient));
        registry.register(Box::new(MoveClient));
        registry.register(Box::new(GetClientInfo));
        registry.register(Box::new(WebSearch));
        registry.register(Box::new(MusicControl::new(config)));
        info!("Skills registered: {:?}", registry.list_skills());
        registry
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
        self.skills.iter().map(|s| s.key().clone()).collect()
    }

    pub async fn execute_skill(
        &self,
        call: &ToolCall,
        exec_ctx: ExecutionContext,
        nc_adapter: Option<Arc<NapCatAdapter>>,
    ) -> String {
        if let Some(skill) = self.get(&call.name) {
            let ts_adapter = Some(exec_ctx.adapter.clone());
            let unified_ctx = UnifiedExecutionContext::from_ts(&exec_ctx)
                .with_cross_adapters(ts_adapter, nc_adapter);

            let args = call.arguments.clone();
            let result = match skill.execute_unified(args.clone(), &unified_ctx).await {
                Ok(val) => Ok(val),
                Err(unified_err) => {
                    debug!(skill = %call.name, error = %unified_err, "Falling back to TS execution");
                    skill.execute(args, &exec_ctx).await
                }
            };

            match result {
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
        self.skills
            .iter()
            .filter(|s| {
                allowed_skills.contains(&"*".to_string())
                    || allowed_skills.contains(&s.key().clone())
            })
            .map(|s| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": s.name(),
                        "description": s.description(),
                        "parameters": s.parameters()
                    }
                })
            })
            .collect()
    }
}
