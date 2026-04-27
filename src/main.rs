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
use tracing::{error, info, warn};

use crate::cli::Args;
use crate::skills::SkillRegistry;
use crate::{
    adapter::TsAdapter, config::AppConfig, llm::LlmEngine, permission::PermissionGate,
    router::EventRouter,
};
use dashmap::DashMap;

#[tokio::main]
async fn main() -> Result<()> {
    execute_app().await
}

async fn execute_app() -> Result<()> {
    print_banner();

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

    let registry = Arc::new(SkillRegistry::with_defaults());
    let llm = Arc::new(LlmEngine::new(config.clone()));

    let adapter = TsAdapter::connect(config.clone()).await?;
    adapter.set_nickname(&config.bot.nickname).await?;

    let nc_adapter = connect_napcat_if_enabled(config.clone()).await?;
    let clients = Arc::new(DashMap::new());

    let headless_runtime = start_headless_if_enabled(
        config.clone(),
        prompts.clone(),
        gate.clone(),
        llm.clone(),
        registry.clone(),
        adapter.clone(),
        clients.clone(),
    );

    let headless_channel =
        if config.headless.enabled && config.headless.tts.enabled && config.headless.tts.always_tts
        {
            let endpoint = format!("http://{}", crate::adapter::headless::INTERNAL_GRPC_ADDR);
            match tonic::transport::Channel::from_shared(endpoint)?
                .connect()
                .await
            {
                Ok(ch) => Some(ch),
                Err(e) => {
                    warn!("Failed to connect to headless gRPC service: {e}");
                    None
                }
            }
        } else {
            None
        };

    let ts_router = EventRouter::new_with_clients(
        config.clone(),
        prompts.clone(),
        adapter.clone(),
        gate.clone(),
        llm.clone(),
        registry.clone(),
        clients.clone(),
        nc_adapter.clone(),
        headless_channel.clone(),
    );

    let run_result = run_routers(
        config.clone(),
        prompts,
        gate,
        llm,
        registry,
        adapter.clone(),
        ts_router,
        nc_adapter,
        headless_channel,
    )
    .await;

    headless_runtime.shutdown().await;

    if let Err(e) = adapter.quit().await {
        error!("Failed to send quit command: {}", e);
    }

    run_result
}

async fn connect_napcat_if_enabled(
    config: Arc<AppConfig>,
) -> Result<Option<Arc<crate::adapter::napcat::NapCatAdapter>>> {
    if config.napcat.enabled {
        let nc = crate::adapter::napcat::NapCatAdapter::connect(config.napcat.clone()).await?;
        Ok(Some(nc))
    } else {
        Ok(None)
    }
}

struct HeadlessRuntime {
    shutdown: tokio_util::sync::CancellationToken,
    service_handle: Option<tokio::task::JoinHandle<()>>,
    bridge_handle: Option<tokio::task::JoinHandle<()>>,
}

impl HeadlessRuntime {
    fn disabled() -> Self {
        Self {
            shutdown: tokio_util::sync::CancellationToken::new(),
            service_handle: None,
            bridge_handle: None,
        }
    }

    async fn shutdown(self) {
        self.shutdown.cancel();

        if let Some(handle) = self.bridge_handle {
            info!("Shutting down headless LLM bridge...");
            let _ = handle.await;
        }

        if let Some(handle) = self.service_handle {
            info!("Shutting down headless voice service...");
            let _ = handle.await;
        }
    }
}

