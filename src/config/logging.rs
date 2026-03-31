use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LogConfig {
    pub file_level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            file_level: "debug".to_string(),
        }
    }
}
