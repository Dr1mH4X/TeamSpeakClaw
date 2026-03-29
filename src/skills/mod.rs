pub mod communication;
pub mod information;
pub mod moderation;
pub mod music;

use crate::adapter::TsAdapter;
use crate::config::{AppConfig, ErrorPrompts};
use crate::permission::PermissionGate;
use crate::router::ClientInfo;
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;
use std::sync::Arc;

pub struct ExecutionContext<'a> {
    pub adapter: Arc<TsAdapter>,
    pub clients: &'a DashMap<u32, ClientInfo>,
    pub caller_id: u32,
    pub caller_groups: Vec<u32>,
    pub caller_channel_group_id: u32,
    pub gate: Arc<PermissionGate>,
    pub config: Arc<AppConfig>,
    pub error_prompts: &'a ErrorPrompts,
}

#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value>;
}

#[derive(Default)]
pub struct SkillRegistry {
    skills: DashMap<String, Box<dyn Skill>>,
}

impl SkillRegistry {
    pub fn with_defaults() -> Self {
        use communication::{PokeClient, SendMessage};
        use information::{GetClientInfo, GetClientList};
        use moderation::{BanClient, KickClient};
        use music::MusicControl;
        use tracing::info;

        let registry = Self::default();
        registry.register(Box::new(PokeClient));
        registry.register(Box::new(SendMessage));
        registry.register(Box::new(KickClient));
        registry.register(Box::new(BanClient));
        registry.register(Box::new(GetClientList));
        registry.register(Box::new(GetClientInfo));
        registry.register(Box::new(MusicControl));
        info!("已注册技能: {:?}", registry.list_skills());
        registry
    }

    pub fn register(&self, skill: Box<dyn Skill>) {
        self.skills.insert(skill.name().to_string(), skill);
    }

    pub fn get(&self, name: &str) -> Option<impl std::ops::Deref<Target = Box<dyn Skill>> + '_> {
        self.skills.get(name)
    }

    pub fn list_skills(&self) -> Vec<String> {
        self.skills.iter().map(|s| s.key().clone()).collect()
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
