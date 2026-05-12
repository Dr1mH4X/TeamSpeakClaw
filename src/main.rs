mod adapter;
mod cli;
mod config;
mod llm;
mod log;
mod permission;
mod router;
mod skills;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing::{error, info};

use crate::cli::Args;
use crate::skills::SkillRegistry;
use crate::{
    adapter::TsAdapter, config::AppConfig, llm::LlmEngine, permission::PermissionGate,
    router::EventRouter,
};
use dashmap::DashMap;

#[tokio::main]
async fn main() -> Result<()> {
    cli::print_banner();

    let args = Args::parse();
    let config_dir = crate::config::config_dir();
    let cfg = AppConfig::load(config_dir.join("settings.toml"))?;
    let _guard = crate::log::init_tracing(&args.log_level, &cfg.logging.file_level);

    info!("Starting TeamSpeakClaw v{}", env!("CARGO_PKG_VERSION"));

    let config = Arc::new(cfg);
    let acl_config = crate::config::AclConfig::load(config_dir.join("acl.toml"))?;
    let prompts_config = crate::config::PromptsConfig::load(config_dir.join("prompts.toml"))?;
    let gate = Arc::new(PermissionGate::new(acl_config));
    let prompts = Arc::new(prompts_config);
    let registry = Arc::new(SkillRegistry::with_defaults(&config.music_backend.backend));
    let llm = Arc::new(LlmEngine::new(config.clone()));

    let adapter = TsAdapter::connect(config.clone()).await?;
    adapter.set_nickname(&config.bot.nickname).await?;

    let nc_adapter = adapter::napcat::connect_if_enabled(config.clone()).await?;
    let clients = Arc::new(DashMap::new());

    let ts_router = EventRouter::new_with_clients(
        config.clone(),
        prompts.clone(),
        adapter.clone(),
        gate.clone(),
        llm.clone(),
        registry.clone(),
        clients.clone(),
        nc_adapter.clone(),
    );

    let headless = adapter::headless::Runtime::start_if_enabled(
        config.clone(),
        prompts.clone(),
        gate.clone(),
        llm.clone(),
        registry.clone(),
        adapter.clone(),
        clients,
    );

    let result = router::run_routers(
        config,
        prompts,
        gate,
        llm,
        registry,
        adapter.clone(),
        ts_router,
        nc_adapter,
    )
    .await;

    headless.shutdown().await;

    if let Err(e) = adapter.quit().await {
        error!("Failed to send quit command: {}", e);
    }

    result
}
