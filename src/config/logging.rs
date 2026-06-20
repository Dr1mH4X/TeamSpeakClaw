use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LogConfig {
    pub file_level: String,
    pub max_log_days: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            file_level: "debug".to_string(),
            max_log_days: 7,
        }
    }
}
