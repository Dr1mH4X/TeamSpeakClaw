use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use dashmap::DashMap;
use futures_util::StreamExt;
use serde_json::json;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::adapter::headless::speech::{
    detect_audio_format, is_speakable, pcm16_mono_to_wav_bytes, preprocess_stt_text,
    preprocess_text_message, OpenAiSpeechProvider, OpusSttPipeline,
};
use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::INTERNAL_GRPC_ADDR;
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::{LlmEngine, SessionSource, StreamCallbacks, ToolCall, ToolExecutor};
use crate::permission::PermissionGate;
use crate::router::ClientInfo;
use crate::skills::{ExecutionContext, SkillRegistry, UnifiedExecutionContext};
use voicev1::voice_service_client::VoiceServiceClient;

struct CallerContext {
    caller_id: u32,
    caller_name: String,
    caller_uid: String,
    groups: Vec<u32>,
    channel_group_id: u32,
    reply_target_mode: i32,
    reply_target_client_id: u32,
}

struct SkillExecutor<'a> {
    router: &'a VoiceRouter,
    ctx: &'a CallerContext,
}

#[async_trait]
impl ToolExecutor for SkillExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> String {
        self.router.execute_skill(call, self.ctx).await
    }
}

pub struct VoiceRouter {
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    ts_adapter: Arc<TsAdapter>,
    ts_clients: Arc<DashMap<u32, ClientInfo>>,
    audio_pipeline: Mutex<Option<OpusSttPipeline>>,
    speech_provider: Option<Arc<OpenAiSpeechProvider>>,
}

impl VoiceRouter {
    const OBS_TEXT_MAX_LEN: usize = 240;
    const STREAM_TTS_MIN_CHARS: usize = 4;
    const STREAM_TTS_WEAK_PUNCT_MIN_CHARS: usize = 8;
    const STREAM_TTS_MAX_CHARS: usize = 28;

    pub fn new(
        config: Arc<AppConfig>,
        prompts: Arc<PromptsConfig>,
        gate: Arc<PermissionGate>,
        llm: Arc<LlmEngine>,
        registry: Arc<SkillRegistry>,
        ts_adapter: Arc<TsAdapter>,
        ts_clients: Arc<DashMap<u32, ClientInfo>>,
    ) -> Self {
        let speech_provider =
            OpenAiSpeechProvider::new(config.clone(), prompts.tts.style_prompt.clone())
                .ok()
                .map(Arc::new);
        let need_audio_pipeline = config.headless.stt.enabled || config.llm.omni_model;
        Self {
            audio_pipeline: Mutex::new(need_audio_pipeline.then(OpusSttPipeline::new)),
            config,
            prompts,
            gate,
            llm,
            registry,
            ts_adapter,
            ts_clients,
            speech_provider,
        }
    }

    fn is_tts_effectively_enabled(&self) -> bool {
        self.config.headless.tts.enabled && self.speech_provider.is_some()
    }

