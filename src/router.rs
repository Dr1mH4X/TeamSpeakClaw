use crate::adapter::TsAdapter;
use crate::audit::AuditLog;
use crate::cache::ClientCache;
use crate::config::AppConfig;
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::skills::SkillRegistry;
use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::Arc;

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
        Self {
            config,
            adapter,
            cache,
            gate,
            llm,
            registry,
            audit,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // TODO: Implement event loop
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    }
}
