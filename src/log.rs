use chrono::Local;
use slog::Drain;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_slog::TracingSlogDrain;
use tracing_subscriber::{
    fmt::{self, time::LocalTime},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter, Layer,
};

fn cleanup_old_logs(log_dir: &PathBuf, max_days: u32) {
    let now = chrono::Local::now();
    let entries = match fs::read_dir(log_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|n| n.to_str())
            .map_or(false, |n| n.starts_with("tsclaw-") && n.ends_with(".log"))
        {
            continue;
        }
        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                let age = now.signed_duration_since::<chrono::Local>(
                    chrono::DateTime::<chrono::Local>::from(modified),
                );
                if age.num_days() >= max_days as i64 {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

pub fn init_tracing(console_level: &str, file_level: &str, max_log_days: u32) -> WorkerGuard {
    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(console_level))
        .add_directive("russh::client=off".parse().unwrap())
        .add_directive("russh=off".parse().unwrap())
        .add_directive("tsclientlib=off".parse().unwrap())
        .add_directive("tsproto=off".parse().unwrap())
        .add_directive("tsproto-packets=off".parse().unwrap())
        .add_directive("ts-bookkeeping=off".parse().unwrap())
        .add_directive("h2=off".parse().unwrap());

    let console_layer = fmt::layer()
        .with_target(true)
        .compact()
        .with_timer(LocalTime::rfc_3339())
        .with_filter(console_filter);

    let log_dir: PathBuf = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("logs");
    let _ = std::fs::create_dir_all(&log_dir);

    cleanup_old_logs(&log_dir, max_log_days);

    let file_appender = DailyFileAppender::new(log_dir.clone(), "tsclaw");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_filter = EnvFilter::new(file_level).add_directive("h2=off".parse().unwrap());

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_timer(LocalTime::rfc_3339())
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    // Bridge slog (used by tsclientlib) to tracing
    let slog_logger = slog::Logger::root(TracingSlogDrain.fuse(), slog::o!());
    let _slog_guard = slog_scope::set_global_logger(slog_logger);
    std::mem::forget(_slog_guard);

    // Periodic cleanup
    {
        let dir = log_dir;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
                cleanup_old_logs(&dir, max_log_days);
            }
        });
    }

    guard
}

pub struct DailyFileAppender {
    dir: PathBuf,
    prefix: String,
    inner: Mutex<Inner>,
}

struct Inner {
    file: Option<File>,
    date_key: String,
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