    pub async fn run(self) -> Result<()> {
        let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
        let channel = Channel::from_shared(endpoint.clone())?.connect().await?;
        let mut client = VoiceServiceClient::new(channel);

        let req = tonic::Request::new(voicev1::SubscribeRequest {
            include_chat: true,
            include_log: true,
            include_audio: self.config.headless.stt.enabled || self.config.llm.omni_model,
        });
        let mut stream = client.subscribe_events(req).await?.into_inner();

        while let Some(item) = stream.next().await {
            match item {
                Ok(ev) => {
                    let Some(payload) = ev.payload else {
                        continue;
                    };
                    match payload {
                        voicev1::event::Payload::Chat(chat) => {
                            if let Err(e) = self.handle_chat_event(&mut client, chat).await {
                                error!("Voice router chat handling failed: {e}");
                            }
                        }
                        voicev1::event::Payload::Audio(audio) => {
                            if let Err(e) = self.handle_audio_event(&mut client, audio).await {
                                error!("Voice router audio handling failed: {e}");
                            }
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    warn!("voice event stream error: {e}");
                    break;
                }
            }
        }
        Ok(())
    }

    fn truncate_for_log(&self, text: &str) -> String {
        let max_len = Self::OBS_TEXT_MAX_LEN.max(16);
        if text.len() <= max_len {
            return text.to_string();
        }
        let mut s = text.chars().take(max_len).collect::<String>();
        s.push_str("...");
        s
    }

    fn resolve_caller_from_chat(&self, chat: &voicev1::ChatEvent) -> CallerContext {
        for item in self.ts_clients.iter() {
            let c = item.value();
            if c.nickname == chat.invoker_name {
                return CallerContext {
                    caller_id: c.clid,
                    caller_name: chat.invoker_name.clone(),
                    caller_uid: c.clid.to_string(),
                    groups: c.server_groups.clone(),
                    channel_group_id: c.channel_group_id,
                    reply_target_mode: chat.reply_target_mode,
                    reply_target_client_id: chat.reply_target_client_id,
                };
            }
        }
        CallerContext {
            caller_id: 0,
            caller_name: chat.invoker_name.clone(),
            caller_uid: 0.to_string(),
            groups: vec![],
            channel_group_id: 0,
            reply_target_mode: chat.reply_target_mode,
            reply_target_client_id: chat.reply_target_client_id,
        }
    }

    fn resolve_caller_from_audio(&self, audio: &voicev1::AudioFrameEvent) -> CallerContext {
        let reply_target_mode = match self.config.bot.default_reply_mode.as_str() {
            "channel" => 2,
            "server" => 3,
            _ => 1,
        };
        let reply_target_client_id = if reply_target_mode == 1 {
            audio.from_client_id
        } else {
            0
        };
        if let Some(item) = self.ts_clients.get(&audio.from_client_id) {
            let c = item.value();
            return CallerContext {
                caller_id: c.clid,
                caller_name: c.nickname.clone(),
                caller_uid: c.clid.to_string(),
                groups: c.server_groups.clone(),
                channel_group_id: c.channel_group_id,
                reply_target_mode,
                reply_target_client_id,
            };
        }
        CallerContext {
            caller_id: audio.from_client_id,
            caller_name: audio.from_client_name.clone(),
            caller_uid: audio.from_client_id.to_string(),
            groups: vec![],
            channel_group_id: 0,
            reply_target_mode,
            reply_target_client_id,
        }
    }

    fn should_ignore_chat(&self, chat: &voicev1::ChatEvent, caller_id: u32) -> bool {
        if chat.invoker_name == self.config.bot.nickname {
            return true;
        }
        let bot_clid = self.ts_adapter.get_bot_clid();
        bot_clid != 0 && caller_id == bot_clid
    }

    async fn execute_skill(&self, call: &ToolCall, ctx: &CallerContext) -> String {
        if let Some(skill) = self.registry.get(&call.name) {
            let exec_ctx = ExecutionContext {
                adapter: self.ts_adapter.clone(),
                clients: self.ts_clients.as_ref(),
                caller_id: ctx.caller_id,
                caller_name: ctx.caller_name.clone(),
                caller_groups: ctx.groups.clone(),
                caller_channel_group_id: ctx.channel_group_id,
                gate: self.gate.clone(),
                config: self.config.clone(),
            };
            let unified_ctx = UnifiedExecutionContext::from_ts(&exec_ctx).with_cross_adapters(
                Some(self.ts_adapter.clone()),
                Some(self.ts_clients.as_ref()),
                None,
            );
            let args = call.arguments.clone();
            let result = match skill.execute_unified(args.clone(), &unified_ctx).await {
                Ok(val) => {
                    info!(skill = %call.name, caller = %ctx.caller_name, "Voice unified skill executed successfully");
                    Ok(val)
                }
                Err(unified_err) => {
                    debug!(skill = %call.name, error = %unified_err, "Falling back to TS execution");
                    skill.execute(args, &exec_ctx).await
                }
            };
            match result {
                Ok(val) => val.to_string(),
                Err(e) => {
                    error!(skill = %call.name, error = %e, "Skill execution failed");
                    format!("Skill execution failed: {}", e)
                }
            }
        } else {
            warn!(caller = %ctx.caller_name, skill = %call.name, "Skill not found");
            "Skill not found".to_string()
        }
    }

    async fn handle_chat_event(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        chat: voicev1::ChatEvent,
    ) -> Result<()> {
        let ctx = self.resolve_caller_from_chat(&chat);
        if self.should_ignore_chat(&chat, ctx.caller_id) || !chat.should_trigger_llm {
            return Ok(());
        }
        let Some(clean_text) = preprocess_text_message(&chat.message) else {
            return Ok(());
        };
        self.handle_user_input(client, ctx, clean_text).await
    }

    async fn handle_audio_event(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        audio: voicev1::AudioFrameEvent,
    ) -> Result<()> {
        let bot_clid = self.ts_adapter.get_bot_clid();
        if bot_clid != 0 && audio.from_client_id == bot_clid {
            return Ok(());
        }
        let musicbot_name = &self.config.music_backend.musicbot_name;
        if !musicbot_name.is_empty()
            && audio
                .from_client_name
                .to_ascii_lowercase()
                .contains(&musicbot_name.to_ascii_lowercase())
        {
            return Ok(());
        }

        if self.config.llm.omni_model {
            return self.handle_omni_audio_event(client, audio).await;
        }

        let chunk = {
            let mut guard = self.audio_pipeline.lock().await;
            let Some(pipeline) = guard.as_mut() else {
                return Ok(());
            };
            pipeline.process_audio_frame(&audio)?
        };
        let Some(chunk) = chunk else {
            return Ok(());
        };

        let Some(speech_provider) = self.speech_provider.as_ref() else {
            return Ok(());
        };
        let wav = pcm16_mono_to_wav_bytes(&chunk.pcm16_mono_16k, 16_000);
        let raw_text = match speech_provider.transcribe_wav(wav).await {
            Ok(t) => t,
            Err(e) => {
                warn!("stt failed for {}: {}", chunk.speaker_name, e);
                return Ok(());
            }
        };
        let Some(text) = preprocess_stt_text(&raw_text, &self.config.headless.stt) else {
            return Ok(());
        };

        let mut ctx = self.resolve_caller_from_audio(&audio);
        ctx.caller_name = chunk.speaker_name;
        ctx.caller_id = chunk.speaker_client_id;
        self.handle_user_input(client, ctx, text).await
    }

    async fn handle_omni_audio_event(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        audio: voicev1::AudioFrameEvent,
    ) -> Result<()> {
        let chunk = {
            let mut guard = self.audio_pipeline.lock().await;
            let Some(pipeline) = guard.as_mut() else {
                return Ok(());
            };
            pipeline.process_audio_frame(&audio)?
        };
        let Some(chunk) = chunk else {
            return Ok(());
        };

        let wav_bytes = pcm16_mono_to_wav_bytes(&chunk.pcm16_mono_16k, 16_000);
        let audio_base64 = BASE64.encode(&wav_bytes);
        let audio_data = format!("data:audio/wav;base64,{}", audio_base64);
        let mut ctx = self.resolve_caller_from_audio(&audio);
        ctx.caller_name = chunk.speaker_name;
        ctx.caller_id = chunk.speaker_client_id;

        let (mut messages, tools, session_source) = self.build_omni_llm_request(&ctx, audio_data);
        let executor = SkillExecutor {
            router: self,
            ctx: &ctx,
        };
        let callbacks = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks().await?)
        } else {
            None
        };

        match self
            .llm
            .run_tool_loop(&mut messages, &tools, &executor, callbacks.as_ref())
            .await
        {
            Ok(result) => {
                if !result.content.is_empty() {
                    info!("[TS&TTS] LLM final reply: {}", &result.content);
                    self.send_reply(client, &ctx, &result.content).await?;
                    self.llm
                        .save_turn(&session_source, "[Audio message]".into(), result.content);
                }
            }
            Err(e) => {
                if let Some(ref cb) = callbacks {
                    if let Some(ref on_end) = cb.on_turn_end {
                        on_end("stop");
                    }
                }
                self.send_reply(client, &ctx, "AI backend unavailable. Please try again later.")
                    .await?;
                return Err(e.into());
            }
        };
        Ok(())
    }

