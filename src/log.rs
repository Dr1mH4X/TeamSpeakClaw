use chrono::Local;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    fmt::{self, format::Writer, time::FormatTime},
    layer::SubscriberExt,
    util::SubscriberInitExt,
    EnvFilter, Layer,
};

use crate::config::LogConfig;

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
            .is_some_and(|n| n.starts_with("tsclaw-") && n.ends_with(".log"))
        {
            continue;
        }
        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                let age =
                    now.signed_duration_since::<chrono::Local>(
                        chrono::DateTime::<chrono::Local>::from(modified),
                    );
                if age.num_days() >= max_days as i64 {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

const TIMESTAMP_FMT: &str = "%Y-%m-%d %H:%M:%S";

struct CustomTime;

impl FormatTime for CustomTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format(TIMESTAMP_FMT))
    }
}

/// 第三方 crate 在 debug 级别下刷屏的噪声源：ONNX 运行时（tract）、DNS 解析器（hickory）
/// 与 HTTP 栈的逐帧日志。这里的 target 用模块路径（下划线），与 `module_path!()` 一致。
const NOISE_DIRECTIVES: &[&str] = &[
    "h2=off",
    "hickory_proto=off",
    "hickory_resolver=off",
    "hyper::client::connect=off",
    "hyper::proto::h2=off",
    "hyper_util::client=off",
    "reqwest::connect=off",
    "tower::buffer::worker=off",
    "tract_core=off",
    "tract_data=off",
    "tract_extra=off",
    "tract_hir=off",
    "tract_linalg=off",
    "tract_nnef=off",
    "tract_onnx=off",
    "tract_onnx_opl=off",
    "tract_transformers=off",
];

/// 在给定级别的 filter 上叠加噪声抑制；`off` 比任何级别都更具体，`RUST_LOG` 也无法重新打开
fn with_noise_off(filter: EnvFilter) -> EnvFilter {
    NOISE_DIRECTIVES.iter().fold(filter, |filter, directive| {
        filter.add_directive(directive.parse().expect("valid noise directive"))
    })
}

pub fn init_tracing(console_level: &str, log_cfg: &LogConfig) -> WorkerGuard {
    let console_filter = with_noise_off(
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(console_level)),
    );

    let console_layer = fmt::layer()
        .with_target(true)
        .compact()
        .with_timer(CustomTime)
        .with_filter(console_filter);

    let log_dir: PathBuf = crate::config::exe_dir().join("logs");
    let _ = std::fs::create_dir_all(&log_dir);

    cleanup_old_logs(&log_dir, log_cfg.max_log_days);

    let file_appender = DailyFileAppender::new(log_dir.clone(), "tsclaw");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_filter = with_noise_off(EnvFilter::new(&log_cfg.file_level));

    let file_layer = fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_timer(CustomTime)
        .with_filter(file_filter);

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    // 定期清理旧日志
    {
        let dir = log_dir;
        let days = log_cfg.max_log_days;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
                cleanup_old_logs(&dir, days);
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

    fn file_path(dir: &Path, prefix: &str, date_key: &str) -> PathBuf {
        dir.join(format!("{prefix}-{date_key}.log"))
    }

    fn ensure_open(inner: &mut Inner, dir: &Path, prefix: &str) -> io::Result<()> {
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
            .map_err(|_| io::Error::other("Log lock poisoned"))?;
        Self::ensure_open(&mut inner, &self.dir, &self.prefix)?;
        inner.file.as_mut().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("Log lock poisoned"))?;
        if let Some(ref mut file) = inner.file {
            file.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::with_noise_off;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer};

    struct CaptureTargets(Arc<Mutex<Vec<&'static str>>>);

    impl<S: tracing::Subscriber> Layer<S> for CaptureTargets {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0.lock().unwrap().push(event.metadata().target());
        }
    }

    #[test]
    fn noise_filter_silences_third_party_debug_and_keeps_app_debug() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(with_noise_off(EnvFilter::new("debug")))
            .with(CaptureTargets(captured.clone()));

        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "tract_core::optim::change_axes", "onnx noise");
            tracing::debug!(target: "tract_hir::infer::analyser", "onnx noise");
            tracing::debug!(target: "hickory_proto::udp::udp_client_stream", "dns noise");
            tracing::debug!(target: "teamspeakclaw::adapter::headless::speech", "kept");
        });

        let targets = captured.lock().unwrap().clone();
        assert_eq!(
            targets,
            vec!["teamspeakclaw::adapter::headless::speech"],
            "第三方 debug 应被抑制，本进程 debug 应保留"
        );
    }
}
