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
use tokio_util::sync::CancellationToken;
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
    let llm = Arc::new(LlmEngine::new(config.clone())?);

    let shutdown = CancellationToken::new();
    let run = crate::adapter::run(config, prompts, gate, registry, llm, shutdown.clone());
    tokio::pin!(run);

    tokio::select! {
        result = &mut run => result,
        signal_result = wait_for_shutdown_signal() => {
            shutdown.cancel();
            match signal_result {
                Ok(()) => {
                    info!("Shutdown signal received");
                    run.await
                }
                Err(error) => {
                    let _ = run.await;
                    Err(error)
                }
            }
        }
    }
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() -> Result<()> {
    use tokio::signal::unix::SignalKind;

    let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.map_err(Into::into),
        signal = sigterm.recv() => signal
            .map(|_| ())
            .ok_or_else(|| anyhow::anyhow!("SIGTERM signal stream closed")),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await.map_err(Into::into)
}
