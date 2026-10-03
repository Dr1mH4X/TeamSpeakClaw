use crate::llm::provider::{
    LlmProvider, LlmStreamEvent, ToolCall, MAX_TOOL_ARGUMENT_BYTES_TOTAL, MAX_TOOL_CALLS_PER_TURN,
};
use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

const MAX_TOOL_LOOP_TURNS: usize = 16;
const MAX_TOOL_CALLS_TOTAL: usize = 32;

/// 异步回调类型：可等待完成，允许下游执行背压（如 send().await）
/// 回调返回 'static future，内部自行克隆所需数据
pub type AsyncTokenCallback =
    Box<dyn Fn(&str) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// 流式与工具循环回调；各槽位的实参不同：文本槽收 token 或 finish_reason，
/// 工具槽收工具名（`on_tool_call_start` 在该工具实际执行之前）
#[derive(Default)]
pub struct StreamCallbacks {
    pub on_text_token: Option<AsyncTokenCallback>,
    pub on_turn_end: Option<AsyncTokenCallback>,
    /// 工具开始执行：调用方可据此播一句短反馈
    pub on_tool_call_start: Option<AsyncTokenCallback>,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> String;
}

#[derive(Error, Debug)]
pub enum ToolLoopError {
    #[error("tool loop exceeded {max_turns} model turns")]
    MaxTurnsExceeded { max_turns: usize },
    /// 插话取消：调用方不回错误文案、不落上下文
    #[error("tool loop cancelled")]
    Cancelled,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug)]
