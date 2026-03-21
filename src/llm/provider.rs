use crate::config::LlmConfig;
use crate::error::Result;
use crate::llm::schema::{Tool, ToolCall};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat_completion(&self, messages: Vec<Value>, tools: Vec<Tool>)
        -> Result<LlmResponse>;
    fn as_any(&self) -> &dyn std::any::Any;
}

#[derive(Debug)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

pub struct OpenAiProvider {
    client: Client,
    config: LlmConfig,
}

impl OpenAiProvider {
    pub fn new(config: LlmConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .unwrap_or_default();
        Self { client, config }
    }

    pub fn config_matches(&self, other: &LlmConfig) -> bool {
        self.config.provider == other.provider
            && self.config.api_key == other.api_key
            && self.config.base_url == other.base_url
            && self.config.model == other.model
            && self.config.max_tokens == other.max_tokens
            && self.config.timeout_secs == other.timeout_secs
            && self.config.retry_max == other.retry_max
            && self.config.retry_delay_ms == other.retry_delay_ms
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_completion(
        &self,
        messages: Vec<Value>,
        tools: Vec<Tool>,
    ) -> Result<LlmResponse> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": self.config.max_tokens,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!("auto");
        }

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let error_text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("LLM API error: {}", error_text).into());
        }

        let data: Value = resp.json().await?;

        let choice = data["choices"][0]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Invalid response format"))?;
        let message = choice["message"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Invalid message format"))?;

        let content = message
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let tool_calls: Vec<ToolCall> = if let Some(calls) = message.get("tool_calls") {
            serde_json::from_value(calls.clone())
                .map_err(|e| anyhow::anyhow!("Failed to parse tool calls: {}", e))?
        } else {
            Vec::new()
        };

        Ok(LlmResponse {
            content,
            tool_calls,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
