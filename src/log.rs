use chrono::Local;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing::level_filters::LevelFilter;
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

/// 第三方 crate 的噪声 target，用模块路径（下划线），与 `module_path!()` 一致。
///
/// 分两组：HTTP 栈逐连接/逐帧的日志一律 `off`（本 PR 之前既有，与 sink 级别无关，
/// 这批日志只在连接抖动时刷屏，任何级别下都没有保留价值）；推理（tract）与 DNS（hickory）
/// 刷屏的大多是 debug/info（本 PR 新增），抑制级别跟随 sink 的全局级别收紧，见
/// `noise_suppression_level`，真实的 warn/error 不会被静音；全局级别是 `off` 时（含
/// `RUST_LOG=""` 这种等价于全局 `off` 的情况）第三方也一并静默。
const HTTP_NOISE_TARGETS: &[&str] = &[
    "h2",
    "hyper::client::connect",
    "hyper::proto::h2",
    "hyper_util::client",
    "reqwest::connect",
    "tower::buffer::worker",
];

/// 推理（tract）与 DNS（hickory）的噪声 target（本 PR 新增）。
const INFERENCE_NOISE_TARGETS: &[&str] = &[
    "hickory_proto",
    "hickory_resolver",
    "tract_core",
    "tract_data",
    "tract_extra",
    "tract_hir",
    "tract_linalg",
    "tract_nnef",
    "tract_onnx",
    "tract_onnx_opl",
    "tract_transformers",
];

/// 从指令字符串（`--log-level`、`log_cfg.file_level` 或 `RUST_LOG` 原始值）里取全局级别。
///
/// 只认不含 `=` 的纯级别指令：`tower_http=debug` 这类 target 指令、以及空 token 都跳过
/// （`LevelFilter::from_str("")` 会把空串解析成 `ERROR`，必须先排除）。无法解析时返回 `None`，
/// 由 `noise_suppression_level` 回落 `warn`。同一字符串里出现多个全局级别时取最严的一个：
/// `EnvFilter` 对同 specificity 指令的取舍未定义，取最严才能保证抑制级别不比用户配置更宽松。
/// `LevelFilter` 的 `Ord` 按限制强度排序（`OFF < ERROR < WARN < INFO < DEBUG < TRACE`），
/// 所以 `min()` 就是最严。
fn global_level_from(directives: &str) -> Option<LevelFilter> {
    directives
        .split(',')
        .filter_map(|directive| {
            let directive = directive.trim();
            if directive.is_empty() || directive.contains('=') {
                return None;
            }
            directive.parse::<LevelFilter>().ok()
        })
        .min()
}

/// 控制台 base filter：`RUST_LOG` 合法时完全接管，否则用 `--log-level`。
///
/// 与原写法 `EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(console_level))`
/// 等价——`try_from_default_env` 就是读同一个环境变量后走 `Builder::parse`；抽成纯函数是为了让
/// 测试能按真实取值构造 base filter，`init_tracing` 也只需读一次环境变量。
fn console_base_filter(console_level: &str, rust_log: Option<&str>) -> EnvFilter {
    match rust_log {
        Some(raw) => EnvFilter::try_new(raw).unwrap_or_else(|_| EnvFilter::new(console_level)),
        None => EnvFilter::new(console_level),
    }
}

/// 某个 sink 的有效全局级别：`RUST_LOG` 合法时以它的全局指令为准（其中没有全局级别则返回
/// `None`，不再看 `--log-level`）；`RUST_LOG` 缺失或非法时退回显式配置的级别
/// （控制台 `--log-level`、文件 `log_cfg.file_level`）。
///
/// 特例：`RUST_LOG` 存在但 trim 后为空时返回 `Some(OFF)`。空串走 `Builder::parse` 会成功并产出
/// 零指令、也没有 default directive，`EnvFilter` 的 `statics` 为空，于是 `enabled` 对任何 target
/// 都返回 false——语义上就是这个 sink 全局 `off`，此时推理/DNS 噪声必须一并静默，否则会出现
/// 「base 全静默、第三方却有输出」的矛盾。全空白串同样按全局 `off` 处理（`Directive::parse` 拒绝
/// 纯空白，base 实际会退回 `--log-level`，所以这里只会比 base 更严，绝不会更宽松）。
///
/// 注意这与「`RUST_LOG` 只有 target 指令、推不出全局级别」是两件事：后者返回 `None` 并回落
/// `warn`，是刻意给第三方留 warn/error 的宽松回落。
///
/// 合法性判据与 `EnvFilter::try_from_default_env` 相同（都是 `Builder::parse`）。
fn effective_global_level(configured: &str, rust_log: Option<&str>) -> Option<LevelFilter> {
    match rust_log {
        Some(raw) if raw.trim().is_empty() => Some(LevelFilter::OFF),
        Some(raw) if EnvFilter::try_new(raw).is_ok() => global_level_from(raw),
        _ => global_level_from(configured),
    }
}

