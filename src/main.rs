use anyhow::Result;
use arc_swap::ArcSwap;
use clap::Parser;
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;

mod adapter;
mod audit;
mod cache;
mod cli;
mod config;
mod error;
mod llm;
mod permission;
mod router;
mod skills;

use crate::cli::Args;
use crate::skills::{
    communication::{PokeClient, SendPrivateMsg},
    information::GetClientList,
    moderation::{BanClient, KickClient},
    music::MusicControl,
    SkillRegistry,
};
use crate::{
    adapter::TsAdapter, audit::AuditLog, cache::ClientCache, config::AppConfig, llm::LlmEngine,
    permission::PermissionGate, router::EventRouter,
};

#[tokio::main]
async fn main() -> Result<()> {
    // 1. 打印 Banner
    print_banner();

    // 2. 解析参数
    let args = Args::parse();

    if let Some(action) = args.config {
        return crate::cli::handle_config_action(action);
    }

    // 3. 初始化配置与日志
    let cfg = AppConfig::load("config/settings.toml")?;
    let _guard = init_tracing(&cfg, &args.log_level);

    info!("Starting TeamSpeakClaw v{}", env!("CARGO_PKG_VERSION"));

    let config = Arc::new(ArcSwap::new(Arc::new(cfg)));

    // 4. 初始化组件
    let audit = Arc::new(AuditLog::new(&config.load().audit)?);
    let cache = Arc::new(ClientCache::new(config.clone()));
    let acl_config = crate::config::AclConfig::load("config/acl.toml")?;
    let prompts_config = crate::config::PromptsConfig::load("config/prompts.toml")?;
    let gate = Arc::new(PermissionGate::new(acl_config));
    let prompts = Arc::new(prompts_config);

    let registry = Arc::new(SkillRegistry::default());
    registry.register(Box::new(PokeClient));
    registry.register(Box::new(SendPrivateMsg));
    registry.register(Box::new(KickClient));
    registry.register(Box::new(BanClient));
    registry.register(Box::new(GetClientList));
    registry.register(Box::new(MusicControl));

    let llm = Arc::new(LlmEngine::new(config.clone()));

    // 5. 连接服务
    let adapter = TsAdapter::connect(config.clone()).await?;
    adapter
        .set_nickname(&config.load().teamspeak.bot_nickname)
        .await?;

    // 6. 后台任务
    let cache_clone = cache.clone();
    let adapter_clone = adapter.clone();
    tokio::spawn(async move {
        cache_clone.run_refresh_loop(adapter_clone).await;
    });

    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::config::watch_config(config_clone).await {
            error!("Config watcher error: {e}");
        }
    });

    // 7. 事件路由循环
    let router = EventRouter::new(
        config,
        prompts,
        adapter.clone(),
        cache,
        gate,
        llm,
        registry,
        audit,
    );

    info!("Bot ready. Listening for events.");

    tokio::select! {
        res = router.run() => {
            if let Err(e) = res {
                error!("Event router exited with error: {}", e);
            } else {
                warn!("Event router exited unexpectedly");
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down...");
        }
    }

    if let Err(e) = adapter.quit().await {
        error!("Failed to send quit command: {}", e);
    }

    Ok(())
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

fn init_tracing(_cfg: &AppConfig, console_level: &str) -> WorkerGuard {
    use tracing_subscriber::{
        fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
    };

    let console_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(console_level));

    let console_layer = fmt::layer()
        .with_target(true)
        .compact()
        .with_filter(console_filter);

    let file_appender = tracing_appender::rolling::daily("logs", "teamspeakclaw.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_filter = EnvFilter::new("trace");

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    guard
}
