use crate::llm::provider::{LlmProvider, LlmStreamEvent, ToolCall};
use anyhow::Result;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tracing::{debug, info};

#[derive(Default)]
pub struct StreamCallbacks {
    pub on_text_token: Option<Box<dyn Fn(&str) + Send + Sync>>,
    pub on_turn_end: Option<Box<dyn Fn(&str) + Send + Sync>>,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> String;
}

#[allow(dead_code)]
pub struct ToolLoopResult {
    pub content: String,
    pub reasoning: String,
    pub finish_reason: String,
}

struct AccumulatedResult {
    text: String,
    reasoning: String,
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
    let reasoning = String::new();
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

    Ok(AccumulatedResult {
        text,
        reasoning,
        tool_calls,
        finish_reason,
    })
}

pub async fn run_tool_loop(
    messages: &mut Vec<Value>,
    tools: &[Value],
    provider: &dyn LlmProvider,
    executor: &dyn ToolExecutor,
    max_turns: usize,
    callbacks: Option<&StreamCallbacks>,
) -> Result<ToolLoopResult> {
    for turn in 0..max_turns {
        debug!(
            "Tool loop turn {}/{} (messages: {})",
            turn + 1,
            max_turns,
            messages.len()
        );

        let acc = accumulate_stream(messages, tools, provider, callbacks).await?;

        if acc.tool_calls.is_empty() {
            return Ok(ToolLoopResult {
                content: acc.text,
                reasoning: acc.reasoning,
                finish_reason: acc.finish_reason,
            });
        }

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
                tool_call_id = %call.id,
                "executing tool call"
            );

            let result = executor.execute(call).await;

            info!(
                event = "tool_loop.result",
                tool_name = %call.name,
                tool_call_id = %call.id,
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

    Err(anyhow::anyhow!("max tool turns exceeded"))
}
