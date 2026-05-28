use crate::config::AppConfig;
use crate::llm::context::{ContextWindow, SessionSource};
use crate::llm::provider::{LlmProvider, OpenAiProvider};
use crate::llm::tool_loop::{
    run_tool_loop, StreamCallbacks, ToolExecutor, ToolLoopError, ToolLoopResult,
};
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct LlmEngine {
    provider: Box<dyn LlmProvider>,
    context: ContextWindow,
}

impl LlmEngine {
    pub fn new(config: Arc<AppConfig>) -> Self {
        let cfg = &config;
        let provider = Box::new(OpenAiProvider::new(cfg.llm.clone()));
        let context = ContextWindow::new(cfg.llm.max_context_turns, cfg.llm.max_context_sessions);
        Self { provider, context }
    }

    pub async fn run_tool_loop(
        &self,
        messages: &mut Vec<Value>,
        tools: &[Value],
        executor: &dyn ToolExecutor,
        max_turns: u32,
        callbacks: Option<&StreamCallbacks>,
    ) -> Result<ToolLoopResult, ToolLoopError> {
        run_tool_loop(
            messages,
            tools,
            self.provider.as_ref(),
            executor,
            max_turns as usize,
            callbacks,
        )
        .await
    }

    /// 构建带历史上下文的 messages
    pub fn build_messages(
        &self,
        source: &SessionSource,
        system_prompt: &str,
        user_ctx: &str,
        user_msg: &str,
    ) -> Vec<Value> {
        let mut messages = vec![
            json!({"role": "system", "content": system_prompt}),
            json!({"role": "system", "content": user_ctx}),
        ];

        if self.context.is_enabled() {
            let history = self.context.get(source);
            for turn in history {
                messages.push(json!({"role": "user", "content": turn.user}));
                messages.push(json!({"role": "assistant", "content": turn.assistant}));
            }
        }

        messages.push(json!({"role": "user", "content": user_msg}));

        messages
    }

    /// 保存一轮对话到上下文
    pub fn save_turn(&self, source: &SessionSource, user: String, assistant: String) {
        self.context
            .push(source, crate::llm::context::ContextTurn { user, assistant });
    }
}
