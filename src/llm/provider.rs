use crate::config::LlmConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) const MAX_TOOL_CALLS_PER_TURN: usize = 8;
pub(crate) const MAX_TOOL_ARGUMENT_BYTES_TOTAL: usize = 64 * 1024;
const MAX_SSE_LINE_BYTES: usize = 256 * 1024;
const MAX_SSE_STREAM_BYTES: usize = 8 * 1024 * 1024;

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
    pub fn new(config: LlmConfig) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
            .user_agent("Version: 5.10.0 (c3d4709c)")
            .build()
            .context("failed to build the LLM HTTP client")?;
        Ok(Self { client, config })
    }

    fn build_request(&self, url: &str, body: &Value) -> reqwest::RequestBuilder {
        let request = self.client.post(url).json(body);
        let api_key = self.config.api_key.trim();
        if api_key.is_empty() {
            request
        } else {
            request.bearer_auth(api_key)
        }
    }
}

fn validate_sse_append(
    pending_len: usize,
    incoming: &[u8],
    received_bytes: usize,
) -> Result<usize> {
    let next_received_bytes = received_bytes
        .checked_add(incoming.len())
        .ok_or_else(|| anyhow::anyhow!("LLM SSE stream byte count overflowed"))?;
    if next_received_bytes > MAX_SSE_STREAM_BYTES {
        anyhow::bail!("LLM SSE stream exceeds the {MAX_SSE_STREAM_BYTES}-byte limit");
    }

    let mut line_bytes = pending_len;
    for byte in incoming {
        if *byte == b'\n' {
            line_bytes = 0;
            continue;
        }

        line_bytes = line_bytes
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("LLM SSE line byte count overflowed"))?;
        if line_bytes > MAX_SSE_LINE_BYTES {
            anyhow::bail!("LLM SSE line exceeds the {MAX_SSE_LINE_BYTES}-byte limit");
        }
    }

    Ok(next_received_bytes)
}

enum StreamPoll<T> {
    Item(T),
    End,
    IdleTimeout,
    TotalTimeout,
}

#[derive(Debug, PartialEq, Eq)]
enum DeadlineOutcome<T> {
    Completed(T),
    IdleTimeout,
    TotalTimeout,
}

async fn wait_with_deadlines<F>(
    future: F,
    idle_timeout: Duration,
    total_deadline: tokio::time::Instant,
) -> DeadlineOutcome<F::Output>
where
    F: Future,
{
    tokio::pin!(future);
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(total_deadline) => DeadlineOutcome::TotalTimeout,
        _ = tokio::time::sleep(idle_timeout) => DeadlineOutcome::IdleTimeout,
        output = &mut future => DeadlineOutcome::Completed(output),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputSend {
    Sent,
    Closed,
    TotalTimeout,
}

async fn send_output_before_deadline<T>(
    tx: &mpsc::Sender<T>,
    value: T,
    total_deadline: tokio::time::Instant,
) -> OutputSend {
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(total_deadline) => OutputSend::TotalTimeout,
        result = tx.send(value) => {
            if result.is_ok() {
                OutputSend::Sent
            } else {
                OutputSend::Closed
            }
        },
    }
}

fn output_stream_with_total_deadline(
    rx: mpsc::Receiver<Result<LlmStreamEvent>>,
    total_deadline: tokio::time::Instant,
    total_timeout_secs: u64,
) -> impl Stream<Item = Result<LlmStreamEvent>> {
    futures_util::stream::unfold(Some(rx), move |state| async move {
        let mut rx = state?;
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(total_deadline) => Some((
                Err(anyhow::anyhow!(
                    "LLM stream exceeded the total timeout of {total_timeout_secs} seconds"
                )),
                None,
            )),
            item = rx.recv() => item.map(|item| (item, Some(rx))),
        }
    })
}

