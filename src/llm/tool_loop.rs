use crate::llm::provider::{
    LlmProvider, LlmStreamEvent, ToolCall, MAX_TOOL_ARGUMENT_BYTES_TOTAL, MAX_TOOL_CALLS_PER_TURN,
};
use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use thiserror::Error;
use tracing::{debug, info};

const MAX_TOOL_LOOP_TURNS: usize = 16;
const MAX_TOOL_CALLS_TOTAL: usize = 32;

#[derive(Default)]
pub struct StreamCallbacks {
    pub on_text_token: Option<Box<dyn Fn(&str) + Send + Sync>>,
    pub on_turn_end: Option<Box<dyn Fn(&str) + Send + Sync>>,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> String;
}

#[derive(Error, Debug)]
pub enum ToolLoopError {
    #[error("tool loop exceeded {max_turns} model turns")]
    MaxTurnsExceeded { max_turns: usize },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug)]
pub struct ToolLoopResult {
    pub content: String,
    pub finish_reason: String,
}

#[derive(Debug)]
struct AccumulatedResult {
    text: String,
    tool_calls: Vec<ToolCall>,
    finish_reason: String,
}

async fn accumulate_stream(
    messages: &[Value],
    tools: &[Value],
    provider: &dyn LlmProvider,
    callbacks: Option<&StreamCallbacks>,
) -> Result<AccumulatedResult> {
    let mut stream = provider
        .chat_completion_stream(messages.to_vec(), tools.to_vec())
        .await?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut finish_reason = String::new();

    while let Some(event) = stream.next().await {
        match event? {
            LlmStreamEvent::Token(token) => {
                text.push_str(&token);
                if let Some(ref cb) = callbacks {
                    if let Some(ref on_token) = cb.on_text_token {
                        on_token(&token);
                    }
                }
            }
            LlmStreamEvent::Done {
                finish_reason: fr,
                tool_calls: tc,
            } => {
                finish_reason = fr.clone();
                tool_calls = tc;
                if let Some(ref cb) = callbacks {
                    if let Some(ref on_end) = cb.on_turn_end {
                        on_end(&finish_reason);
                    }
                }
                break;
            }
        }
    }

    if finish_reason.is_empty() {
        anyhow::bail!("LLM stream ended without a completion event");
    }

    Ok(AccumulatedResult {
        text,
        tool_calls,
        finish_reason,
    })
}

fn validate_tool_batch(
    finish_reason: &str,
    tool_calls: &[ToolCall],
    executed_tool_calls: usize,
    argument_bytes_total: usize,
) -> Result<usize> {
    if tool_calls.is_empty() {
        if finish_reason == "tool_calls" {
            anyhow::bail!("LLM reported tool_calls without any tool call");
        }
        return Ok(0);
    }
    if finish_reason != "tool_calls" {
        anyhow::bail!(
            "refusing tool calls with finish reason '{finish_reason}'; expected 'tool_calls'"
        );
    }
    if tool_calls.len() > MAX_TOOL_CALLS_PER_TURN {
        anyhow::bail!("tool call count exceeds the per-turn limit of {MAX_TOOL_CALLS_PER_TURN}");
    }

    let next_tool_call_count = executed_tool_calls
        .checked_add(tool_calls.len())
        .ok_or_else(|| anyhow::anyhow!("tool call count overflowed"))?;
    if next_tool_call_count > MAX_TOOL_CALLS_TOTAL {
        anyhow::bail!("tool call count exceeds the total limit of {MAX_TOOL_CALLS_TOTAL}");
    }

    let turn_argument_bytes = tool_calls.iter().try_fold(0usize, |total, call| {
        total.checked_add(call.arguments.to_string().len())
    });
    let turn_argument_bytes = turn_argument_bytes
        .ok_or_else(|| anyhow::anyhow!("tool argument byte count overflowed"))?;
    let next_argument_bytes = argument_bytes_total
        .checked_add(turn_argument_bytes)
        .ok_or_else(|| anyhow::anyhow!("tool argument byte count overflowed"))?;
    if next_argument_bytes > MAX_TOOL_ARGUMENT_BYTES_TOTAL {
        anyhow::bail!(
            "tool arguments exceed the total byte limit of {MAX_TOOL_ARGUMENT_BYTES_TOTAL}"
        );
    }

    Ok(turn_argument_bytes)
}

