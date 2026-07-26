use crate::config::AppConfig;
use crate::llm::context::{ContextWindow, SessionSource};
use crate::llm::provider::{LlmProvider, OpenAiProvider};
use crate::llm::tool_loop::{
    run_tool_loop, StreamCallbacks, ToolExecutor, ToolLoopError, ToolLoopResult,
};
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub struct LlmEngine {
    provider: Box<dyn LlmProvider>,
    context: ContextWindow,
    request_limit: Semaphore,
}

impl LlmEngine {
    pub fn new(config: Arc<AppConfig>) -> Self {
        let cfg = &config;
        let provider = Box::new(OpenAiProvider::new(cfg.llm.clone()));
        let context = ContextWindow::new(cfg.llm.max_context_turns, cfg.llm.max_context_sessions);
        let request_limit = Semaphore::new(cfg.llm.max_concurrent_requests);
        Self {
            provider,
            context,
            request_limit,
        }
    }

    pub async fn run_tool_loop(
        &self,
        messages: &mut Vec<Value>,
        tools: &[Value],
        executor: &dyn ToolExecutor,
        callbacks: Option<&StreamCallbacks>,
    ) -> Result<ToolLoopResult, ToolLoopError> {
        let _permit = self
            .request_limit
            .acquire()
            .await
            .map_err(|error| anyhow::anyhow!("LLM request limit closed: {error}"))?;
        run_tool_loop(messages, tools, self.provider.as_ref(), executor, callbacks).await
    }

    /// 构建系统提示 + 可选的上下文历史（不含最后一条用户消息）
    fn build_context_base(
        &self,
        source: &SessionSource,
        system_prompt: &str,
        user_ctx: &str,
    ) -> Vec<Value> {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let system_prompt = system_prompt.replace("{date}", &date);
        let system_content = format!("{system_prompt}\n\n{user_ctx}");
        let mut messages = vec![json!({"role": "system", "content": system_content})];

        if self.context.is_enabled() {
            let history = self.context.get(source);
            for turn in history {
                messages.push(json!({"role": "user", "content": turn.user}));
                messages.push(json!({"role": "assistant", "content": turn.assistant}));
            }
        }

        messages
    }

    /// 构建带历史上下文的 messages
    pub fn build_messages(
        &self,
        source: &SessionSource,
        system_prompt: &str,
        user_ctx: &str,
        user_msg: &str,
    ) -> Vec<Value> {
        let mut messages = self.build_context_base(source, system_prompt, user_ctx);
        messages.push(json!({"role": "user", "content": user_msg}));
        messages
    }

    /// 构建带历史上下文的 omni messages（用户消息为 audio content）
    pub fn build_omni_messages(
        &self,
        source: &SessionSource,
        system_prompt: &str,
        user_ctx: &str,
        user_content: Vec<Value>,
    ) -> Vec<Value> {
        let mut messages = self.build_context_base(source, system_prompt, user_ctx);
        messages.push(json!({"role": "user", "content": user_content}));
        messages
    }

    /// 保存一轮对话到上下文
    pub fn save_turn(&self, source: &SessionSource, user: String, assistant: String) {
        self.context
            .push(source, crate::llm::context::ContextTurn { user, assistant });
    }
}