    async fn handle_user_input(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: CallerContext,
        user_msg: String,
    ) -> Result<()> {
        info!(event = "voice.chat.user_message", invoker = %ctx.caller_name, clid = ctx.caller_id, message = %self.truncate_for_log(&user_msg));

        let (mut messages, tools, session_source) = self.build_llm_request(&ctx, user_msg.clone());
        let executor = SkillExecutor {
            router: self,
            ctx: &ctx,
        };

        let callbacks = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks().await?)
        } else {
            None
        };

        let result = match self
            .llm
            .run_tool_loop(&mut messages, &tools, &executor, callbacks.as_ref())
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(ref cb) = callbacks {
                    if let Some(ref on_end) = cb.on_turn_end {
                        on_end("stop");
                    }
                }
                self.send_reply(client, &ctx, "AI backend unavailable. Please try again later.")
                    .await?;
                return Err(e.into());
            }
        };

        if !result.content.is_empty() {
            info!("[TS&TTS] LLM final reply: {}", &result.content);
            self.send_reply(client, &ctx, &result.content).await?;
            self.llm
                .save_turn(&session_source, user_msg, result.content);
        }
        Ok(())
    }

    async fn build_tts_callbacks(&self) -> Result<StreamCallbacks> {
        let speech_provider = self
            .speech_provider
            .clone()
            .ok_or_else(|| anyhow::anyhow!("TTS provider missing"))?;
        let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
        let channel = Channel::from_shared(endpoint)?.connect().await?;
        let (sentence_tx, sentence_rx) = mpsc::channel::<String>(128);
        let (audio_tx, audio_rx) = mpsc::channel::<voicev1::TtsAudioChunk>(8);
        let trace_id = format!(
            "tts-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );

        let synth_audio_tx = audio_tx.clone();
        let synth_trace = trace_id.clone();
        tokio::spawn(async move {
            let mut rx = sentence_rx;
            while let Some(sentence) = rx.recv().await {
                if !is_speakable(&sentence) {
                    debug!("skipping unspeakable tts segment: {sentence}");
                    continue;
                }
                match speech_provider.synthesize(&sentence).await {
                    Ok(audio) => {
                        let codec = detect_audio_format(&audio);
                        let _ = synth_audio_tx
                            .send(voicev1::TtsAudioChunk {
                                payload: audio,
                                codec: codec.to_string(),
                                end_of_stream: false,
                                trace_id: synth_trace.clone(),
                            })
                            .await;
                    }
                    Err(e) => warn!(error = %e, "tts synthesis failed"),
                }
            }
            let _ = synth_audio_tx
                .send(voicev1::TtsAudioChunk {
                    payload: vec![],
                    codec: "mp3".to_string(),
                    end_of_stream: true,
                    trace_id: synth_trace,
                })
                .await;
        });

        tokio::spawn(async move {
            let mut tts_client = VoiceServiceClient::new(channel);
            if let Err(e) = tts_client
                .stream_tts_audio(tonic::Request::new(ReceiverStream::new(audio_rx)))
                .await
            {
                warn!("stream_tts_audio failed: {e}");
            }
        });

        let chunker = Arc::new(std::sync::Mutex::new(StreamingSentenceChunker::new(
            Self::STREAM_TTS_MIN_CHARS,
            Self::STREAM_TTS_WEAK_PUNCT_MIN_CHARS,
            Self::STREAM_TTS_MAX_CHARS,
        )));
        let shared_tx = Arc::new(std::sync::Mutex::new(Some(sentence_tx)));

        let on_text_token_shared = shared_tx.clone();
        let on_text_token_chunker = chunker.clone();
        let on_text_token = move |token: &str| {
            let Ok(mut chunker_guard) = on_text_token_chunker.lock() else {
                return;
            };
            let Ok(tx_guard) = on_text_token_shared.lock() else {
                return;
            };
            if let Some(ref tx) = *tx_guard {
                for segment in chunker_guard.push_token(token) {
                    let _ = tx.try_send(segment);
                }
            }
        };

        let on_turn_end_shared = shared_tx.clone();
        let on_turn_end_chunker = chunker.clone();
        let on_turn_end = move |finish_reason: &str| {
            if finish_reason == "stop" {
                let Ok(mut chunker_guard) = on_turn_end_chunker.lock() else {
                    return;
                };
                if let Ok(tx_guard) = on_turn_end_shared.lock() {
                    if let Some(ref tx) = *tx_guard {
                        for segment in chunker_guard.finish() {
                            let _ = tx.try_send(segment);
                        }
                    }
                }
                if let Ok(mut tx_guard) = shared_tx.lock() {
                    *tx_guard = None;
                }
            }
        };

        Ok(StreamCallbacks {
            on_text_token: Some(Box::new(on_text_token)),
            on_turn_end: Some(Box::new(on_turn_end)),
        })
    }

    fn build_llm_base_context(
        &self,
        ctx: &CallerContext,
    ) -> (String, String, Vec<serde_json::Value>) {
        let system_prompt = self.prompts.system.content.clone();
        let online_clients: Vec<_> = self.ts_clients.iter().map(|item| {
            let c = item.value();
            json!({ "clid": c.clid, "nickname": c.nickname, "uid": c.cldbid, "groups": c.server_groups, "channel_id": c.channel_id })
        }).collect();
        let user_ctx = format!(
            "User: {} (uid: {}, clid: {}, groups: {:?}, channel_group: {})\nOnline clients: {}",
            ctx.caller_name,
            ctx.caller_uid,
            ctx.caller_id,
            ctx.groups,
            ctx.channel_group_id,
            serde_json::to_string(&online_clients).unwrap_or_default()
        );
        let allowed_skills = self
            .gate
            .get_allowed_skills(&ctx.groups, ctx.channel_group_id);
        let tools = self.registry.to_tool_schemas(&allowed_skills);
        (system_prompt, user_ctx, tools)
    }

    fn build_llm_request(
        &self,
        ctx: &CallerContext,
        user_msg: String,
    ) -> (
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
        SessionSource,
    ) {
        let (system_prompt, user_ctx, tools) = self.build_llm_base_context(ctx);
        let source = SessionSource::Headless {
            caller_id: ctx.caller_id,
        };
        let messages = self
            .llm
            .build_messages(&source, &system_prompt, &user_ctx, &user_msg);
        (messages, tools, source)
    }

    fn build_omni_llm_request(
        &self,
        ctx: &CallerContext,
        audio_data: String,
    ) -> (
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
        SessionSource,
    ) {
        let (system_prompt, user_ctx, tools) = self.build_llm_base_context(ctx);
        let source = SessionSource::Headless {
            caller_id: ctx.caller_id,
        };
        let content = vec![json!({ "type": "input_audio", "input_audio": { "data": audio_data } })];
        let messages = self
            .llm
            .build_omni_messages(&source, &system_prompt, &user_ctx, content);
        (messages, tools, source)
    }

    async fn send_reply(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: &CallerContext,
        text: &str,
    ) -> Result<()> {
        let req = voicev1::NoticeRequest {
            message: text.to_string(),
            target_mode: ctx.reply_target_mode,
            target_client_id: ctx.reply_target_client_id,
        };
        let _ = client.send_notice(tonic::Request::new(req)).await?;
        Ok(())
    }
}

