use std::sync::Arc;
use std::time::{Duration, Instant};
use base64::Engine;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::adapter::headless::speech::{
    pcm16_mono_to_wav_bytes, OpenAiSpeechProvider, OpusSttPipeline,
};
use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::{TsAdapter, TsEvent};
use crate::llm::LlmEngine;

use super::transcriber::Transcriber;

#[derive(Debug, Clone, PartialEq)]
pub enum RecordingState {
    Idle,
    Recording,
    Processing,
}

#[derive(Clone)]
pub struct Recorder {
    state: Arc<tokio::sync::RwLock<RecordingState>>,
    transcript: Arc<tokio::sync::RwLock<Vec<TranscriptEntry>>>,
    start_time: Arc<tokio::sync::RwLock<Option<Instant>>>,
    last_audio_time: Arc<tokio::sync::RwLock<Option<Instant>>>,
    empty_channel_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct TranscriptEntry {
    pub speaker_name: String,
    pub text: String,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            state: Arc::new(tokio::sync::RwLock::new(RecordingState::Idle)),
            transcript: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            start_time: Arc::new(tokio::sync::RwLock::new(None)),
            last_audio_time: Arc::new(tokio::sync::RwLock::new(None)),
            empty_channel_timeout: Duration::from_secs(60), // 频道内无人60秒后自动停止
        }
    }

    pub async fn start_recording(&self) -> anyhow::Result<String> {
        let mut state = self.state.write().await;
        if *state != RecordingState::Idle {
            return Err(anyhow::anyhow!("已有录制在进行中"));
        }

        *state = RecordingState::Recording;
        drop(state);

        let mut transcript = self.transcript.write().await;
        transcript.clear();
        drop(transcript);

        let mut start_time = self.start_time.write().await;
        *start_time = Some(Instant::now());
        drop(start_time);

        let mut last_audio_time = self.last_audio_time.write().await;
        *last_audio_time = Some(Instant::now());
        drop(last_audio_time);

        info!("会议录制已开始");
        Ok("录制已开始".to_string())
    }

    pub async fn stop_recording(&self) -> anyhow::Result<String> {
        let mut state = self.state.write().await;
        if *state != RecordingState::Recording {
            return Err(anyhow::anyhow!("没有正在进行的录制"));
        }

        *state = RecordingState::Processing;
        drop(state);

        let start_time = self.start_time.read().await;
        let duration = start_time.map(|t| t.elapsed()).unwrap_or_default();
        drop(start_time);

        info!("会议录制已停止，时长: {:?}", duration);
        Ok(format!("录制已停止，时长: {:?}", duration))
    }

    pub async fn cancel_recording(&self) -> anyhow::Result<String> {
        let mut state = self.state.write().await;
        if *state != RecordingState::Recording {
            return Err(anyhow::anyhow!("没有正在进行的录制"));
        }

        *state = RecordingState::Idle;
        drop(state);

        let mut transcript = self.transcript.write().await;
        transcript.clear();
        drop(transcript);

        let mut start_time = self.start_time.write().await;
        *start_time = None;
        drop(start_time);

        let mut last_audio_time = self.last_audio_time.write().await;
        *last_audio_time = None;

        info!("会议录制已取消");
        Ok("录制已取消".to_string())
    }

    pub async fn get_state(&self) -> RecordingState {
        self.state.read().await.clone()
    }

    pub async fn update_last_audio_time(&self) {
        let mut last_audio_time = self.last_audio_time.write().await;
        *last_audio_time = Some(Instant::now());
    }

    pub async fn handle_client_leave(
        &self,
        ts_adapter: Arc<TsAdapter>,
        config: Arc<crate::config::AppConfig>,
        _client_id: u32,
    ) {
        let state = self.state.read().await;
        if *state != RecordingState::Recording {
            return;
        }
        drop(state);

        match ts_adapter.list_clients().await {
            Ok(clients) => {
                let bot_clid = ts_adapter.get_bot_clid();
                let bot_channel_id = clients
                    .iter()
                    .find(|c| c.id as u32 == bot_clid)
                    .map(|c| c.channel_id);
                let musicbot_name = config.music_backend.musicbot_name.clone();

                let non_bot_clients: Vec<_> = clients
                    .iter()
                    .filter(|c| {
                        let clid = c.id as u32;
                        if clid == bot_clid {
                            return false;
                        }
                        // 只统计机器人所在频道的用户
                        if let Some(ch) = bot_channel_id {
                            if c.channel_id != ch {
                                return false;
                            }
                        }
                        if !musicbot_name.is_empty()
                            && c.nickname
                                .to_ascii_lowercase()
                                .contains(&musicbot_name.to_ascii_lowercase())
                        {
                            return false;
                        }
                        true
                    })
                    .collect();

                if non_bot_clients.is_empty() {
                    info!(
                        "频道内用户（除机器人外）已全部离开，启动{:?}超时计时器",
                        self.empty_channel_timeout
                    );
                    let recorder = self.clone();
                    let ts_adapter = ts_adapter.clone();
                    let musicbot_name = musicbot_name.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(recorder.empty_channel_timeout).await;
                        if let Ok(clients) = ts_adapter.list_clients().await {
                            let bot_channel_id = clients
                                .iter()
                                .find(|c| c.id as u32 == bot_clid)
                                .map(|c| c.channel_id);
                            let non_bot_clients: Vec<_> = clients
                                .iter()
                                .filter(|c| {
                                    let clid = c.id as u32;
                                    if clid == bot_clid {
                                        return false;
                                    }
                                    if let Some(ch) = bot_channel_id {
                                        if c.channel_id != ch {
                                            return false;
                                        }
                                    }
                                    if !musicbot_name.is_empty()
                                        && c.nickname
                                            .to_ascii_lowercase()
                                            .contains(&musicbot_name.to_ascii_lowercase())
                                    {
                                        return false;
                                    }
                                    true
                                })
                                .collect();

                            if non_bot_clients.is_empty() {
                                info!(
                                    "频道内无人超过{:?}，自动停止录制",
                                    recorder.empty_channel_timeout
                                );
                                recorder.reset().await;
                            }
                        }
                    });
                }
            }
            Err(e) => {
                warn!("获取频道用户列表失败: {}", e);
            }
        }
    }

    pub async fn add_transcript_entry(&self, entry: TranscriptEntry) {
        let state = self.state.read().await;
        if *state != RecordingState::Recording {
            return;
        }
        drop(state);

        // 更新最后音频时间
        self.update_last_audio_time().await;

        let mut transcript = self.transcript.write().await;
        transcript.push(entry);
    }

    pub async fn get_transcript_text(&self) -> String {
        let transcript = self.transcript.read().await;
        transcript
            .iter()
            .map(|entry| format!("{}: {}", entry.speaker_name, entry.text))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub async fn reset(&self) {
        let mut state = self.state.write().await;
        *state = RecordingState::Idle;
        drop(state);

        let mut transcript = self.transcript.write().await;
        transcript.clear();
        drop(transcript);

        let mut start_time = self.start_time.write().await;
        *start_time = None;

        let mut last_audio_time = self.last_audio_time.write().await;
        *last_audio_time = None;
    }
}

