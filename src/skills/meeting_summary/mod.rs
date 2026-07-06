pub mod recorder;
pub mod storage;
pub mod summarizer;
pub mod transcriber;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::info;

use crate::adapter::headless::speech::{OpenAiSpeechProvider, OpusSttPipeline};
use crate::adapter::TsAdapter;
use crate::config::AppConfig;
use crate::llm::LlmEngine;
use crate::skills::{ExecutionContext, Skill, UnifiedExecutionContext};

use self::recorder::{Recorder, RecordingState};
use self::summarizer::Summarizer;
use self::transcriber::Transcriber;

pub struct MeetingSummary {
    recorder: Arc<Recorder>,
    summarizer: Arc<Summarizer>,
}

impl MeetingSummary {
    pub fn new(
        config: Arc<AppConfig>,
        llm: Arc<LlmEngine>,
        prompts: &crate::config::PromptsConfig,
        ts_adapter: Arc<TsAdapter>,
    ) -> Self {
        let recorder = Arc::new(Recorder::new());
        let transcriber = Arc::new(Transcriber::new(llm.clone()));
        let summarizer = Arc::new(Summarizer::new(llm.clone()));

        // 创建STT管道（两种模式都需要opus解码）
        let stt_pipeline = Arc::new(tokio::sync::Mutex::new(OpusSttPipeline::new()));

        // STT模式需要SpeechProvider
        let speech_provider = if config.headless.stt.enabled {
            match OpenAiSpeechProvider::new(config.clone(), prompts.tts.style_prompt.clone()) {
                Ok(provider) => Some(Arc::new(provider)),
                Err(e) => {
                    tracing::warn!("创建语音提供器失败: {}", e);
                    return Self {
                        recorder,
                        summarizer,
                    };
                }
            }
        } else {
            None
        };

        // 启动音频监听任务
        let recorder_clone = recorder.clone();
        let transcriber_clone = transcriber.clone();
        let config_clone = config.clone();
        let llm_clone = llm.clone();
        tokio::spawn(async move {
            let event_rx = crate::adapter::headless::TsAdapter::subscribe_global();
            recorder::listen_for_audio(
                recorder_clone,
                event_rx,
                stt_pipeline,
                speech_provider,
                transcriber_clone,
                ts_adapter,
                config_clone,
                llm_clone,
            )
            .await;
        });

        Self {
            recorder,
            summarizer,
        }
    }

    async fn handle_command(&self, command: &str, _ctx: &ExecutionContext) -> Result<Value> {
        let cmd = command.trim().to_lowercase();

        match cmd.as_str() {
            // 支持中文和英文命令
            "开始录制" | "start recording" | "start" | "record" => {
                let result = self.recorder.start_recording().await?;
                Ok(json!({"status": "ok", "message": result}))
            }
            "结束录制" | "stop recording" | "stop" | "end" => {
                let _result = self.recorder.stop_recording().await?;

                // 获取转录文本
                let transcript = self.recorder.get_transcript_text().await;
                if transcript.is_empty() {
                    return Ok(json!({"status": "ok", "message": "录制已结束，但没有录制到内容"}));
                }

                // 生成总结
                let summary = self.summarizer.generate_summary(&transcript).await?;

                // 保存到文件
                let dir = storage::create_recording_dir()?;
                storage::save_transcript(&dir, &transcript)?;
                storage::save_summary_json(&dir, &summary)?;
                storage::save_summary_markdown(&dir, &summary)?;

                // 重置录制器
                self.recorder.reset().await;

                Ok(json!({
                    "status": "ok",
                    "message": format!("会议总结已生成并保存到: {}", dir.display()),
                    "summary": summary
                }))
            }
            "会议总结" | "meeting summary" | "summary" | "总结" => {
                let state = self.recorder.get_state().await;
                if state != RecordingState::Idle {
                    return Ok(json!({"status": "error", "message": "请先结束当前录制"}));
                }

                // 获取最后一条录制的转录文本
                let dir = storage::storage_dir().join("recordings");
                if !dir.exists() {
                    return Ok(json!({"status": "error", "message": "没有找到会议记录"}));
                }

                // 找到最新的录制目录
                let mut entries: Vec<_> = std::fs::read_dir(&dir)
                    .map_err(|e| anyhow::anyhow!("读取录制目录失败: {}", e))?
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .collect();

                entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

                if let Some(latest) = entries.first() {
                    let transcript_path = latest.path().join("transcript.txt");
                    if transcript_path.exists() {
                        let transcript = std::fs::read_to_string(&transcript_path)
                            .map_err(|e| anyhow::anyhow!("读取转录文件失败: {}", e))?;

                        let summary = self.summarizer.generate_summary(&transcript).await?;

                        // 更新总结文件
                        storage::save_summary_json(&latest.path(), &summary)?;
                        storage::save_summary_markdown(&latest.path(), &summary)?;

                        return Ok(json!({
                            "status": "ok",
                            "message": "会议总结已更新",
                            "summary": summary
                        }));
                    }
                }

                Ok(json!({"status": "error", "message": "没有找到可总结的会议记录"}))
            }
            "取消录制" | "cancel recording" | "cancel" => {
                let result = self.recorder.cancel_recording().await?;
                Ok(json!({"status": "ok", "message": result}))
            }
            _ => Ok(json!({"status": "error", "message": format!("未知命令: {}", command)})),
        }
    }
}

#[async_trait]
impl Skill for MeetingSummary {
    fn name(&self) -> &'static str {
        "meeting_summary"
    }

    fn description(&self) -> &'static str {
        "管理语音会议录制和生成会议总结。当用户想要记录会议、录制比赛、保存语音对话时使用此技能。"
    }

    fn is_enabled(&self, config: &AppConfig) -> bool {
        config.headless.stt.enabled || config.llm.omni_model
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "enum": ["start", "stop", "summary", "cancel"],
                    "description": "操作类型：start-开始录制，stop-结束录制并生成总结，summary-对已有录制生成总结，cancel-取消录制"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("缺少command参数"))?;

        self.handle_command(command, ctx).await
    }

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!(
            "MeetingSummary: unified execution, platform={:?}",
            ctx.platform
        );

        let ts_ctx = ctx.to_ts_ctx()?;
        self.execute(args, &ts_ctx).await
    }
}
