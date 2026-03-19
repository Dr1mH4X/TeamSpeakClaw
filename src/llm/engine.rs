use crate::config::AppConfig;
use crate::llm::provider::{LlmProvider, LlmResponse, OpenAiProvider};
use anyhow::Result;
use arc_swap::ArcSwap;
use serde_json::Value;
use std::sync::Arc;

pub struct LlmEngine {
    #[allow(dead_code)]
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