pub struct ToolLoopResult {
    pub content: String,
    finish_reason: String,
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
    cancel: &CancellationToken,
) -> Result<AccumulatedResult, ToolLoopError> {
    // 连接期与流读取期都可取消：插话不必等当前流自然结束
    let mut stream = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ToolLoopError::Cancelled),
        stream = provider.chat_completion_stream(messages.to_vec(), tools.to_vec()) => stream?,
    };
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut finish_reason = String::new();

    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ToolLoopError::Cancelled),
            event = stream.next() => event,
        };
        let Some(event) = event else {
            break;
        };
        match event? {
            LlmStreamEvent::Token(token) => {
                text.push_str(&token);
                if let Some(cb) = callbacks {
                    if let Some(ref on_token) = cb.on_text_token {
                        on_token(&token).await;
                    }
                }
            }
            LlmStreamEvent::Done {
                finish_reason: fr,
                tool_calls: tc,
            } => {
                finish_reason = fr.clone();
                tool_calls = tc;
                if let Some(cb) = callbacks {
                    if let Some(ref on_end) = cb.on_turn_end {
                        on_end(&finish_reason).await;
                    }
                }
                break;
            }
        }
    }

    if finish_reason.is_empty() {
        return Err(ToolLoopError::Other(anyhow::anyhow!(
            "LLM stream ended without a completion event"
        )));
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
    cancel: &CancellationToken,
) -> Result<ToolLoopResult, ToolLoopError> {
    let mut executed_tool_calls = 0usize;
    let mut argument_bytes_total = 0usize;

    for turn in 0..MAX_TOOL_LOOP_TURNS {
        if cancel.is_cancelled() {
            return Err(ToolLoopError::Cancelled);
        }
        debug!(
            "Tool loop turn {}/{} (messages: {})",
            turn + 1,
            MAX_TOOL_LOOP_TURNS,
            messages.len()
        );

        let acc = accumulate_stream(messages, tools, provider, callbacks, cancel).await?;
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
            if cancel.is_cancelled() {
                return Err(ToolLoopError::Cancelled);
            }
            info!(
                event = "tool_loop.execute",
                tool_name = %call.name,
                "executing tool call"
            );

            if let Some(cb) = callbacks {
                if let Some(ref on_start) = cb.on_tool_call_start {
                    on_start(&call.name).await;
                }
            }

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
    use std::sync::{Arc, Mutex};

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

        let error = run_tool_loop(
            &mut messages,
            &[],
            &provider,
            &executor,
            None,
            &CancellationToken::new(),
        )
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
        let error = accumulate_stream(&[], &[], &EmptyProvider, None, &CancellationToken::new())
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("ended without a completion event"));
    }

    /// 取消在流中途生效：不等当前流自然结束
    #[tokio::test]
    async fn cancel_interrupts_a_running_stream() {
        struct EndlessProvider;

        #[async_trait]
        impl LlmProvider for EndlessProvider {
            async fn chat_completion_stream(
                &self,
                _messages: Vec<Value>,
                _tools: Vec<Value>,
            ) -> Result<BoxStream<'static, Result<LlmStreamEvent>>> {
                Ok(Box::pin(stream::unfold((), |_| async {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    Some((Ok(LlmStreamEvent::Token("t".to_string())), ()))
                })))
            }
        }

        let executor = CountingExecutor {
            calls: AtomicUsize::new(0),
        };
        let mut messages = Vec::new();
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            canceller.cancel();
        });

        let error = run_tool_loop(
            &mut messages,
            &[],
            &EndlessProvider,
            &executor,
            None,
            &cancel,
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ToolLoopError::Cancelled));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
    }

    /// 已取消的令牌在调用模型前就短路，不产生请求
    #[tokio::test]
    async fn cancel_before_the_turn_skips_the_model_call() {
        let provider = FixedProvider {
            calls: AtomicUsize::new(0),
            finish_reason: "stop",
            tool_calls: Vec::new(),
        };
        let executor = CountingExecutor {
            calls: AtomicUsize::new(0),
        };
        let mut messages = Vec::new();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let error = run_tool_loop(&mut messages, &[], &provider, &executor, None, &cancel)
            .await
            .unwrap_err();

        assert!(matches!(error, ToolLoopError::Cancelled));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
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

        let error = run_tool_loop(
            &mut messages,
            &[],
            &provider,
            &executor,
            None,
            &CancellationToken::new(),
        )
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

        let error = run_tool_loop(
            &mut messages,
            &[],
            &provider,
            &executor,
            None,
            &CancellationToken::new(),
        )
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

        let error = run_tool_loop(
            &mut messages,
            &[],
            &provider,
            &executor,
            None,
            &CancellationToken::new(),
        )
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

        let error = run_tool_loop(
            &mut messages,
            &[],
            &provider,
            &executor,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("total byte limit"));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }

    struct ToolThenStopProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for ToolThenStopProvider {
        async fn chat_completion_stream(
            &self,
            _messages: Vec<Value>,
            _tools: Vec<Value>,
        ) -> Result<BoxStream<'static, Result<LlmStreamEvent>>> {
            let turn = self.calls.fetch_add(1, Ordering::SeqCst);
            let event = if turn == 0 {
                LlmStreamEvent::Done {
                    finish_reason: "tool_calls".to_string(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".to_string(),
                        name: "web_search".to_string(),
                        arguments: json!({}),
                    }],
                }
            } else {
                LlmStreamEvent::Done {
                    finish_reason: "stop".to_string(),
                    tool_calls: Vec::new(),
                }
            };
            Ok(Box::pin(stream::iter([Ok(event)])))
        }
    }

    /// `StreamCallbacks` 只有 `on_tool_call_start` 一个工具回调槽（没有配套的结束回调），
    /// 因此只断言「开始 → 执行」的先后；实参是工具名
    #[tokio::test]
    async fn tool_callbacks_wrap_tool_execution_in_order() {
        struct RecordingExecutor {
            events: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl ToolExecutor for RecordingExecutor {
            async fn execute(&self, call: &ToolCall) -> String {
                self.events
                    .lock()
                    .expect("event log")
                    .push(format!("execute:{}", call.name));
                "ok".to_string()
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let recorder = |slot: &'static str| {
            let events = events.clone();
            Box::new(move |name: &str| {
                let events = events.clone();
                let entry = format!("{slot}:{name}");
                Box::pin(async move {
                    events.lock().expect("event log").push(entry);
                }) as Pin<Box<dyn Future<Output = ()> + Send>>
            }) as AsyncTokenCallback
        };
        let callbacks = StreamCallbacks {
            on_tool_call_start: Some(recorder("start")),
            ..StreamCallbacks::default()
        };
        let executor = RecordingExecutor {
            events: events.clone(),
        };
        let provider = ToolThenStopProvider {
            calls: AtomicUsize::new(0),
        };
        let mut messages = Vec::new();

        run_tool_loop(
            &mut messages,
            &[],
            &provider,
            &executor,
            Some(&callbacks),
            &CancellationToken::new(),
        )
        .await
        .expect("tool loop completes");

        assert_eq!(
            *events.lock().expect("event log"),
            vec![
                "start:web_search".to_string(),
                "execute:web_search".to_string()
            ]
        );
    }
}
