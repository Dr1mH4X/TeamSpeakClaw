use super::toml_value;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct LlmConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            max_tokens: 1024,
        }
    }
}

impl LlmConfig {
    pub fn to_toml(&self) -> String {
        let mut output = String::new();
        output.push_str("[llm]\n");
        output.push_str(&format!("api_key = {}\n", toml_value(&self.api_key)));
        output.push_str(&format!("base_url = {}\n", toml_value(&self.base_url)));
        output.push_str(&format!("model = {}\n", toml_value(&self.model)));
        output.push_str(&format!("max_tokens = {}\n", self.max_tokens));
        output
    }
}