struct StreamingSentenceChunker {
    buffer: String,
    min_chars: usize,
    weak_punct_min_chars: usize,
    max_chars: usize,
}

impl StreamingSentenceChunker {
    fn new(min_chars: usize, weak_punct_min_chars: usize, max_chars: usize) -> Self {
        Self {
            buffer: String::new(),
            min_chars,
            weak_punct_min_chars,
            max_chars,
        }
    }

    fn push_token(&mut self, token: &str) -> Vec<String> {
        let mut out = Vec::new();
        for ch in token.chars() {
            self.buffer.push(ch);
            let len = self.buffer.chars().count();
            let strong_punct = matches!(ch, '。' | '！' | '？' | '.' | '!' | '?' | ';' | '；');
            let weak_punct = matches!(ch, '，' | ',' | '：' | ':');
            let flush = strong_punct
                || (weak_punct && len >= self.weak_punct_min_chars)
                || len >= self.max_chars;
            if flush {
                if let Some(seg) = self.take_buffer(len >= self.min_chars || len >= self.max_chars)
                {
                    out.push(seg);
                }
            }
        }
        out
    }

    fn finish(&mut self) -> Vec<String> {
        self.take_buffer(true).into_iter().collect()
    }

    fn take_buffer(&mut self, force: bool) -> Option<String> {
        let text = self.buffer.trim();
        if text.is_empty() {
            self.buffer.clear();
            return None;
        }
        if !force && text.chars().count() < self.min_chars {
            return None;
        }
        let out = text.to_string();
        self.buffer.clear();
        Some(out)
    }
}
