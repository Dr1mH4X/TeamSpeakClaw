use crate::config::LlmConfig;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat_completion(&self, messages: Vec<Value>, tools: Vec<Value>)
        -> Result<LlmResponse>;
}

#[derive(Debug)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
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
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_completion(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
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

        let mut tool_calls = Vec::new();
        if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
            for call in calls {
                let id = call["id"].as_str().unwrap_or_default().to_string();
                let func = &call["function"];
                let name = func["name"].as_str().unwrap_or_default().to_string();
                let args_str = func["arguments"].as_str().unwrap_or("{}");
                let args = serde_json::from_str(args_str).unwrap_or(json!({}));

                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: args,
                });
            }
        }

        Ok(LlmResponse {
            content,
            tool_calls,
        })
    }
}
