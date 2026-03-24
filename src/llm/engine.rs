use crate::config::AppConfig;
use crate::llm::provider::{LlmProvider, LlmResponse, OpenAiProvider};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

pub struct LlmEngine {
    provider: Box<dyn LlmProvider>,
}

impl LlmEngine {
    pub fn new(config: Arc<AppConfig>) -> Self {
        let cfg = &config;
        let provider = Box::new(OpenAiProvider::new(cfg.llm.clone()));
        Self { provider }
    }

    pub async fn chat(&self, messages: Vec<Value>, tools: Vec<Value>) -> Result<LlmResponse> {
        Ok(self.provider.chat_completion(messages, tools).await?)
    }
}
