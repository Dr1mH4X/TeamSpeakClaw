#![allow(dead_code)]

use crate::config::AuditConfig;
use anyhow::Result;
use chrono::{Local, Utc};
use serde::Serialize;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

pub struct AuditLog {
    enabled: bool,
    writer: Option<Mutex<std::fs::File>>,
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
        let writer = if config.enabled {
            std::fs::create_dir_all(&config.log_dir)?;
            
            let filename = Local::now().format("tsclaw-%y-%m-%d.log").to_string();
            let path = format!("{}/{}", config.log_dir, filename);
            
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            Some(Mutex::new(file))
        } else {
            None
        };
        Ok(Self {
            enabled: config.enabled,
            writer,
        })
    }

    pub fn log(&self, event: &str, details: Value) {
        if !self.enabled {
            return;
        }
        if let Some(writer) = &self.writer {
            let entry = LogEntry {
                ts: Utc::now().to_rfc3339(),
                level: "INFO".into(),
                event: event.into(),
                details,
            };
            if let Ok(line) = serde_json::to_string(&entry) {
                if let Ok(mut w) = writer.lock() {
                    let _ = writeln!(w, "{}", line);
                }
            }
        }
    }
}
