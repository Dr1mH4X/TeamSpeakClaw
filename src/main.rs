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
use tracing::info;

use crate::cli::Args;
use crate::config::AppConfig;
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::skills::SkillRegistry;

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
    let registry = Arc::new(SkillRegistry::with_defaults(config.clone()));
    let llm = Arc::new(LlmEngine::new(config.clone()));

    crate::adapter::run(config, prompts, gate, registry, llm).await
}
