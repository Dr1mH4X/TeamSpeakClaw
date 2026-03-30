use super::toml_value;
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

impl LogConfig {
    pub fn to_toml(&self) -> String {
        let mut output = String::new();
        output.push_str("[logging]\n");
        output.push_str(&format!("file_level = {}\n", toml_value(&self.file_level)));
        output
    }
}
