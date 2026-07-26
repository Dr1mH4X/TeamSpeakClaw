use crate::config::LlmConfig;
use anyhow::Result;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub(crate) const MAX_TOOL_CALLS_PER_TURN: usize = 8;
pub(crate) const MAX_TOOL_ARGUMENT_BYTES_TOTAL: usize = 64 * 1024;

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

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChunkDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct ToolCallBuilder {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn merge_tool_call_delta(
    builders: &mut BTreeMap<usize, ToolCallBuilder>,
    tool_call: ToolCallDelta,
) -> Result<()> {
    let index = tool_call.index;
    if !builders.contains_key(&index) && builders.len() >= MAX_TOOL_CALLS_PER_TURN {
        anyhow::bail!("tool call count exceeds the per-turn limit of {MAX_TOOL_CALLS_PER_TURN}");
    }

    let incoming_argument_bytes = tool_call
        .function
        .as_ref()
        .and_then(|function| function.arguments.as_ref())
        .map_or(0, String::len);
    let accumulated_argument_bytes = builders
        .values()
        .try_fold(0usize, |total, builder| {
            total.checked_add(builder.arguments.len())
        })
        .ok_or_else(|| anyhow::anyhow!("tool argument byte count overflowed"))?;
    let next_argument_bytes = accumulated_argument_bytes
        .checked_add(incoming_argument_bytes)
        .ok_or_else(|| anyhow::anyhow!("tool argument byte count overflowed"))?;
    if next_argument_bytes > MAX_TOOL_ARGUMENT_BYTES_TOTAL {
        anyhow::bail!(
            "tool arguments exceed the total byte limit of {MAX_TOOL_ARGUMENT_BYTES_TOTAL}"
        );
    }

    let entry = builders.entry(index).or_default();
    if let Some(id) = tool_call.id.filter(|value| !value.is_empty()) {
        if entry.id.replace(id.clone()).is_some_and(|old| old != id) {
            anyhow::bail!("conflicting IDs for tool call index {index}");
        }
    }
    if let Some(function) = tool_call.function {
        if let Some(name) = function.name.filter(|value| !value.is_empty()) {
            if entry
                .name
                .replace(name.clone())
                .is_some_and(|old| old != name)
            {
                anyhow::bail!("conflicting names for tool call index {index}");
            }
        }
        if let Some(arguments) = function.arguments {
            entry.arguments.push_str(&arguments);
        }
    }
    Ok(())
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
            return Err(anyhow::anyhow!("LLM API error: HTTP {}", resp.status()).into());
        }

        let mut byte_stream = resp.bytes_stream();
        let (tx, rx) = mpsc::channel::<Result<LlmStreamEvent>>(128);
        tokio::spawn(async move {
            let mut pending: Vec<u8> = Vec::new();
            let mut tool_call_builders: BTreeMap<usize, ToolCallBuilder> = BTreeMap::new();

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
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let payload = payload.trim_start();
                    if payload.is_empty() {
                        continue;
                    }
                    if payload == "[DONE]" {
                        let _ = tx
                            .send(Err(anyhow::anyhow!(
                                "LLM stream ended without a finish reason"
                            )))
                            .await;
                        return;
                    }
                    let event: ChatCompletionChunk = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = tx.send(Err(e.into())).await;
                            return;
                        }
                    };
                    let Some(choice) = event.choices.into_iter().next() else {
                        continue;
                    };
                    if let Some(content) = choice.delta.content {
                        if !content.is_empty() {
                            if tx.send(Ok(LlmStreamEvent::Token(content))).await.is_err() {
                                return;
                            }
                        }
                    }
                    for tool_call in choice.delta.tool_calls {
                        if let Err(error) =
                            merge_tool_call_delta(&mut tool_call_builders, tool_call)
                        {
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    }
                    if let Some(finish_reason) = choice.finish_reason {
                        if !finish_reason.is_empty() {
                            let tool_calls = match finalize_tool_calls(&mut tool_call_builders) {
                                Ok(tool_calls) => tool_calls,
                                Err(error) => {
                                    let _ = tx.send(Err(error)).await;
                                    return;
                                }
                            };
                            let _ = tx
                                .send(Ok(LlmStreamEvent::Done {
                                    finish_reason,
                                    tool_calls,
                                }))
                                .await;
                            return;
                        }
                    }
                }
            }
            let _ = tx
                .send(Err(anyhow::anyhow!(
                    "LLM stream closed without a finish reason"
                )))
                .await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