/// 推理/DNS 噪声的抑制级别：sink 全局级别比 `warn` 更严（`error` / `off`）时跟随全局级别，
/// 否则（`trace`/`debug`/`info`/`warn`、没有全局级别、级别无法解析）压到 `warn`。
fn noise_suppression_level(global: Option<LevelFilter>) -> LevelFilter {
    match global {
        Some(level) if level < LevelFilter::WARN => level,
        _ => LevelFilter::WARN,
    }
}

/// 构造叠加到某个 sink filter 上的完整指令表：HTTP 栈固定 `off`，推理/DNS 按 `global` 收紧。
///
/// 纯函数，便于单测；`global` 为 `None` 时按 `warn` 处理。
fn noise_directives(global: Option<LevelFilter>) -> Vec<String> {
    let suppression = noise_suppression_level(global);
    HTTP_NOISE_TARGETS
        .iter()
        .map(|target| format!("{target}=off"))
        .chain(
            INFERENCE_NOISE_TARGETS
                .iter()
                .map(|target| format!("{target}={suppression}")),
        )
        .collect()
}

/// 在给定 filter 上叠加第三方噪声抑制：HTTP 栈一律静音，推理/DNS 压掉低于抑制级别的日志。
///
/// 这些指令要么比 `RUST_LOG` 里更宽的同 target 指令更具体，要么与 `RUST_LOG` 的指令完全同
/// target；后者按 `EnvFilter` 的语义被本表覆盖（`add_directive` 对匹配完全相同事件的旧指令
/// 覆盖其级别），所以 `RUST_LOG=tract_onnx=debug` 依然打不开被抑制的 debug/info；`RUST_LOG`
/// 只能抬升本表未覆盖的 target。
fn with_noise_suppressed(filter: EnvFilter, global: Option<LevelFilter>) -> EnvFilter {
    noise_directives(global)
        .iter()
        .fold(filter, |filter, directive| {
            filter.add_directive(directive.parse().expect("valid noise directive"))
        })
}

