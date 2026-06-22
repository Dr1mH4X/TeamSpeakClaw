use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use dashmap::DashMap;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::adapter::headless::speech::{
    detect_audio_format, pcm16_mono_to_wav_bytes, preprocess_stt_text, preprocess_text_message,
    OpenAiSpeechProvider, OpusSttPipeline,
};
use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::INTERNAL_GRPC_ADDR;
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::{
    LlmEngine, SessionSource, StreamCallbacks, ToolCall, ToolExecutor, ToolLoopError,
};
use crate::permission::PermissionGate;
use crate::router::ClientInfo;
use crate::skills::music::{PLAY_TITLE_KEY, PLAY_URL_KEY};
use crate::skills::{ExecutionContext, SkillRegistry, UnifiedExecutionContext};
use voicev1::voice_service_client::VoiceServiceClient;

fn validate_play_url(raw: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(raw).map_err(|e| anyhow::anyhow!("invalid play URL: {e}"))?;

    match parsed.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(anyhow::anyhow!(
                "play URL scheme '{scheme}' is not allowed; only http/https are accepted"
            ))
        }
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("play URL has no host"))?;

    if host.eq_ignore_ascii_case("localhost") {
        return Err(anyhow::anyhow!("play URL must not point to localhost"));
    }

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if ip.is_loopback() {
            return Err(anyhow::anyhow!(
                "play URL must not point to a loopback address"
            ));
        }
        if is_private_ip(ip) {
            return Err(anyhow::anyhow!(
                "play URL must not point to a private network address"
            ));
        }
    }

    Ok(())
}

fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 10
                || (octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31)
                || (octets[0] == 192 && octets[1] == 168)
                || (octets[0] == 169 && octets[1] == 254)
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80
        }
    }
}

struct CallerContext {
    caller_id: u32,
    caller_name: String,
    caller_uid: String,
    groups: Vec<u32>,
    channel_group_id: u32,
    reply_target_mode: i32,
    reply_target_client_id: u32,
}

struct PendingPlay {
    source_url: String,
    title: String,
    requested_by: String,
}

struct HeadlessExecutor<'a> {
    bridge: &'a HeadlessLlmBridge,
    ctx: &'a CallerContext,
    pending_play: Arc<Mutex<Option<PendingPlay>>>,
}

#[async_trait]
impl ToolExecutor for HeadlessExecutor<'_> {
    async fn execute(&self, call: &ToolCall) -> String {
        let tool_result = self.bridge.execute_skill(call, self.ctx).await;

        if let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(&tool_result) {
            if let Some(url) = parsed.get(PLAY_URL_KEY).and_then(|v| v.as_str()) {
                let title = parsed
                    .get(PLAY_TITLE_KEY)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                debug!(
                    event = "headless.play_url",
                    title = %title,
                    "embedded backend requested playback"
                );
                match validate_play_url(url) {
                    Err(e) => {
                        warn!("rejected play URL from tool result: {e}");
                    }
                    Ok(()) => {
                        let mut guard = self.pending_play.lock().await;
                        *guard = Some(PendingPlay {
                            source_url: url.to_string(),
                            title: title.clone(),
                            requested_by: self.ctx.caller_name.clone(),
                        });
                    }
                }
                if let Some(obj) = parsed.as_object_mut() {
                    obj.remove(PLAY_URL_KEY);
                    obj.remove(PLAY_TITLE_KEY);
                }
            }
            match serde_json::to_string(&parsed) {
                Ok(s) => s,
                Err(e) => {
                    warn!("failed to re-serialize tool result after stripping play fields: {e}");
                    json!({
                        "error": "serialization_failed",
                        "raw": tool_result,
                    })
                    .to_string()
                }
            }
        } else {
            tool_result
        }
    }
}

