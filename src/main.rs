mod adapter;
mod cli;
mod config;
mod llm;
mod log;
mod permission;
mod prompt;
mod router;
mod tools;
mod ui;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::cli::Args;
use crate::config::AppConfig;
use crate::llm::LlmEngine;
use crate::permission::PermissionGate;
use crate::tools::ToolRegistry;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let ui = ui::Ui::new(args.ui);
    ui.print_line(&cli::banner(ui.rich()));
    let result = run(args, ui.clone()).await;
    ui.shutdown(result.is_ok());
    result
}

async fn run(args: Args, ui: ui::Ui) -> Result<()> {
    let (cfg, acl_config) = AppConfig::load_all()?;
    let _guard = crate::log::init_tracing(&args.log_level, &cfg.logging, &ui);

    info!("Starting TeamSpeakClaw v{}", env!("CARGO_PKG_VERSION"));
    cfg.log_voice_input_conflicts();

    let config = Arc::new(cfg);
    let gate = Arc::new(PermissionGate::new(acl_config));
    let voice_audio = crate::tools::VoiceAudioHandles::default();
    let registry = Arc::new(ToolRegistry::with_defaults(
        config.clone(),
        voice_audio.clone(),
    ));
    ui.step_ok("tools", format!("{} enabled", registry.list_tools().len()));
    let llm = Arc::new(LlmEngine::new(config.clone())?);

    let shutdown = CancellationToken::new();
    let run = crate::adapter::run(
        config,
        gate,
        registry,
        llm,
        shutdown.clone(),
        voice_audio,
        ui.clone(),
    );
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