/// 监听音频事件并处理录制
pub async fn listen_for_audio(
    recorder: Arc<Recorder>,
    mut event_rx: broadcast::Receiver<TsEvent>,
    stt_pipeline: Arc<tokio::sync::Mutex<OpusSttPipeline>>,
    speech_provider: Option<Arc<OpenAiSpeechProvider>>,
    transcriber: Arc<Transcriber>,
    ts_adapter: Arc<TsAdapter>,
    config: Arc<crate::config::AppConfig>,
    llm: Arc<LlmEngine>,
) {
    loop {
        match event_rx.recv().await {
            Ok(event) => {
                match event {
                    TsEvent::AudioFrame(audio_data) => {
                        // 检查录制状态
                        let state = recorder.get_state().await;
                        if state != RecordingState::Recording {
                            continue;
                        }

                        // 解码音频帧
                        let audio_event = voicev1::AudioFrameEvent {
                            from_client_id: audio_data.from_client_id,
                            from_client_name: audio_data.from_client_name.clone(),
                            frame: audio_data.frame,
                            codec: audio_data.codec,
                            is_whisper: false,
                        };

                        let chunk = {
                            let mut guard = stt_pipeline.lock().await;
                            match guard.process_audio_frame(&audio_event) {
                                Ok(Some(chunk)) => Some(chunk),
                                Ok(None) => None,
                                Err(e) => {
                                    warn!("音频解码失败: {}", e);
                                    None
                                }
                            }
                        };

                        if let Some(chunk) = chunk {
                            let wav = pcm16_mono_to_wav_bytes(&chunk.pcm16_mono_16k, 16_000);

                            if let Some(provider) = speech_provider.as_ref() {
                                // STT模式：转录 → LLM纠错 → 记录
                                match provider.transcribe_wav(wav.clone()).await {
                                    Ok(raw_text) => {
                                        if !raw_text.is_empty() {
                                            let text =
                                                match transcriber.correct_stt_errors(&raw_text).await {
                                                    Ok(corrected) => corrected,
                                                    Err(e) => {
                                                        warn!("LLM纠错失败，使用原始文本: {}", e);
                                                        raw_text
                                                    }
                                                };
                                            let entry = TranscriptEntry {
                                                speaker_name: chunk.speaker_name,
                                                text,
                                            };
                                            recorder.add_transcript_entry(entry).await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!("STT转录失败: {}", e);
                                    }
                                }
                            } else if config.llm.omni_model {
                                // omni模式：直接发送音频给多模态模型总结
                                let audio_base64 = base64::engine::general_purpose::STANDARD.encode(&wav);
                                let audio_data = format!("data:audio/wav;base64,{}", audio_base64);
                                let prompt = format!(
                                    "请转录以下音频内容，只输出转录文本，不要添加任何解释。发言人: {}",
                                    chunk.speaker_name
                                );
                                let messages = vec![
                                    serde_json::json!({"role": "user", "content": [
                                        serde_json::json!({"type": "text", "text": prompt}),
                                        serde_json::json!({"type": "input_audio", "input_audio": {"data": audio_data, "format": "wav"}})
                                    ]})
                                ];
                                let mut msgs = messages;
                                match llm.run_tool_loop(
                                    &mut msgs,
                                    &[],
                                    &super::transcriber::NoopExecutor,
                                    None,
                                ).await {
                                    Ok(r) => {
                                        if !r.content.is_empty() {
                                            let entry = TranscriptEntry {
                                                speaker_name: chunk.speaker_name,
                                                text: r.content,
                                            };
                                            recorder.add_transcript_entry(entry).await;
                                        }
                                    }
                                    Err(e) => {
                                        warn!("omni模型处理失败: {}", e);
                                    }
                                }
                            }
                        }
                    }
                    TsEvent::ClientLeave(left_event) => {
                        // 处理客户端离开事件
                        recorder
                            .handle_client_leave(
                                ts_adapter.clone(),
                                config.clone(),
                                left_event.client_id,
                            )
                            .await;
                    }
                    _ => {}
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("音频事件流落后 {} 帧", n);
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!("音频事件流已关闭");
                break;
            }
        }
    }
}
