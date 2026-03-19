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
    adapter::TsAdapter, audit::AuditLog, cache::ClientCache, config::AppConfig, llm::LlmEngine,
    permission::PermissionGate, router::EventRouter, skills::SkillRegistry,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env if present
    // let _ = dotenvy::dotenv(); // dotenvy not in cargo.toml, skipping or need to add

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
    adapter
        .set_nickname(&config.load().teamspeak.bot_nickname)
        .await?;

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
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .compact()
        .init();
}
