use crate::config::AppConfig;
use crate::llm::provider::{LlmProvider, LlmResponse, OpenAiProvider};
use crate::llm::schema::Tool;
use crate::error::Result;
use arc_swap::ArcSwap;
use serde_json::Value;
use std::sync::{Arc, RwLock};

pub struct LlmEngine {
    config: Arc<ArcSwap<AppConfig>>,
    provider: RwLock<Box<dyn LlmProvider>>,
}

impl LlmEngine {
    pub fn new(config: Arc<ArcSwap<AppConfig>>) -> Self {
        let cfg = config.load();
        let provider = Box::new(OpenAiProvider::new(cfg.llm.clone()));
        Self {
            config,
            provider: RwLock::new(provider),
        }
    }

    fn ensure_provider(&self) {
        let cfg = self.config.load();
        let new_llm = &cfg.llm;
        {
            let p = self.provider.read().unwrap();
            if let Some(openai) = p.as_any().downcast_ref::<OpenAiProvider>() {
                if openai.config_matches(new_llm) {
                    return;
                }
            }
        }
        let mut p = self.provider.write().unwrap();
        *p = Box::new(OpenAiProvider::new(new_llm.clone()));
        tracing::info!("LLM provider hot-reloaded");
    }

    pub async fn chat(&self, messages: Vec<Value>, tools: Vec<Tool>) -> Result<LlmResponse> {
        self.ensure_provider();
        let p = self.provider.read().unwrap();
        p.chat_completion(messages, tools).await
    }
}
