use crate::config::LlmConfig;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat_completion(&self, messages: Vec<Value>, tools: Vec<Value>)
        -> Result<LlmResponse>;
    async fn chat_completion_stream(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
    ) -> Result<BoxStream<'static, Result<LlmStreamEvent>>>;
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

#[derive(Debug, Clone)]
pub enum LlmStreamEvent {
    Token(String),
    ToolCalls,
    Done,
}

pub struct OpenAiProvider {
    client: Client,
    config: LlmConfig,
}

impl OpenAiProvider {
    pub fn new(config: LlmConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
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

    async fn chat_completion_stream(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
    ) -> Result<BoxStream<'static, Result<LlmStreamEvent>>> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "stream": true,
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

        let mut byte_stream = resp.bytes_stream();
        let (tx, rx) = mpsc::channel::<Result<LlmStreamEvent>>(128);
        tokio::spawn(async move {
            let mut pending: Vec<u8> = Vec::new();
            while let Some(item) = byte_stream.next().await {
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                };
                pending.extend_from_slice(&bytes);

                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let mut line_bytes = pending.drain(..=pos).collect::<Vec<u8>>();
                    if line_bytes.last() == Some(&b'\n') {
                        line_bytes.pop();
                    }
                    if line_bytes.last() == Some(&b'\r') {
                        line_bytes.pop();
                    }
                    let line = match std::str::from_utf8(&line_bytes) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = tx
                                .send(Err(anyhow::anyhow!("invalid sse utf8 line: {e}").into()))
                                .await;
                            return;
                        }
                    };
                    if line.is_empty() || !line.starts_with("data: ") {
                        continue;
                    }
                    let payload = line.trim_start_matches("data: ").trim();
                    if payload == "[DONE]" {
                        let _ = tx.send(Ok(LlmStreamEvent::Done)).await;
                        return;
                    }
                    let event: Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = tx.send(Err(e.into())).await;
                            return;
                        }
                    };
                    if let Some(content) = event["choices"][0]["delta"]["content"].as_str() {
                        if !content.is_empty() {
                            let _ = tx
                                .send(Ok(LlmStreamEvent::Token(content.to_string())))
                                .await;
                        }
                    }
                    if let Some(tool_calls) = event["choices"][0]["delta"]["tool_calls"].as_array() {
                        if !tool_calls.is_empty() {
                            let _ = tx.send(Ok(LlmStreamEvent::ToolCalls)).await;
                        }
                    }
                    if event["choices"][0]["finish_reason"].is_string() {
                        let _ = tx.send(Ok(LlmStreamEvent::Done)).await;
                        return;
                    }
                }
            }
            let _ = tx.send(Ok(LlmStreamEvent::Done)).await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}
