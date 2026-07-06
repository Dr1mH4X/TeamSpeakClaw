use anyhow::Result;
use serde_json::json;
use std::sync::Arc;
use tracing::warn;

use crate::llm::LlmEngine;

pub struct Transcriber {
    llm: Arc<LlmEngine>,
}

impl Transcriber {
    pub fn new(llm: Arc<LlmEngine>) -> Self {
        Self { llm }
    }

    pub async fn correct_stt_errors(&self, raw_text: &str) -> Result<String> {
        let prompt = format!(
            r#"你是一个专业的语音转录纠错助手。请修正以下语音转录文本中的错误：
- 修正同音字错误
- 修正语法错误
- 保持原意不变
- 保留发言人信息
- 只输出修正后的文本，不要添加任何解释

转录文本：
{}"#,
            raw_text
        );

        let messages = vec![json!({"role": "user", "content": prompt})];

        // 使用LLM进行纠错
        let result = self
            .llm
            .run_tool_loop(
                &mut messages.clone(),
                &[], // 不需要工具
                &NoopExecutor,
                None,
            )
            .await;

        match result {
            Ok(r) => {
                if r.content.is_empty() {
                    Ok(raw_text.to_string())
                } else {
                    Ok(r.content)
                }
            }
            Err(e) => {
                warn!("LLM纠错失败，使用原始文本: {}", e);
                Ok(raw_text.to_string())
            }
        }
    }
}

pub struct NoopExecutor;

#[async_trait::async_trait]
impl crate::llm::tool_loop::ToolExecutor for NoopExecutor {
    async fn execute(&self, _call: &crate::llm::ToolCall) -> String {
        "未实现".to_string()
    }
}