async fn poll_stream<S>(
    stream: &mut S,
    idle_timeout: Duration,
    total_deadline: tokio::time::Instant,
) -> StreamPoll<S::Item>
where
    S: Stream + Unpin,
{
    match wait_with_deadlines(stream.next(), idle_timeout, total_deadline).await {
        DeadlineOutcome::Completed(Some(item)) => StreamPoll::Item(item),
        DeadlineOutcome::Completed(None) => StreamPoll::End,
        DeadlineOutcome::IdleTimeout => StreamPoll::IdleTimeout,
        DeadlineOutcome::TotalTimeout => StreamPoll::TotalTimeout,
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

        let stream_idle_timeout = Duration::from_secs(self.config.stream_idle_timeout_secs);
        let stream_total_timeout = Duration::from_secs(self.config.stream_total_timeout_secs);
        let total_deadline = tokio::time::Instant::now()
            .checked_add(stream_total_timeout)
            .ok_or_else(|| anyhow::anyhow!("llm.stream_total_timeout_secs is too large"))?;
        let resp = match wait_with_deadlines(
            self.build_request(&url, &body).send(),
            stream_idle_timeout,
            total_deadline,
        )
        .await
        {
            DeadlineOutcome::Completed(result) => result?,
            DeadlineOutcome::IdleTimeout => anyhow::bail!(
                "LLM response headers were idle for {} seconds",
                self.config.stream_idle_timeout_secs
            ),
            DeadlineOutcome::TotalTimeout => anyhow::bail!(
                "LLM stream exceeded the total timeout of {} seconds",
                self.config.stream_total_timeout_secs
            ),
        };

        if !resp.status().is_success() {
            return Err(anyhow::anyhow!("LLM API error: HTTP {}", resp.status()));
        }

        let mut byte_stream = resp.bytes_stream();
        let stream_idle_timeout_secs = self.config.stream_idle_timeout_secs;
        let stream_total_timeout_secs = self.config.stream_total_timeout_secs;
        let (tx, rx) = mpsc::channel::<Result<LlmStreamEvent>>(128);
        tokio::spawn(async move {
            let mut pending: Vec<u8> = Vec::new();
            let mut received_bytes = 0usize;
            let mut tool_call_builders: BTreeMap<usize, ToolCallBuilder> = BTreeMap::new();

            loop {
                let item = match poll_stream(&mut byte_stream, stream_idle_timeout, total_deadline)
                    .await
                {
                    StreamPoll::Item(item) => item,
                    StreamPoll::End => break,
                    StreamPoll::TotalTimeout => return,
                    StreamPoll::IdleTimeout => {
                        let _ = send_output_before_deadline(
                            &tx,
                            Err(anyhow::anyhow!(
                                "LLM stream was idle for {stream_idle_timeout_secs} seconds"
                            )),
                            total_deadline,
                        )
                        .await;
                        return;
                    }
                };
                let bytes = match item {
                    Ok(b) => b,
                    Err(e) => {
                        let _ =
                            send_output_before_deadline(&tx, Err(e.into()), total_deadline).await;
                        return;
                    }
                };
                received_bytes = match validate_sse_append(pending.len(), &bytes, received_bytes) {
                    Ok(received_bytes) => received_bytes,
                    Err(error) => {
                        let _ = send_output_before_deadline(&tx, Err(error), total_deadline).await;
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
                            let _ = send_output_before_deadline(
                                &tx,
                                Err(anyhow::anyhow!("invalid sse utf8 line: {e}")),
                                total_deadline,
                            )
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
                        let _ = send_output_before_deadline(
                            &tx,
                            Err(anyhow::anyhow!("LLM stream ended without a finish reason")),
                            total_deadline,
                        )
                        .await;
                        return;
                    }
                    let event: ChatCompletionChunk = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(e) => {
                            let _ = send_output_before_deadline(&tx, Err(e.into()), total_deadline)
                                .await;
                            return;
                        }
                    };
                    let Some(choice) = event.choices.into_iter().next() else {
                        continue;
                    };
                    if let Some(content) = choice.delta.content {
                        if !content.is_empty()
                            && send_output_before_deadline(
                                &tx,
                                Ok(LlmStreamEvent::Token(content)),
                                total_deadline,
                            )
                            .await
                                != OutputSend::Sent
                        {
                            return;
                        }
                    }
                    for tool_call in choice.delta.tool_calls {
                        if let Err(error) =
                            merge_tool_call_delta(&mut tool_call_builders, tool_call)
                        {
                            let _ =
                                send_output_before_deadline(&tx, Err(error), total_deadline).await;
                            return;
                        }
                    }
                    if let Some(finish_reason) = choice.finish_reason {
                        if !finish_reason.is_empty() {
                            let tool_calls = match finalize_tool_calls(&mut tool_call_builders) {
                                Ok(tool_calls) => tool_calls,
                                Err(error) => {
                                    let _ = send_output_before_deadline(
                                        &tx,
                                        Err(error),
                                        total_deadline,
                                    )
                                    .await;
                                    return;
                                }
                            };
                            let _ = send_output_before_deadline(
                                &tx,
                                Ok(LlmStreamEvent::Done {
                                    finish_reason,
                                    tool_calls,
                                }),
                                total_deadline,
                            )
                            .await;
                            return;
                        }
                    }
                }
            }
            let _ = send_output_before_deadline(
                &tx,
                Err(anyhow::anyhow!("LLM stream closed without a finish reason")),
                total_deadline,
            )
            .await;
        });

        Ok(Box::pin(output_stream_with_total_deadline(
            rx,
            total_deadline,
            stream_total_timeout_secs,
        )))
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
    use reqwest::header::AUTHORIZATION;

    #[test]
    fn omits_authorization_header_for_empty_api_key() {
        let config = LlmConfig {
            api_key: " \t".to_string(),
            ..LlmConfig::default()
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let request = provider
            .build_request("http://localhost/chat/completions", &json!({}))
            .build()
            .unwrap();

        assert!(!request.headers().contains_key(AUTHORIZATION));
    }

    #[test]
    fn adds_authorization_header_for_non_empty_api_key() {
        let config = LlmConfig {
            api_key: " test-key ".to_string(),
            ..LlmConfig::default()
        };
        let provider = OpenAiProvider::new(config).unwrap();

        let request = provider
            .build_request("http://localhost/chat/completions", &json!({}))
            .build()
            .unwrap();

        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer test-key"
        );
    }

    #[test]
    fn accepts_bounded_sse_lines_before_append() {
        let mut incoming = vec![b'x'; MAX_SSE_LINE_BYTES];
        incoming.push(b'\n');
        incoming.extend_from_slice(b"short");

        let received_bytes = validate_sse_append(0, &incoming, 0).unwrap();

        assert_eq!(received_bytes, incoming.len());
    }

    #[test]
    fn rejects_oversized_sse_line_before_append() {
        let error = validate_sse_append(MAX_SSE_LINE_BYTES, b"x", 0).unwrap_err();

        assert!(error.to_string().contains("SSE line"));
    }

    #[test]
    fn rejects_oversized_raw_sse_stream_before_append() {
        let error = validate_sse_append(0, b"x", MAX_SSE_STREAM_BYTES).unwrap_err();

        assert!(error.to_string().contains("SSE stream"));
    }

    #[tokio::test]
    async fn reports_stream_idle_timeout() {
        let mut stream = futures_util::stream::pending::<()>();
        let total_deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        let result = poll_stream(&mut stream, Duration::from_millis(20), total_deadline).await;

        assert!(matches!(result, StreamPoll::IdleTimeout));
    }

    #[tokio::test]
    async fn reports_stream_total_timeout() {
        let mut stream = futures_util::stream::pending::<()>();
        let total_deadline = tokio::time::Instant::now() + Duration::from_millis(20);

        let result = poll_stream(&mut stream, Duration::from_secs(5), total_deadline).await;

        assert!(matches!(result, StreamPoll::TotalTimeout));
    }

    #[tokio::test]
    async fn pending_header_future_reports_idle_timeout() {
        let total_deadline = tokio::time::Instant::now() + Duration::from_secs(5);

        let result = wait_with_deadlines(
            std::future::pending::<()>(),
            Duration::from_millis(20),
            total_deadline,
        )
        .await;

        assert_eq!(result, DeadlineOutcome::IdleTimeout);
    }

    #[tokio::test]
    async fn pending_header_future_reports_total_timeout() {
        let total_deadline = tokio::time::Instant::now() + Duration::from_millis(20);

        let result = wait_with_deadlines(
            std::future::pending::<()>(),
            Duration::from_secs(5),
            total_deadline,
        )
        .await;

        assert_eq!(result, DeadlineOutcome::TotalTimeout);
    }

    #[tokio::test]
    async fn full_output_channel_stops_at_total_deadline() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(1).await.unwrap();
        let total_deadline = tokio::time::Instant::now() + Duration::from_millis(20);

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            send_output_before_deadline(&tx, 2, total_deadline),
        )
        .await
        .expect("full output channel must stop at the total deadline");

        assert_eq!(result, OutputSend::TotalTimeout);
        assert_eq!(rx.recv().await, Some(1));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn output_stream_total_deadline_preempts_buffered_items() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(Ok(LlmStreamEvent::Token("late".to_string())))
            .await
            .unwrap();
        let mut stream = Box::pin(output_stream_with_total_deadline(
            rx,
            tokio::time::Instant::now() - Duration::from_secs(1),
            1,
        ));

        let error = stream.next().await.unwrap().unwrap_err();

        assert!(error.to_string().contains("total timeout"));
    }

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
