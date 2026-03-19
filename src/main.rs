use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::Arc;
use tracing::{error, info, warn};

mod adapter;
mod audit;
mod cache;
mod config;
mod error;
mod llm;
mod permission;
mod router;
mod skills;

use crate::skills::{
    communication::{PokeClient, SendPrivateMsg},
    information::GetClientList,
    moderation::{BanClient, KickClient},
    SkillRegistry,
};
use crate::{
    adapter::TsAdapter, audit::AuditLog, cache::ClientCache, config::AppConfig, llm::LlmEngine,
    permission::PermissionGate, router::EventRouter,
};

#[tokio::main]
async fn main() -> Result<()> {
    // 如存在则加载 .env
    // let _ = dotenvy::dotenv(); // Cargo.toml 中未包含 dotenvy，先跳过（如需使用请添加依赖）

    // 初始化 tracing 日志
    let cfg = AppConfig::load("config/settings.toml")?;
    init_tracing(&cfg);

    info!("Starting TeamSpeakClaw v{}", env!("CARGO_PKG_VERSION"));

    // 共享配置（支持热重载）
    let config = Arc::new(ArcSwap::new(Arc::new(cfg)));

    // 基础设施组件
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

    // LLM 引擎
    let llm = Arc::new(LlmEngine::new(config.clone()));

    // TS 适配器（连接、注册事件、保持心跳）
    let adapter = TsAdapter::connect(config.clone()).await?;
    adapter
        .set_nickname(&config.load().teamspeak.bot_nickname)
        .await?;

    // 启动后台缓存刷新任务
    let cache_clone = cache.clone();
    let adapter_clone = adapter.clone();
    tokio::spawn(async move {
        cache_clone.run_refresh_loop(adapter_clone).await;
    });

    // 启动配置热重载监视器
    let config_clone = config.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::config::watch_config(config_clone).await {
            error!("Config watcher error: {e}");
        }
    });

    // 主事件循环
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

    // Ctrl+C 信号处理
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

fn init_tracing(_cfg: &AppConfig) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .compact()
        .init();
}
