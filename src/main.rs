use anyhow::Result;
use arc_swap::ArcSwap;
use clap::Parser;
use std::sync::Arc;
use tracing::{error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;

mod adapter;
mod cache;
mod cli;
mod config;
mod llm;
mod permission;
mod router;
mod skills;

use crate::cli::Args;
use crate::skills::SkillRegistry;
use crate::{
    adapter::TsAdapter, cache::ClientCache, config::AppConfig, llm::LlmEngine,
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
    let _guard = init_tracing(&args.log_level);

    info!("Starting TeamSpeakClaw v{}", env!("CARGO_PKG_VERSION"));

    let config = Arc::new(ArcSwap::new(Arc::new(cfg)));

    // 4. 初始化组件
    let cache = Arc::new(ClientCache::new(config.clone()));
    let acl_config = crate::config::AclConfig::load("config/acl.toml")?;
    let prompts_config = crate::config::PromptsConfig::load("config/prompts.toml")?;
    let gate = Arc::new(PermissionGate::new(acl_config));
    let prompts = Arc::new(prompts_config);

    let registry = Arc::new(SkillRegistry::with_defaults());

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

    // 7. 事件路由循环
    let router = EventRouter::new(
        config,
        prompts,
        adapter.clone(),
        cache,
        gate,
        llm,
        registry,
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

fn init_tracing(console_level: &str) -> WorkerGuard {
    use std::path::PathBuf;
    use tracing_subscriber::{
        fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
    };

    let console_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(console_level));

    let console_layer = fmt::layer()
        .with_target(true)
        .compact()
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
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)?;
                inner.file = Some(file);
                inner.date_key = today;
            }
            Ok(())
        }
    }

    impl Write for DailyFileAppender {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut inner = self.inner.lock().map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "日志锁被污染")
            })?;
            Self::ensure_open(&mut inner, &self.dir, &self.prefix)?;
            inner.file.as_mut().unwrap().write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            let mut inner = self.inner.lock().map_err(|_| {
                io::Error::new(io::ErrorKind::Other, "日志锁被污染")
            })?;
            if let Some(ref mut file) = inner.file {
                file.flush()?;
            }
            Ok(())
        }
    }
}
