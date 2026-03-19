use crate::config::AppConfig;
use crate::llm::provider::{LlmProvider, OpenAiProvider, LlmResponse};
use anyhow::Result;
use arc_swap::ArcSwap;
use std::sync::Arc;
use serde_json::Value;

pub struct LlmEngine {
    config: Arc<ArcSwap<AppConfig>>,
    provider: Box<dyn LlmProvider>,
}

impl LlmEngine {
    pub fn new(config: Arc<ArcSwap<AppConfig>>) -> Self {
        let cfg = config.load();
        let provider = Box::new(OpenAiProvider::new(cfg.llm.clone()));
        Self { config, provider }
    }
    
    pub async fn chat(&self, messages: Vec<Value>, tools: Vec<Value>) -> Result<LlmResponse> {
        Ok(self.provider.chat_completion(messages, tools).await?)
    }
}
