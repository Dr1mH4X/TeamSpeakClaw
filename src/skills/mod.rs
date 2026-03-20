pub mod communication;
pub mod information;
pub mod moderation;
pub mod music;

use crate::adapter::UnifiedAdapter;
use crate::cache::ClientCache;
use crate::error::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;
use std::sync::Arc;

pub struct ExecutionContext {
    pub adapter: Arc<UnifiedAdapter>,
    pub cache: Arc<ClientCache>,
    #[allow(dead_code)]
    pub caller_id: u32,
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
    pub fn register(&self, skill: Box<dyn Skill>) {
        self.skills.insert(skill.name().to_string(), skill);
    }

    pub fn get(&self, name: &str) -> Option<impl std::ops::Deref<Target = Box<dyn Skill>> + '_> {
        self.skills.get(name)
    }

    #[allow(dead_code)]
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