fn start_headless_if_enabled(
    config: Arc<AppConfig>,
    prompts: Arc<crate::config::PromptsConfig>,
    gate: Arc<crate::permission::PermissionGate>,
    llm: Arc<crate::llm::LlmEngine>,
    registry: Arc<crate::skills::SkillRegistry>,
    ts_adapter: Arc<crate::adapter::TsAdapter>,
    ts_clients: Arc<dashmap::DashMap<u32, crate::router::ClientInfo>>,
) -> HeadlessRuntime {
    if !config.headless.enabled {
        return HeadlessRuntime::disabled();
    }

    let shutdown = tokio_util::sync::CancellationToken::new();
    let hl_runtime = crate::adapter::headless::HeadlessRuntimeConfig {
        ts3_host: config.headless.ts3_host.clone(),
        ts3_port: config.headless.ts3_port,
        nickname: config.bot.nickname.clone(),
        server_password: config.headless.server_password.clone(),
        channel_password: config.headless.channel_password.clone(),
        channel_path: config.headless.channel_path.clone(),
        channel_id: config.headless.channel_id.clone(),
        bot_respond_to_private: config.bot.respond_to_private,
        bot_default_reply_mode: config.bot.default_reply_mode.clone(),
        bot_trigger_prefixes: config.bot.trigger_prefixes.clone(),
    };

    let shutdown_for_service = shutdown.clone();
    let service_handle = Some(tokio::spawn(async move {
        if let Err(e) = crate::adapter::headless::run(hl_runtime, shutdown_for_service).await {
            error!("headless service failed: {}", e);
        }
    }));

    info!("Headless voice service enabled");

    let bridge_config = config.clone();
    let bridge_prompts = prompts.clone();
    let bridge_gate = gate.clone();
    let bridge_llm = llm.clone();
    let bridge_registry = registry.clone();
    let bridge_ts_adapter = ts_adapter.clone();
    let bridge_ts_clients = ts_clients.clone();
    let shutdown_for_bridge = shutdown.clone();
    let bridge_handle = Some(tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_for_bridge.cancelled() => {
                    break;
                }
                run_result = crate::router::HeadlessLlmBridge::new(
                    bridge_config.clone(),
                    bridge_prompts.clone(),
                    bridge_gate.clone(),
                    bridge_llm.clone(),
                    bridge_registry.clone(),
                    bridge_ts_adapter.clone(),
                    bridge_ts_clients.clone(),
                ).run() => {
                    if let Err(e) = run_result {
                        error!("headless LLM bridge failed: {}", e);
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        continue;
                    }
                    break;
                }
            }
        }
    }));

    HeadlessRuntime {
        shutdown,
        service_handle,
        bridge_handle,
    }
}

async fn run_routers(
    config: Arc<AppConfig>,
    prompts: Arc<crate::config::PromptsConfig>,
    gate: Arc<crate::permission::PermissionGate>,
    llm: Arc<crate::llm::LlmEngine>,
    registry: Arc<crate::skills::SkillRegistry>,
    adapter: Arc<crate::adapter::TsAdapter>,
    ts_router: crate::router::EventRouter,
    nc_adapter: Option<Arc<crate::adapter::napcat::NapCatAdapter>>,
    headless_channel: Option<tonic::transport::Channel>,
) -> Result<()> {
    if let Some(nc_adapter) = nc_adapter {
        let nc_router = crate::router::NcRouter::new_with_ts(
            config,
            prompts,
            nc_adapter,
            gate,
            llm,
            registry,
            Some(adapter),
            Some(ts_router.clients.clone()),
            headless_channel,
        );
        let nc_future = tokio::spawn(async move { nc_router.run().await });

        info!("Bot ready. Listening for TS + NapCat events.");

        tokio::select! {
            res = ts_router.run() => map_ts_router_result(res),
            res = nc_future => map_nc_router_result(res),
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C, shutting down...");
                Ok(())
            }
        }
    } else {
        info!("NapCat adapter disabled, running in TeamSpeak-only mode");
        info!("Bot ready. Listening for TeamSpeak events.");

        tokio::select! {
            res = ts_router.run() => map_ts_router_result(res),
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C, shutting down...");
                Ok(())
            }
        }
    }
}

fn map_ts_router_result(res: Result<()>) -> Result<()> {
    match res {
        Ok(()) => {
            warn!("TS Event router exited unexpectedly");
            Err(anyhow::anyhow!("TS Event router exited unexpectedly"))
        }
        Err(e) => {
            error!("TS Event router exited with error: {}", e);
            Err(e)
        }
    }
}

fn map_nc_router_result(res: Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match res {
        Ok(Ok(())) => {
            warn!("NC router exited unexpectedly");
            Err(anyhow::anyhow!("NC router exited unexpectedly"))
        }
        Ok(Err(e)) => {
            error!("NC router error: {e}");
            Err(e)
        }
        Err(e) => {
            error!("NC router task panicked: {e}");
            Err(anyhow::anyhow!("NC router panicked"))
        }
    }
}

fn print_banner() {
    let banner = r#"
    ░▒▓████████▓▒░▒▓███████▓▒░░▒▓██████▓▒░░▒▓█▓▒░       ░▒▓██████▓▒░░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░  ░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░  ░▒▓█▓▒░      ░▒▓█▓▒░      ░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░   ░▒▓██████▓▒░░▒▓█▓▒░      ░▒▓█▓▒░      ░▒▓████████▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░         ░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░         ░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░      ░▒▓█▓▒░░▒▓█▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█▓▒░
       ░▒▓█▓▒░  ░▒▓███████▓▒░ ░▒▓██████▓▒░░▒▓████████▓▒░▒▓█▓▒░░▒▓█▓▒░░▒▓█████████████▓▒░

                                                                                           "#;

    println!("{}", banner);
    println!(" 版本: v{}", env!("CARGO_PKG_VERSION"));
    println!(" GitHub: https://github.com/Dr1mH4X/TeamSpeakClaw");
    println!("{:-<86}", "");
}