fn finalize_tool_calls(builders: &mut BTreeMap<usize, ToolCallBuilder>) -> Result<Vec<ToolCall>> {
    std::mem::take(builders)
        .into_iter()
        .map(|(index, builder)| {
            let id = builder
                .id
                .ok_or_else(|| anyhow::anyhow!("tool call index {index} is missing an ID"))?;
            let name = builder
                .name
                .ok_or_else(|| anyhow::anyhow!("tool call index {index} is missing a name"))?;
            let arguments = serde_json::from_str(&builder.arguments).map_err(|error| {
                anyhow::anyhow!("tool call index {index} has invalid arguments: {error}")
            })?;
            Ok(ToolCall {
                id,
                name,
                arguments,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_content_is_ignored() {
        let chunk: ChatCompletionChunk = serde_json::from_value(json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "hidden",
                    "content": "visible"
                },
                "finish_reason": null
            }]
        }))
        .unwrap();

        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("visible"));
    }

    #[test]
    fn finalizes_fragmented_tool_call() {
        let chunks: Vec<ChatCompletionChunk> = serde_json::from_value(json!([
            {
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {
                                "name": "lookup",
                                "arguments": "{\"query\":\""
                            }
                        }]
                    },
                    "finish_reason": null
                }]
            },
            {
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": {
                                "arguments": "rust\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }
        ]))
        .unwrap();
        let mut builders = BTreeMap::new();
        for chunk in chunks {
            for tool_call in chunk.choices.into_iter().next().unwrap().delta.tool_calls {
                merge_tool_call_delta(&mut builders, tool_call).unwrap();
            }
        }

        let calls = finalize_tool_calls(&mut builders).unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-1");
        assert_eq!(calls[0].name, "lookup");
        assert_eq!(calls[0].arguments, json!({"query": "rust"}));
    }

    #[test]
    fn rejects_invalid_tool_call_arguments() {
        let mut builders = BTreeMap::from([(
            0,
            ToolCallBuilder {
                id: Some("call-1".to_string()),
                name: Some("lookup".to_string()),
                arguments: "{".to_string(),
            },
        )]);

        let error = finalize_tool_calls(&mut builders).unwrap_err();

        assert!(error.to_string().contains("invalid arguments"));
    }

    #[test]
    fn rejects_tool_call_without_identity() {
        let mut builders = BTreeMap::from([(0, ToolCallBuilder::default())]);

        let error = finalize_tool_calls(&mut builders).unwrap_err();

        assert!(error.to_string().contains("missing an ID"));
    }

    #[test]
    fn rejects_too_many_streamed_tool_calls() {
        let mut builders = BTreeMap::new();
        for index in 0..MAX_TOOL_CALLS_PER_TURN {
            merge_tool_call_delta(
                &mut builders,
                ToolCallDelta {
                    index,
                    id: Some(format!("call-{index}")),
                    function: Some(FunctionDelta {
                        name: Some("lookup".to_string()),
                        arguments: Some("{}".to_string()),
                    }),
                },
            )
            .unwrap();
        }

        let error = merge_tool_call_delta(
            &mut builders,
            ToolCallDelta {
                index: MAX_TOOL_CALLS_PER_TURN,
                id: Some("call-over-limit".to_string()),
                function: Some(FunctionDelta {
                    name: Some("lookup".to_string()),
                    arguments: Some("{}".to_string()),
                }),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("per-turn limit"));
        assert_eq!(builders.len(), MAX_TOOL_CALLS_PER_TURN);
    }

    #[test]
    fn rejects_oversized_streamed_tool_arguments() {
        let mut builders = BTreeMap::new();
        let error = merge_tool_call_delta(
            &mut builders,
            ToolCallDelta {
                index: 0,
                id: Some("call-1".to_string()),
                function: Some(FunctionDelta {
                    name: Some("lookup".to_string()),
                    arguments: Some("x".repeat(MAX_TOOL_ARGUMENT_BYTES_TOTAL + 1)),
                }),
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("total byte limit"));
        assert!(builders.is_empty());
    }
}
