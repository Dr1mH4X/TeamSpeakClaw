use anyhow::{Context, Result};
use serde_json::json;
use std::sync::Arc;
use tracing::warn;

use crate::llm::LlmEngine;

use super::storage::MeetingSummaryData;

pub struct Summarizer {
    llm: Arc<LlmEngine>,
}

impl Summarizer {
    pub fn new(llm: Arc<LlmEngine>) -> Self {
        Self { llm }
    }

    pub async fn generate_summary(&self, transcript: &str) -> Result<MeetingSummaryData> {
        let prompt = format!(
            r#"请根据以下会议转录生成总结。要求：
1. 使用与转录文本相同的语言
2. 提取讨论要点、行动项、决策记录
3. 标注发言人
4. 生成一个简洁的会议标题
5. 以JSON格式返回，包含以下字段：
   - title: 会议标题
   - time_range: 时间范围（如果无法确定，使用"未知"）
   - participants: 参与者列表
   - discussion_points: 讨论要点数组，每个包含topic, summary, speakers
   - action_items: 行动项数组，每个包含task, assignee, deadline（可为null）
   - decisions: 决策数组，每个包含decision, context, participants
   - full_transcript: 完整转录文本

只返回JSON，不要添加任何解释。

转录内容：
{}"#,
            transcript
        );

        let messages = vec![json!({"role": "user", "content": prompt})];

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
                    return Err(anyhow::anyhow!("LLM返回空内容"));
                }

                // 尝试解析JSON
                let json_str = extract_json(&r.content);
                let data: MeetingSummaryData =
                    serde_json::from_str(&json_str).context("解析总结JSON失败")?;

                Ok(data)
            }
            Err(e) => {
                warn!("生成总结失败: {}", e);
                Err(e.into())
            }
        }
    }
}

fn extract_json(text: &str) -> String {
    // 尝试找到JSON块
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            return text[start..=end].to_string();
        }
    }
    // 如果没有找到，返回原始文本
    text.to_string()
}

struct NoopExecutor;

#[async_trait::async_trait]
impl crate::llm::tool_loop::ToolExecutor for NoopExecutor {
    async fn execute(&self, _call: &crate::llm::ToolCall) -> String {
        "未实现".to_string()
    }
}
