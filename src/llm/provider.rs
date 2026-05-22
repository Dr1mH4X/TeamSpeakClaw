use crate::config::LlmConfig;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat_completion_stream(
        &self,
        messages: Vec<Value>,
        tools: Vec<Value>,
    ) -> Result<BoxStream<'static, Result<LlmStreamEvent>>>;
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
    Done {
        finish_reason: String,
        tool_calls: Vec<ToolCall>,
    },
}

pub struct OpenAiProvider {
    client: Client,
    config: LlmConfig,
}

impl OpenAiProvider {
    pub fn new(config: LlmConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("Version: 5.10.0 (c3d4709c)")
            .build()
            .unwrap_or_default();
        Self { client, config }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
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
            let mut tool_call_builders: HashMap<usize, (Option<String>, Option<String>, String)> =
                HashMap::new();

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
                        let tool_calls = finalize_tool_calls(&mut tool_call_builders);
                        let _ = tx
                            .send(Ok(LlmStreamEvent::Done {
                                finish_reason: "stop".to_string(),
                                tool_calls,
                            }))
                            .await;
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
                    if let Some(tool_calls) = event["choices"][0]["delta"]["tool_calls"].as_array()
                    {
                        for tc in tool_calls {
                            let index = tc["index"].as_i64().unwrap_or(0) as usize;
                            let entry = tool_call_builders.entry(index).or_insert((
                                None,
                                None,
                                String::new(),
                            ));
                            if let Some(id) = tc["id"].as_str() {
                                if !id.is_empty() {
                                    entry.0 = Some(id.to_string());
                                }
                            }
                            if let Some(func) = tc.get("function").and_then(|v| v.as_object()) {
                                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                                    if !name.is_empty() {
                                        entry.1 = Some(name.to_string());
                                    }
                                }
                                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                                    entry.2.push_str(args);
                                }
                            }
                        }
                    }
                    if let Some(finish_reason) = event["choices"][0]["finish_reason"].as_str() {
                        if !finish_reason.is_empty() {
                            let tool_calls = finalize_tool_calls(&mut tool_call_builders);
                            let _ = tx
                                .send(Ok(LlmStreamEvent::Done {
                                    finish_reason: finish_reason.to_string(),
                                    tool_calls,
                                }))
                                .await;
                            return;
                        }
                    }
                }
            }
            let tool_calls = finalize_tool_calls(&mut tool_call_builders);
            let _ = tx
                .send(Ok(LlmStreamEvent::Done {
                    finish_reason: "stop".to_string(),
                    tool_calls,
                }))
                .await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

fn finalize_tool_calls(
    builders: &mut HashMap<usize, (Option<String>, Option<String>, String)>,
) -> Vec<ToolCall> {
    builders
        .drain()
        .map(|(_, (id, name, args))| ToolCall {
            id: id.unwrap_or_default(),
            name: name.unwrap_or_default(),
            arguments: serde_json::from_str(&args).unwrap_or(Value::Null),
        })
        .collect()
}
