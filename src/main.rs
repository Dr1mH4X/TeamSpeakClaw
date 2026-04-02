use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;

mod adapter;
mod cli;
mod config;
mod llm;
mod permission;
mod router;
mod skills;

use crate::cli::Args;
use crate::skills::SkillRegistry;
use crate::{
    adapter::TsAdapter, config::AppConfig, llm::LlmEngine, permission::PermissionGate,
    router::EventRouter,
};
use dashmap::DashMap;

#[tokio::main]
async fn main() -> Result<()> {
    // 1. 打印 Banner
    print_banner();

    // 2. 解析参数
    let args = Args::parse();

    // 3. 初始化配置与日志
    let config_dir = crate::config::config_dir();
    let cfg = AppConfig::load(config_dir.join("settings.toml"))?;
    let _guard = init_tracing(&args.log_level, &cfg.logging.file_level);

    info!("Starting TeamSpeakClaw v{}", env!("CARGO_PKG_VERSION"));

    let config = Arc::new(cfg);

    // 4. 初始化组件
    let acl_config = crate::config::AclConfig::load(config_dir.join("acl.toml"))?;
    let prompts_config = crate::config::PromptsConfig::load(config_dir.join("prompts.toml"))?;
    let gate = Arc::new(PermissionGate::new(acl_config));
    let prompts = Arc::new(prompts_config);

    let registry = Arc::new(SkillRegistry::with_defaults());

    let llm = Arc::new(LlmEngine::new(config.clone()));

    // 5. 连接服务
    let adapter = TsAdapter::connect(config.clone()).await?;
    adapter
        .set_nickname(&config.serverquery.bot_nickname)
        .await?;

    // 6. NapCat 适配器（可选，需要在 ts_router 之前创建）
    use crate::adapter::napcat::NapCatAdapter;
    use crate::router::NcRouter;

    let nc_adapter: Option<Arc<NapCatAdapter>> = if config.napcat.enabled {
        let nc = NapCatAdapter::connect(config.napcat.clone()).await?;
        Some(nc)
    } else {
        None
    };

    // 7. Headless 适配器（可选）
    let headless_shutdown = tokio_util::sync::CancellationToken::new();
    if config.headless.enabled {
        let hl_runtime = crate::adapter::headless::HeadlessRuntimeConfig {
            grpc_addr: config.headless.grpc_addr.clone(),
            ts3_host: config.headless.ts3_host.clone(),
            ts3_port: config.headless.ts3_port,
            nickname: config.headless.nickname.clone(),
            server_password: config.headless.server_password.clone(),
            channel_password: config.headless.channel_password.clone(),
            channel_path: config.headless.channel_path.clone(),
            channel_id: config.headless.channel_id.clone(),
            identity: config.headless.identity.clone(),
            identity_file: config.headless.identity_file.clone(),
            avatar_dir: config.headless.avatar_dir.clone(),
            voice_state_file: config.headless.voice_state_file.clone(),
            sq_host: config.serverquery.host.clone(),
            sq_port: config.serverquery.port,
            sq_user: config.serverquery.login_name.clone(),
            sq_password: config.serverquery.login_pass.clone(),
            sq_sid: config.serverquery.server_id,
        };
        let hl_shutdown = headless_shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::adapter::headless::run(hl_runtime, hl_shutdown).await {
                error!("headless service failed: {}", e);
            }
        });
        info!("Headless voice service enabled");
    }

    // 8. 事件路由循环（TeamSpeak）
    let ts_router = EventRouter::new_with_clients(
        config.clone(),
        prompts.clone(),
        adapter.clone(),
        gate.clone(),
        llm.clone(),
        registry.clone(),
        Arc::new(DashMap::new()),
        nc_adapter.clone(),
    );

    let run_result: Result<()> = if let Some(nc_adapter) = nc_adapter {
        let nc_router = NcRouter::new_with_ts(
            config.clone(),
            prompts.clone(),
            nc_adapter,
            gate.clone(),
            llm.clone(),
            registry.clone(),
            Some(adapter.clone()),
            Some(ts_router.clients.clone()),
        );
        let nc_future = tokio::spawn(async move { nc_router.run().await });

        info!("Bot ready. Listening for TS + NapCat events.");

        tokio::select! {
            res = ts_router.run() => {
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
            res = nc_future => {
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
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C, shutting down...");
                Ok(())
            }
        }
    } else {
        info!("NapCat adapter disabled, running in TeamSpeak-only mode");
        info!("Bot ready. Listening for TeamSpeak events.");

        tokio::select! {
            res = ts_router.run() => {
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
            _ = tokio::signal::ctrl_c() => {
                info!("Received Ctrl+C, shutting down...");
                Ok(())
            }
        }
    };

    if let Err(e) = adapter.quit().await {
        error!("Failed to send quit command: {}", e);
    }

    run_result
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

fn init_tracing(console_level: &str, file_level: &str) -> WorkerGuard {
    use std::path::PathBuf;
    use tracing_subscriber::{
        fmt::{self, time::LocalTime},
        layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
    };

    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("{console_level},russh::client=off,russh=off")));

    let console_layer = fmt::layer()
        .with_target(true)
        .compact()
        .with_timer(LocalTime::rfc_3339())
        .with_filter(console_filter);

    // 使用可执行文件所在目录作为日志根目录
    let log_dir: PathBuf = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("logs");
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = daily_file::DailyFileAppender::new(log_dir, "tsclaw");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_filter = EnvFilter::new(file_level);

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_timer(LocalTime::rfc_3339())
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    guard
}

/// 按日滚动日志文件，文件名格式: `{prefix}-{yyyy-MM-dd}.log`
mod daily_file {
    use chrono::Local;
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::Mutex;

    pub struct DailyFileAppender {
        dir: PathBuf,
        prefix: String,
        inner: Mutex<Inner>,
    }

    struct Inner {
        file: Option<File>,
        date_key: String, // "2026-03-24"
    }

    impl DailyFileAppender {
        pub fn new(dir: PathBuf, prefix: &str) -> Self {
            Self {
                dir,
                prefix: prefix.to_string(),
                inner: Mutex::new(Inner {
                    file: None,
                    date_key: String::new(),
                }),
            }
        }

        fn file_path(dir: &PathBuf, prefix: &str, date_key: &str) -> PathBuf {
            dir.join(format!("{prefix}-{date_key}.log"))
        }

        fn ensure_open(inner: &mut Inner, dir: &PathBuf, prefix: &str) -> io::Result<()> {
            let today = Local::now().format("%Y-%m-%d").to_string();
            if inner.date_key != today || inner.file.is_none() {
                let path = Self::file_path(dir, prefix, &today);
                let file = OpenOptions::new().create(true).append(true).open(path)?;
                inner.file = Some(file);
                inner.date_key = today;
            }
            Ok(())
        }
    }

    impl Write for DailyFileAppender {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "日志锁被污染"))?;
            Self::ensure_open(&mut inner, &self.dir, &self.prefix)?;
            inner.file.as_mut().unwrap().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "日志锁被污染"))?;
            if let Some(ref mut file) = inner.file {
                file.flush()?;
            }
            Ok(())
        }
    }
}