pub fn init_tracing(console_level: &str, log_cfg: &LogConfig) -> WorkerGuard {
    // 噪声抑制级别必须与 base filter 的全局级别同源：RUST_LOG 合法时两者都取 RUST_LOG，
    // 否则都取 --log-level。环境变量只读一次，两条判定共用。
    let rust_log = std::env::var(EnvFilter::DEFAULT_ENV).ok();
    let console_filter = with_noise_suppressed(
        console_base_filter(console_level, rust_log.as_deref()),
        effective_global_level(console_level, rust_log.as_deref()),
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

    let file_filter = with_noise_suppressed(
        EnvFilter::new(&log_cfg.file_level),
        global_level_from(&log_cfg.file_level),
    );

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
    use super::{
        console_base_filter, effective_global_level, global_level_from, noise_directives,
        noise_suppression_level, with_noise_suppressed, HTTP_NOISE_TARGETS,
        INFERENCE_NOISE_TARGETS,
    };
    use std::sync::{Arc, Mutex};
    use tracing::level_filters::LevelFilter;
    use tracing_subscriber::{layer::SubscriberExt, EnvFilter, Layer};

    type CapturedEvents = Arc<Mutex<Vec<(&'static str, tracing::Level)>>>;

    struct CaptureEvents(CapturedEvents);

    impl<S: tracing::Subscriber> Layer<S> for CaptureEvents {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let metadata = event.metadata();
            self.0
                .lock()
                .unwrap()
                .push((metadata.target(), *metadata.level()));
        }
    }

    /// 用 `with_default`（线程本地 dispatcher）按调用方给定的固定顺序发事件，返回实际通过的
    /// `(target, level)`。单线程、单 subscriber，结果确定。
    fn capture_events(
        base: EnvFilter,
        global: Option<LevelFilter>,
        emit: impl FnOnce(),
    ) -> Vec<(&'static str, tracing::Level)> {
        let captured: CapturedEvents = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry()
            .with(with_noise_suppressed(base, global))
            .with(CaptureEvents(captured.clone()));

        tracing::subscriber::with_default(subscriber, emit);

        let events = captured.lock().unwrap().clone();
        events
    }

    /// 复刻 `init_tracing` 的控制台路径：base filter 与抑制级别同源于 `rust_log`/`configured`。
    fn console_events_passing(
        configured: &str,
        rust_log: Option<&str>,
        emit: impl FnOnce(),
    ) -> Vec<(&'static str, tracing::Level)> {
        capture_events(
            console_base_filter(configured, rust_log),
            effective_global_level(configured, rust_log),
            emit,
        )
    }

    /// 便捷入口：等价于只给了 `--log-level=<global_level>`、没有 `RUST_LOG`。
    fn events_passing(
        global_level: &str,
        emit: impl FnOnce(),
    ) -> Vec<(&'static str, tracing::Level)> {
        console_events_passing(global_level, None, emit)
    }

    #[test]
    fn noise_filter_silences_third_party_debug_and_keeps_app_debug() {
        let events = events_passing("debug", || {
            // 第三方 debug/info：被噪声指令丢弃
            tracing::debug!(target: "tract_onnx::optim", "onnx debug noise");
            tracing::info!(target: "tract_onnx_opl::optim", "onnx info noise");
            tracing::debug!(target: "hickory_proto::udp::udp_client_stream", "dns debug noise");
            // 第三方 warn/error：保留
            tracing::warn!(target: "tract_core::optim::change_axes", "onnx warn kept");
            tracing::error!(target: "tract_hir::infer::analyser", "onnx error kept");
            tracing::warn!(target: "hickory_resolver::async_resolver", "dns warn kept");
            // 本进程 debug：保留
            tracing::debug!(target: "teamspeakclaw::adapter::headless::speech", "app debug kept");
        });

        assert_eq!(
            events,
            vec![
                ("tract_core::optim::change_axes", tracing::Level::WARN),
                ("tract_hir::infer::analyser", tracing::Level::ERROR),
                ("hickory_resolver::async_resolver", tracing::Level::WARN),
                (
                    "teamspeakclaw::adapter::headless::speech",
                    tracing::Level::DEBUG
                ),
            ],
            "debug 全局级别下：第三方 debug/info 应被抑制，第三方 warn/error 与本进程 debug 应保留"
        );
    }

    #[test]
    fn error_level_sink_drops_third_party_warn_and_keeps_errors() {
        let events = events_passing("error", || {
            // 全局级别收紧到 error：推理/DNS 的 warn/info/debug 都要跟着被压掉
            tracing::warn!(target: "tract_onnx::optim", "onnx warn dropped");
            tracing::info!(target: "tract_onnx_opl::optim", "onnx info dropped");
            tracing::warn!(target: "hickory_proto::udp::udp_client_stream", "dns warn dropped");
            tracing::debug!(target: "hickory_resolver::async_resolver", "dns debug dropped");
            // error 保留
            tracing::error!(target: "tract_onnx::optim", "onnx error kept");
            tracing::error!(target: "hickory_resolver::async_resolver", "dns error kept");
            tracing::error!(target: "teamspeakclaw::adapter::headless::speech", "app error kept");
        });

        assert_eq!(
            events,
            vec![
                ("tract_onnx::optim", tracing::Level::ERROR),
                ("hickory_resolver::async_resolver", tracing::Level::ERROR),
                (
                    "teamspeakclaw::adapter::headless::speech",
                    tracing::Level::ERROR
                ),
            ],
            "error 全局级别下：第三方 warn/info/debug 应被抑制，第三方与本进程 error 应保留"
        );
    }

    #[test]
    fn noise_suppression_level_follows_stricter_global_levels() {
        // `LevelFilter` 的 Ord 按限制强度排序，下面的推导规则依赖这一点。
        assert!(LevelFilter::OFF < LevelFilter::ERROR);
        assert!(LevelFilter::ERROR < LevelFilter::WARN);
        assert!(LevelFilter::WARN < LevelFilter::INFO);
        assert!(LevelFilter::INFO < LevelFilter::DEBUG);
        assert!(LevelFilter::DEBUG < LevelFilter::TRACE);

        for global in [
            None,
            Some(LevelFilter::TRACE),
            Some(LevelFilter::DEBUG),
            Some(LevelFilter::INFO),
            Some(LevelFilter::WARN),
        ] {
            assert_eq!(
                noise_suppression_level(global),
                LevelFilter::WARN,
                "{global:?} 下应压到 warn"
            );
        }
        assert_eq!(
            noise_suppression_level(Some(LevelFilter::ERROR)),
            LevelFilter::ERROR
        );
        assert_eq!(
            noise_suppression_level(Some(LevelFilter::OFF)),
            LevelFilter::OFF
        );
    }

    #[test]
    fn noise_directives_keep_http_off_and_scale_inference_suppression() {
        let default = noise_directives(None);
        let error_sink = noise_directives(Some(LevelFilter::ERROR));
        let off_sink = noise_directives(Some(LevelFilter::OFF));

        assert_eq!(
            default.len(),
            HTTP_NOISE_TARGETS.len() + INFERENCE_NOISE_TARGETS.len(),
            "每个噪声 target 恰好一条指令"
        );
        for target in HTTP_NOISE_TARGETS.iter() {
            for directives in [&default, &error_sink, &off_sink] {
                assert!(
                    directives.contains(&format!("{target}=off")),
                    "{target} 应与 sink 全局级别无关，保持 off"
                );
            }
        }
        for target in INFERENCE_NOISE_TARGETS.iter() {
            assert!(default.contains(&format!("{target}=warn")));
            assert!(error_sink.contains(&format!("{target}=error")));
            assert!(off_sink.contains(&format!("{target}=off")));
        }
    }

    #[test]
    fn empty_rust_log_silences_inference_noise_and_app_alike() {
        // RUST_LOG=""：base 是零指令的 EnvFilter（`enabled` 对任何 target 都返回 false），推导出
        // 的抑制级别是 off，所以谁都不该输出——包括本进程的 ERROR。这一条证明噪声抑制没有在
        // base 之外另开口子。
        let silent = console_events_passing("error", Some(""), || {
            tracing::warn!(target: "tract_onnx::optim", "onnx warn");
            tracing::warn!(target: "hickory_proto::udp::udp_client_stream", "dns warn");
            tracing::error!(target: "tract_onnx::optim", "onnx error");
            tracing::error!(target: "teamspeakclaw::adapter::headless::speech", "app error");
        });
        assert!(
            silent.is_empty(),
            "RUST_LOG 为空串时应全静默，实际通过: {silent:?}"
        );

        // 全空白 RUST_LOG：`Directive::parse` 拒绝纯空白，base 实际退回 --log-level，所以 app
        // 日志仍输出；但显式空配置被当作全局 off，第三方噪声连 error 也不输出——只会更严。
        let whitespace = console_events_passing("error", Some("   "), || {
            tracing::warn!(target: "hickory_resolver::async_resolver", "dns warn");
            tracing::error!(target: "tract_onnx::optim", "onnx error");
            tracing::error!(target: "teamspeakclaw::adapter::headless::speech", "app error");
        });
        assert_eq!(
            whitespace,
            vec![(
                "teamspeakclaw::adapter::headless::speech",
                tracing::Level::ERROR
            )],
            "全空白 RUST_LOG：第三方噪声应全静默，app error 仍按 --log-level 输出"
        );

        // 两种空配置推导出的指令本身也要是 off
        for rust_log in ["", "   "] {
            let directives = noise_directives(effective_global_level("error", Some(rust_log)));
            for target in INFERENCE_NOISE_TARGETS.iter() {
                assert!(
                    directives.contains(&format!("{target}=off")),
                    "RUST_LOG={rust_log:?} 下 {target} 应为 off"
                );
            }
        }
    }

    #[test]
    fn effective_global_level_prefers_valid_rust_log() {
        // RUST_LOG 合法：以它的全局级别为准，忽略 --log-level
        assert_eq!(
            effective_global_level("error", Some("debug")),
            Some(LevelFilter::DEBUG)
        );
        // RUST_LOG 合法但只有 target 指令、没有全局级别：不给全局级别，也不再退回 --log-level。
        // 这是刻意的宽松回落：base 只启用显式 target，我们仍给第三方留 warn/error。
        assert_eq!(
            effective_global_level("error", Some("tower_http=debug")),
            None
        );
        assert!(
            noise_directives(effective_global_level("error", Some("tower_http=debug")))
                .contains(&"tract_onnx=warn".to_string()),
            "只有 target 指令时第三方应回落到 warn，而不是 off"
        );
        // RUST_LOG 存在但为空串 / 全空白：等价于该 sink 全局 off，第三方一并静默
        assert_eq!(
            effective_global_level("error", Some("")),
            Some(LevelFilter::OFF)
        );
        assert_eq!(
            effective_global_level("error", Some("   ")),
            Some(LevelFilter::OFF)
        );
        // RUST_LOG 缺失或非法：退回配置级别
        assert_eq!(
            effective_global_level("info", None),
            Some(LevelFilter::INFO)
        );
        assert_eq!(
            effective_global_level("error", Some("tower_http=verbose")),
            Some(LevelFilter::ERROR)
        );
    }

    #[test]
    fn global_level_from_reads_only_bare_level_directives() {
        assert_eq!(global_level_from("info"), Some(LevelFilter::INFO));
        assert_eq!(global_level_from(" off "), Some(LevelFilter::OFF));
        assert_eq!(global_level_from("debug,"), Some(LevelFilter::DEBUG));
        assert_eq!(
            global_level_from("error,tower_http=debug"),
            Some(LevelFilter::ERROR)
        );
        // 只有 target 指令、空串、未知词都推不出全局级别，调用方回落 warn
        assert_eq!(global_level_from("tower_http=debug"), None);
        assert_eq!(global_level_from(""), None);
        assert_eq!(global_level_from("verbose"), None);
        // 多个全局级别时取最严的一个，保证不比用户配置更宽松
        assert_eq!(
            global_level_from("debug,error,tower_http=debug"),
            Some(LevelFilter::ERROR)
        );
    }
}
