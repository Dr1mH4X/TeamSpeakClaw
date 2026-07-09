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

#[tokio::main]
async fn main() -> Result<()> {
    cli::print_banner();

    let args = Args::parse();
    let (cfg, acl_config, prompts_config) = AppConfig::load_all()?;
    let _guard = crate::log::init_tracing(&args.log_level, &cfg.logging);

    info!("Starting TeamSpeakClaw v{}", env!("CARGO_PKG_VERSION"));

    let config = Arc::new(cfg);
    let gate = Arc::new(PermissionGate::new(acl_config));
    let prompts = Arc::new(prompts_config);
    let registry = Arc::new(SkillRegistry::with_defaults(&config));
    let llm = Arc::new(LlmEngine::new(config.clone()));

    let adapter = TsAdapter::connect(config.clone()).await?;

    let nc_adapter = adapter::napcat::connect_if_enabled(config.clone()).await?;

    let ts_router = EventRouter::new_with_clients(
        config.clone(),
        prompts.clone(),
        adapter.clone(),
        gate.clone(),
        llm.clone(),
        registry.clone(),
        nc_adapter.clone(),
    );

    let headless = adapter::headless::Runtime::start(
        config.clone(),
        prompts.clone(),
        gate.clone(),
        llm.clone(),
        registry.clone(),
        adapter.clone(),
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
