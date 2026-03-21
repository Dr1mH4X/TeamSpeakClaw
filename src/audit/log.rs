use crate::config::AuditConfig;
use anyhow::Result;
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};

pub struct DailyLogWriter {
    dir: PathBuf,
    file_stem: String,
    extension: String,
    current_date: String,
    file: Option<File>,
}

impl DailyLogWriter {
    pub fn new(dir: PathBuf, filename: &str) -> Self {
        let path = Path::new(filename);
        let file_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("tsclaw")
            .to_string();
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("log")
            .to_string();

        Self {
            dir,
            file_stem,
            extension,
            current_date: String::new(),
            file: None,
        }
    }

    fn ensure_file_open(&mut self) -> std::io::Result<()> {
        let now = Utc::now();
        let now_date = now.format("%Y-%m-%d").to_string();

        if self.file.is_none() || self.current_date != now_date {
             let separator = if self.file_stem.ends_with('-') || self.file_stem.ends_with('_') {
                ""
            } else {
                "-"
            };

            let new_filename = format!(
                "{}{}{}.{}",
                self.file_stem, separator, now_date, self.extension
            );
            let file_path = self.dir.join(new_filename);
            
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(file_path)?;

            self.file = Some(file);
            self.current_date = now_date;
        }
        Ok(())
    }
}

impl Write for DailyLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.ensure_file_open()?;
        if let Some(file) = &mut self.file {
            file.write(buf)
        } else {
            Ok(0)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()
        } else {
            Ok(())
        }
    }
}

pub struct AuditLog {
    enabled: bool,
    writer: Option<Mutex<NonBlocking>>,
    _guard: Option<WorkerGuard>,
}

#[derive(Debug, Serialize)]
struct LogEntry {
    ts: String,
    level: String,
    event: String,
    details: Value,
}

impl AuditLog {
    pub fn new(config: &AuditConfig) -> Result<Self> {
        if !config.enabled {
            return Ok(Self {
                enabled: false,
                writer: None,
                _guard: None,
            });
        }

        std::fs::create_dir_all(&config.log_dir)?;

        let custom_writer = DailyLogWriter::new(PathBuf::from(&config.log_dir), "teamspeakclaw-audit.log");

        let (non_blocking, guard) = tracing_appender::non_blocking(custom_writer);

        Ok(Self {
            enabled: true,
            writer: Some(Mutex::new(non_blocking)),
            _guard: Some(guard),
        })
    }

    pub fn log(&self, event: &str, details: Value) {
        if !self.enabled {
            return;
        }

        if let Some(writer_mutex) = &self.writer {
            let entry = LogEntry {
                ts: Utc::now().to_rfc3339(),
                level: "INFO".into(),
                event: event.into(),
                details,
            };

            if let Ok(line) = serde_json::to_string(&entry) {
                if let Ok(mut w) = writer_mutex.lock() {
                    let _ = writeln!(w, "{}", line);
                }
            }
        }
    }
}