pub struct HeadlessLlmBridge {
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    ts_adapter: Arc<TsAdapter>,
    ts_clients: Arc<DashMap<u32, ClientInfo>>,
    audio_pipeline: Mutex<Option<OpusSttPipeline>>,
    speech_provider: Option<Arc<OpenAiSpeechProvider>>,
    playback_status_cache: Mutex<(Instant, bool)>,
}

impl HeadlessLlmBridge {
    const OBS_CHAT: bool = true;
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
            playback_status_cache: Mutex::new((Instant::now(), false)),
        }
    }

    fn is_tts_effectively_enabled(&self) -> bool {
        self.config.headless.tts.enabled && self.speech_provider.is_some()
    }

    async fn should_ignore_stt_while_playing(
        &self,
        client: &mut VoiceServiceClient<Channel>,
    ) -> bool {
        if !self.config.music_backend.ignore_stt_playing {
            return false;
        }

        {
            let cache = self.playback_status_cache.lock().await;
            if cache.0.elapsed() < Duration::from_secs(1) {
                return cache.1;
            }
        }

        match client.get_status(tonic::Request::new(voicev1::Empty {})).await {
            Ok(rsp) => {
                let playing = rsp.into_inner().state() == voicev1::status_response::State::Playing;
                let mut cache = self.playback_status_cache.lock().await;
                *cache = (Instant::now(), playing);
                playing
            }
            Err(e) => {
                debug!("ignore_stt_playing: get_status failed: {e}");
                false
            }
        }
    }

    pub async fn run(self) -> Result<()> {
        let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
        let channel = Channel::from_shared(endpoint.clone())?.connect().await?;
        let mut client = VoiceServiceClient::new(channel);

        let req = tonic::Request::new(voicev1::SubscribeRequest {
            include_chat: true,
            include_playback: false,
            include_log: true,
            include_audio: self.config.headless.stt.enabled || self.config.llm.omni_model,
        });

        let mut stream = client.subscribe_events(req).await?.into_inner();
        debug!("Headless LLM bridge subscribed: {}", endpoint);

        while let Some(item) = stream.next().await {
            match item {
                Ok(ev) => {
                    let Some(payload) = ev.payload else {
                        continue;
                    };
                    match payload {
                        voicev1::event::Payload::Chat(chat) => {
                            if let Err(e) = self.handle_chat_event(&mut client, chat).await {
                                error!("headless bridge chat handling failed: {e}");
                            }
                        }
                        voicev1::event::Payload::Audio(audio) => {
                            if let Err(e) = self.handle_audio_event(&mut client, audio).await {
                                error!("headless bridge audio handling failed: {e}");
                            }
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    warn!("headless event stream error: {e}");
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
                    caller_uid: chat.invoker_unique_id.clone(),
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
            caller_uid: chat.invoker_unique_id.clone(),
            groups: vec![],
            channel_group_id: 0,
            reply_target_mode: chat.reply_target_mode,
            reply_target_client_id: chat.reply_target_client_id,
        }
    }

    fn resolve_caller_from_audio(&self, audio: &voicev1::AudioFrameEvent) -> CallerContext {
        let reply_target_mode = self.default_reply_target_mode();
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
                caller_uid: c.cldbid.to_string(),
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

    fn default_reply_target_mode(&self) -> i32 {
        match self.config.bot.default_reply_mode.as_str() {
            "channel" => 2,
            "server" => 3,
            _ => 1,
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
                Ok(v) => Ok(v),
                Err(_) => skill.execute(args, &exec_ctx).await,
            };

            match result {
                Ok(v) => v.to_string(),
                Err(e) => format!("技能执行失败: {}", e),
            }
        } else {
            "未找到指定的技能".to_string()
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
        self.handle_user_input(client, ctx, clean_text, "text")
            .await
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
        if !musicbot_name.is_empty() && musicbot_name.eq_ignore_ascii_case(&audio.from_client_name)
        {
            return Ok(());
        }

        if self.should_ignore_stt_while_playing(client).await {
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
            warn!("speech provider unavailable, skip stt");
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

        debug!(
            event = "headless.stt.result",
            speaker = %chunk.speaker_name,
            clid = chunk.speaker_client_id,
            text = %self.truncate_for_log(&text),
            "headless stt text"
        );

        let mut ctx = self.resolve_caller_from_audio(&audio);
        ctx.caller_name = chunk.speaker_name;
        self.handle_user_input(client, ctx, text, "voice").await
    }

    async fn handle_omni_audio_event(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        audio: voicev1::AudioFrameEvent,
    ) -> Result<()> {
        let chunk = {
            let mut guard = self.audio_pipeline.lock().await;
            let Some(pipeline) = guard.as_mut() else {
                warn!(
                    event = "headless.omni.no_pipeline",
                    "omni model: audio pipeline not available, dropping audio frame"
                );
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

        info!(
            event = "headless.omni.audio_input",
            speaker = %ctx.caller_name,
            clid = ctx.caller_id,
            audio_size = wav_bytes.len(),
            "omni model: sending audio directly to LLM"
        );

        self.handle_omni_user_input(client, ctx, audio_data, "voice")
            .await
    }

    async fn handle_user_input(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: CallerContext,
        user_msg: String,
        source: &str,
    ) -> Result<()> {
        if Self::OBS_CHAT {
            info!(
                event = "headless.chat.user_message",
                source = source,
                invoker = %ctx.caller_name,
                uid = %ctx.caller_uid,
                clid = ctx.caller_id,
                message = %self.truncate_for_log(&user_msg),
                "headless inbound message"
            );
        }

        let (mut messages, tools, session_source) = self.build_llm_request(&ctx, user_msg.clone());
        let pending_play = Arc::new(Mutex::new(None));
        let executor = HeadlessExecutor {
            bridge: self,
            ctx: &ctx,
            pending_play: pending_play.clone(),
        };

        let callbacks = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks().await?)
        } else {
            None
        };

        let result = match self
            .llm
            .run_tool_loop(
                &mut messages,
                &tools,
                &executor,
                self.config.llm.max_tool_turns,
                callbacks.as_ref(),
            )
            .await
        {
            Ok(r) => r,
            Err(ToolLoopError::MaxTurnsExceeded) => {
                // 清理 TTS 管道
                if let Some(ref cb) = callbacks {
                    if let Some(ref on_end) = cb.on_turn_end {
                        on_end("stop");
                    }
                }
                self.send_reply(
                    client,
                    &ctx,
                    "达到最大工具调用次数，请在设置中调整 max_tool_turns",
                )
                .await?;
                return Err(ToolLoopError::MaxTurnsExceeded.into());
            }
            Err(e) => {
                // 清理 TTS 管道
                if let Some(ref cb) = callbacks {
                    if let Some(ref on_end) = cb.on_turn_end {
                        on_end("stop");
                    }
                }
                self.send_reply(client, &ctx, "AI 后端当前不可用。请稍后再试。")
                    .await?;
                return Err(e.into());
            }
        };

        if !result.content.is_empty() {
            self.send_reply(client, &ctx, &result.content).await?;
            self.llm
                .save_turn(&session_source, user_msg, result.content);
        }
        Self::execute_pending_play(client, pending_play.lock().await.take()).await;
        Ok(())
    }

    async fn handle_omni_user_input(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: CallerContext,
        audio_data: String,
        _source: &str,
    ) -> Result<()> {
        if Self::OBS_CHAT {
            info!(
                event = "headless.omni.user_input",
                invoker = %ctx.caller_name,
                uid = %ctx.caller_uid,
                clid = ctx.caller_id,
                "omni model: processing audio input"
            );
        }

        let (mut messages, tools) = self.build_omni_llm_request(&ctx, audio_data);
        let pending_play = Arc::new(Mutex::new(None));
        let executor = HeadlessExecutor {
            bridge: self,
            ctx: &ctx,
            pending_play: pending_play.clone(),
        };

        let callbacks = if self.is_tts_effectively_enabled() {
            Some(self.build_tts_callbacks().await?)
        } else {
            None
        };

        let result = match self
            .llm
            .run_tool_loop(
                &mut messages,
                &tools,
                &executor,
                self.config.llm.max_tool_turns,
                callbacks.as_ref(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(e.into());
            }
        };

        if !result.content.is_empty() {
            self.send_reply(client, &ctx, &result.content).await?;
        }
        Self::execute_pending_play(client, pending_play.lock().await.take()).await;
        Ok(())
    }

    async fn build_tts_callbacks(&self) -> Result<StreamCallbacks> {
        let speech_provider = self
            .speech_provider
            .clone()
            .ok_or_else(|| anyhow::anyhow!("TTS enabled but speech provider not initialized"))?;
        let endpoint = format!("http://{}", INTERNAL_GRPC_ADDR);
        let channel = Channel::from_shared(endpoint)?.connect().await?;

        let (sentence_tx, sentence_rx) = mpsc::channel::<String>(128);
        let (audio_tx, audio_rx) = mpsc::channel::<voicev1::TtsAudioChunk>(8);
        let trace_id = format!(
            "tts-stream-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );

        // TTS synthesis task: consume sentences, synthesize, produce audio chunks
        let synth_audio_tx = audio_tx.clone();
        let synth_trace = trace_id.clone();
        tokio::spawn(async move {
            let mut rx = sentence_rx;
            while let Some(sentence) = rx.recv().await {
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
                    Err(e) => {
                        warn!(event = "headless.tts.synthesis_error", error = %e, "tts synthesis failed");
                    }
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

        // gRPC streaming task
        tokio::spawn(async move {
            let mut tts_client = VoiceServiceClient::new(channel);
            let rsp = tts_client
                .stream_tts_audio(tonic::Request::new(ReceiverStream::new(audio_rx)))
                .await;
            match rsp {
                Ok(resp) => {
                    let body = resp.into_inner();
                    if !body.ok {
                        warn!("stream_tts_audio rejected: {}", body.message);
                    }
                }
                Err(e) => {
                    warn!("stream_tts_audio failed: {e}");
                }
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
            let Some(ref tx) = *tx_guard else {
                return;
            };
            for segment in chunker_guard.push_token(token) {
                let _ = tx.try_send(segment);
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

    async fn execute_pending_play(
        client: &mut VoiceServiceClient<Channel>,
        pending: Option<PendingPlay>,
    ) {
        if let Some(pp) = pending {
            let play_req = voicev1::PlayRequest {
                source_url: pp.source_url,
                title: pp.title,
                requested_by: pp.requested_by,
                notice: String::new(),
            };
            if let Err(e) = client.play(tonic::Request::new(play_req)).await {
                warn!("deferred play failed: {e}");
            }
        }
    }

    fn build_llm_base_context(
        &self,
        ctx: &CallerContext,
    ) -> (String, String, Vec<serde_json::Value>) {
        let system_prompt = self.prompts.system.content.clone();

        let online_clients: Vec<_> = self
            .ts_clients
            .iter()
            .map(|item| {
                let c = item.value();
                json!({
                    "clid": c.clid,
                    "nickname": c.nickname,
                    "uid": c.cldbid,
                    "groups": c.server_groups,
                })
            })
            .collect();

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
    ) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let (system_prompt, user_ctx, tools) = self.build_llm_base_context(ctx);

        let content = vec![json!({
            "type": "input_audio",
            "input_audio": {
                "data": audio_data
            }
        })];

        let messages = vec![
            json!({"role": "system", "content": format!("{system_prompt}\n\n{user_ctx}")}),
            json!({"role": "user", "content": content}),
        ];
        (messages, tools)
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