pub async fn run_tool_loop(
    messages: &mut Vec<Value>,
    tools: &[Value],
    provider: &dyn LlmProvider,
    executor: &dyn ToolExecutor,
    callbacks: Option<&StreamCallbacks>,
) -> Result<ToolLoopResult, ToolLoopError> {
    let mut executed_tool_calls = 0usize;
    let mut argument_bytes_total = 0usize;

    for turn in 0..MAX_TOOL_LOOP_TURNS {
        debug!(
            "Tool loop turn {}/{} (messages: {})",
            turn + 1,
            MAX_TOOL_LOOP_TURNS,
            messages.len()
        );

        let acc = accumulate_stream(messages, tools, provider, callbacks).await?;
        let turn_argument_bytes = validate_tool_batch(
            &acc.finish_reason,
            &acc.tool_calls,
            executed_tool_calls,
            argument_bytes_total,
        )?;

        if acc.tool_calls.is_empty() {
            let result = ToolLoopResult {
                content: acc.text,
                finish_reason: acc.finish_reason,
            };
            debug!(
                event = "tool_loop.completed",
                finish_reason = %result.finish_reason,
                "tool loop finished with no tool calls"
            );
            return Ok(result);
        }

        if turn + 1 == MAX_TOOL_LOOP_TURNS {
            return Err(ToolLoopError::MaxTurnsExceeded {
                max_turns: MAX_TOOL_LOOP_TURNS,
            });
        }

        executed_tool_calls += acc.tool_calls.len();
        argument_bytes_total += turn_argument_bytes;

        let assistant_tool_calls: Vec<Value> = acc
            .tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "type": "function",
                    "function": {
                        "name": tc.name,
                        "arguments": tc.arguments.to_string()
                    }
                })
            })
            .collect();

        let assistant_msg = json!({
            "role": "assistant",
            "content": acc.text,
            "tool_calls": assistant_tool_calls,
        });
        messages.push(assistant_msg);

        for call in &acc.tool_calls {
            info!(
                event = "tool_loop.execute",
                tool_name = %call.name,
                "executing tool call"
            );

            let result = executor.execute(call).await;

            info!(
                event = "tool_loop.result",
                tool_name = %call.name,
                "tool execution completed"
            );

            messages.push(json!({
                "role": "tool",
                "tool_call_id": call.id,
                "name": call.name,
                "content": result,
            }));
        }
    }

    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::{self, BoxStream};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RepeatingProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for RepeatingProvider {
        async fn chat_completion_stream(
            &self,
            _messages: Vec<Value>,
            _tools: Vec<Value>,
        ) -> Result<BoxStream<'static, Result<LlmStreamEvent>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(stream::iter([Ok(LlmStreamEvent::Done {
                finish_reason: "tool_calls".to_string(),
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "lookup".to_string(),
                    arguments: json!({}),
                }],
            })])))
        }
    }

    struct CountingExecutor {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ToolExecutor for CountingExecutor {
        async fn execute(&self, _call: &ToolCall) -> String {
            self.calls.fetch_add(1, Ordering::SeqCst);
            "ok".to_string()
        }
    }

    #[tokio::test]
    async fn stops_before_executing_unbounded_tool_calls() {
        let provider = RepeatingProvider {
            calls: AtomicUsize::new(0),
        };
        let executor = CountingExecutor {
            calls: AtomicUsize::new(0),
        };
        let mut messages = Vec::new();

        let error = run_tool_loop(&mut messages, &[], &provider, &executor, None)
            .await
            .unwrap_err();

        assert!(matches!(error, ToolLoopError::MaxTurnsExceeded { .. }));
        assert_eq!(provider.calls.load(Ordering::SeqCst), MAX_TOOL_LOOP_TURNS);
        assert_eq!(
            executor.calls.load(Ordering::SeqCst),
            MAX_TOOL_LOOP_TURNS - 1
        );
    }

    struct EmptyProvider;

    #[async_trait]
    impl LlmProvider for EmptyProvider {
        async fn chat_completion_stream(
            &self,
            _messages: Vec<Value>,
            _tools: Vec<Value>,
        ) -> Result<BoxStream<'static, Result<LlmStreamEvent>>> {
            Ok(Box::pin(stream::empty()))
        }
    }

    #[tokio::test]
    async fn rejects_stream_without_completion_event() {
        let error = accumulate_stream(&[], &[], &EmptyProvider, None)
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("ended without a completion event"));
    }

    struct FixedProvider {
        calls: AtomicUsize,
        finish_reason: &'static str,
        tool_calls: Vec<ToolCall>,
    }

    #[async_trait]
    impl LlmProvider for FixedProvider {
        async fn chat_completion_stream(
            &self,
            _messages: Vec<Value>,
            _tools: Vec<Value>,
        ) -> Result<BoxStream<'static, Result<LlmStreamEvent>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(stream::iter([Ok(LlmStreamEvent::Done {
                finish_reason: self.finish_reason.to_string(),
                tool_calls: self.tool_calls.clone(),
            })])))
        }
    }

    fn tool_calls(count: usize, arguments: Value) -> Vec<ToolCall> {
        (0..count)
            .map(|index| ToolCall {
                id: format!("call-{index}"),
                name: "lookup".to_string(),
                arguments: arguments.clone(),
            })
            .collect()
    }

    #[tokio::test]
    async fn rejects_tool_calls_with_non_tool_finish_reason() {
        let provider = FixedProvider {
            calls: AtomicUsize::new(0),
            finish_reason: "length",
            tool_calls: tool_calls(1, json!({})),
        };
        let executor = CountingExecutor {
            calls: AtomicUsize::new(0),
        };
        let mut messages = Vec::new();

        let error = run_tool_loop(&mut messages, &[], &provider, &executor, None)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("expected 'tool_calls'"));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_too_many_tool_calls_in_one_turn() {
        let provider = FixedProvider {
            calls: AtomicUsize::new(0),
            finish_reason: "tool_calls",
            tool_calls: tool_calls(MAX_TOOL_CALLS_PER_TURN + 1, json!({})),
        };
        let executor = CountingExecutor {
            calls: AtomicUsize::new(0),
        };
        let mut messages = Vec::new();

        let error = run_tool_loop(&mut messages, &[], &provider, &executor, None)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("per-turn limit"));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rejects_tool_calls_over_total_limit() {
        let provider = FixedProvider {
            calls: AtomicUsize::new(0),
            finish_reason: "tool_calls",
            tool_calls: tool_calls(MAX_TOOL_CALLS_PER_TURN, json!({})),
        };
        let executor = CountingExecutor {
            calls: AtomicUsize::new(0),
        };
        let mut messages = Vec::new();

        let error = run_tool_loop(&mut messages, &[], &provider, &executor, None)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("total limit"));
        assert_eq!(executor.calls.load(Ordering::SeqCst), MAX_TOOL_CALLS_TOTAL);
    }

    #[tokio::test]
    async fn rejects_tool_arguments_over_cumulative_byte_limit() {
        let provider = FixedProvider {
            calls: AtomicUsize::new(0),
            finish_reason: "tool_calls",
            tool_calls: tool_calls(
                1,
                Value::String("x".repeat(MAX_TOOL_ARGUMENT_BYTES_TOTAL / 2)),
            ),
        };
        let executor = CountingExecutor {
            calls: AtomicUsize::new(0),
        };
        let mut messages = Vec::new();

        let error = run_tool_loop(&mut messages, &[], &provider, &executor, None)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("total byte limit"));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }
}
