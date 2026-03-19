use crate::adapter::{TsAdapter, TsEvent, TextMessageEvent, TextMessageTarget};
use crate::audit::AuditLog;
use crate::cache::ClientCache;
use crate::config::AppConfig;
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::skills::SkillRegistry;
use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::Arc;
use tracing::{debug, info};

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
        let mut rx = self.adapter.subscribe();
        
        while let Ok(event) = rx.recv().await {
            match event {
                TsEvent::TextMessage(msg) => {
                    self.handle_message(msg).await;
                }
                TsEvent::ClientEnterView(e) => {
                    self.cache.update_client(e.clid, e.cldbid, e.client_nickname, e.client_server_groups);
                }
                TsEvent::ClientLeftView(e) => {
                    self.cache.remove_client(e.clid);
                }
                _ => {}
            }
        }
        Ok(())
    }
    
    async fn handle_message(&self, event: TextMessageEvent) {
        // Ignore self
        if event.invoker_name == self.config.load().teamspeak.bot_nickname {
            return;
        }
        
        // Only respond to private messages or if triggered by prefix
        let is_private = event.target_mode == TextMessageTarget::Private;
        let msg_content = event.message.trim();
        let triggers = &self.config.load().bot.trigger_prefixes;
        
        let should_respond = is_private && self.config.load().bot.respond_to_private
            || triggers.iter().any(|prefix| msg_content.starts_with(prefix));
            
        if !should_respond {
            return;
        }

        info!("Handling message from {}: {}", event.invoker_name, msg_content);
        
        let groups = if let Some(client) = self.cache.get_client(event.invoker_id) {
            client.server_groups
        } else {
            debug!("Client {} not in cache, assuming default permissions", event.invoker_id);
            vec![]
        };
        
        // TODO: Call LLM
        let _ = groups;
        let _ = self.gate;
        let _ = self.llm;
        let _ = self.registry;
        let _ = self.audit;
    }
}
