use crate::config::{LlmConfig, LLM_RETRY_DELAY_MS, LLM_RETRY_MAX, LLM_TIMEOUT_SECS};
use crate::error::Result;
use crate::llm::schema::{Tool, ToolCall};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::sleep;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat_completion(&self, messages: Vec<Value>, tools: Vec<Tool>) -> Result<LlmResponse>;
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
            .timeout(Duration::from_secs(LLM_TIMEOUT_SECS))
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
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat_completion(&self, messages: Vec<Value>, tools: Vec<Tool>) -> Result<LlmResponse> {
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

        for attempt in 0..=LLM_RETRY_MAX {
            let resp_result = self
                .client
                .post(&url)
                .header("Authorization", format!("Bearer {}", self.config.api_key))
                .json(&body)
                .send()
                .await;

            match resp_result {
                Ok(resp) => {
                    if resp.status().is_success() {
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

                        let tool_calls: Vec<ToolCall> = if let Some(calls) =
                            message.get("tool_calls")
                        {
                            serde_json::from_value(calls.clone())
                                .map_err(|e| anyhow::anyhow!("Failed to parse tool calls: {}", e))?
                        } else {
                            Vec::new()
                        };

                        return Ok(LlmResponse {
                            content,
                            tool_calls,
                        });
                    }

                    let status = resp.status();
                    let error_text = resp.text().await.unwrap_or_default();
                    let can_retry = status.is_server_error() || status.as_u16() == 429;
                    if can_retry && attempt < LLM_RETRY_MAX {
                        sleep(Duration::from_millis(LLM_RETRY_DELAY_MS)).await;
                        continue;
                    }
                    return Err(
                        anyhow::anyhow!("LLM API error [{}]: {}", status, error_text).into(),
                    );
                }
                Err(e) => {
                    if attempt < LLM_RETRY_MAX {
                        sleep(Duration::from_millis(LLM_RETRY_DELAY_MS)).await;
                        continue;
                    }
                    return Err(e.into());
                }
            }
        }

        Err(anyhow::anyhow!("LLM request failed after retries").into())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
