use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use dashmap::DashMap;
use futures_util::StreamExt;
use serde_json::json;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

#[derive(Debug, thiserror::Error)]
#[error("headless tool loop reached max turns")]
struct HeadlessToolLoopTimeoutError;

use crate::adapter::headless::speech::{
    detect_audio_format, pcm16_mono_to_wav_bytes, preprocess_stt_text, preprocess_text_message,
    OpenAiSpeechProvider, OpusSttPipeline,
};
use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::INTERNAL_GRPC_ADDR;
use crate::adapter::TsAdapter;
use crate::config::{AppConfig, PromptsConfig};
use crate::llm::provider::ToolCall;
use crate::llm::{LlmEngine, LlmStreamEvent, SessionSource};
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

pub struct HeadlessLlmBridge {
    config: Arc<AppConfig>,
    prompts: Arc<PromptsConfig>,
    gate: Arc<PermissionGate>,
    llm: Arc<LlmEngine>,
    registry: Arc<SkillRegistry>,
    ts_adapter: Arc<TsAdapter>,
    ts_clients: Arc<DashMap<u32, ClientInfo>>,
    audio_pipeline: Mutex<Option<OpusSttPipeline>>,
    speech_provider: Option<OpenAiSpeechProvider>,
}

impl HeadlessLlmBridge {
    const OBS_CHAT: bool = true;
    const OBS_LLM: bool = true;
    const OBS_TOOL: bool = true;
    const OBS_TEXT_MAX_LEN: usize = 240;
    const TTS_SEGMENT_SOFT_LIMIT: usize = 120;
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
            OpenAiSpeechProvider::new(config.clone(), prompts.tts.style_prompt.clone()).ok();
        // Create audio pipeline if STT is enabled OR omni_model is true (needs audio framing)
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

    /// Check if TTS is effectively enabled (disabled when omni_model is true)
    fn is_tts_effectively_enabled(&self) -> bool {
        !self.config.llm.omni_model && self.config.headless.tts.enabled
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
        info!("Headless LLM bridge subscribed: {}", endpoint);

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
                error_prompts: &self.prompts.error,
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
                Err(e) => self
                    .prompts
                    .error
                    .skill_error
                    .replace("{detail}", &e.to_string()),
            }
        } else {
            self.prompts.error.skill_not_found.clone()
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

        // Omni model: skip STT, send audio directly to LLM
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

    /// Handle audio event for omni models (skip STT, send audio directly to LLM)
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

        // Convert audio to WAV and base64 encode
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
        // Stop TTS if effectively enabled (considers omni_model)
        if self.is_tts_effectively_enabled() {
            let _ = client.stop(tonic::Request::new(voicev1::Empty {})).await;
        }

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

        let stream_tts_enabled = self.is_tts_effectively_enabled() && self.config.llm.stream_output;

        let reply = match if stream_tts_enabled {
            self.run_llm_chain_streaming_tts(client, &ctx, user_msg)
                .await
        } else {
            self.run_llm_chain(client, &ctx, user_msg).await
        } {
            Ok(reply) => reply,
            Err(e) => {
                if e.downcast_ref::<HeadlessToolLoopTimeoutError>().is_some() {
                    let _ = self.send_reply(client, &ctx, "操作超时，请稍后再试").await;
                    return Ok(());
                }
                return Err(e);
            }
        };
        if !reply.trim().is_empty() {
            self.send_reply(client, &ctx, &reply).await?;
            if self.is_tts_effectively_enabled() && !stream_tts_enabled {
                if let Err(e) = self.speak_reply(client, &reply).await {
                    warn!("tts playback failed, fallback text only: {e}");
                }
            }
        }
        Ok(())
    }

    async fn run_llm_chain_streaming_tts(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: &CallerContext,
        user_msg: String,
    ) -> Result<String> {
        let (messages, tools, source) = self.build_llm_request(ctx, user_msg.clone());
        match self
            .collect_stream_reply_and_speak(client, messages, tools)
            .await
        {
            Ok(reply) => {
                if !reply.trim().is_empty() {
                    self.llm.save_turn(&source, user_msg, reply.clone());
                }
                return Ok(reply);
            }
            Err(e) => {
                warn!("streaming llm+tts fast path failed, fallback to normal chain: {e}");
            }
        }

        let reply = self.run_llm_chain(client, ctx, user_msg).await?;
        if !reply.trim().is_empty() && self.is_tts_effectively_enabled() {
            info!(
                event = "headless.tts.fallback",
                reply_len = reply.chars().count(),
                "fallback to non-streaming tts"
            );
            if let Err(e) = self.speak_reply(client, &reply).await {
                warn!(%e, "fallback tts playback failed, will send text only");
            }
        }
        Ok(reply)
    }

    async fn run_llm_chain(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: &CallerContext,
        user_msg: String,
    ) -> Result<String> {
        let (messages, tools, source) = self.build_llm_request(ctx, user_msg.clone());
        let (messages, content) = self
            .execute_llm_with_tools(client, ctx, messages, tools)
            .await?;

        if let Some(content) = content {
            self.llm.save_turn(&source, user_msg, content.clone());
            return Ok(content);
        }

        // Try stream reply if no content from chat
        let stream_reply = self.collect_stream_reply(messages.clone()).await?;
        if !stream_reply.trim().is_empty() {
            self.llm.save_turn(&source, user_msg, stream_reply.clone());
            return Ok(stream_reply);
        }
        Err(anyhow!("empty llm response"))
    }

    /// Shared LLM execution with tool loop
    /// Returns (messages, content) where content is Some if LLM returned content without tool calls
    async fn execute_llm_with_tools(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: &CallerContext,
        mut messages: Vec<serde_json::Value>,
        tools: Vec<serde_json::Value>,
    ) -> Result<(Vec<serde_json::Value>, Option<String>)> {
        let max_turns = self.config.llm.max_tool_turns;

        for turn in 0..max_turns {
            if Self::OBS_LLM {
                info!(
                    event = "headless.llm.request",
                    turn = turn + 1,
                    messages_count = messages.len(),
                    tools_count = tools.len(),
                    "headless llm request"
                );
            }

            let response = match self.llm.chat(messages.clone(), tools.clone()).await {
                Ok(r) => r,
                Err(e) => {
                    error!(
                        event = "headless.llm.error",
                        turn = turn + 1,
                        error = %e,
                        "headless llm request failed"
                    );
                    return Err(e);
                }
            };

            if Self::OBS_LLM {
                let reply = response
                    .content
                    .as_deref()
                    .map(|s| self.truncate_for_log(s))
                    .unwrap_or_default();
                info!(
                    event = "headless.llm.response",
                    turn = turn + 1,
                    tool_calls = response.tool_calls.len(),
                    reply = %reply,
                    "headless llm response"
                );
            }

            if response.tool_calls.is_empty() {
                let content = response.content.filter(|c| !c.trim().is_empty());
                return Ok((messages, content));
            }

            // Build assistant tool calls JSON
            let assistant_tool_calls: Vec<_> = response
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

            messages.push(json!({
                "role":"assistant",
                "content": response.content,
                "tool_calls": assistant_tool_calls
            }));

            for call in &response.tool_calls {
                if Self::OBS_TOOL {
                    info!(
                        event = "headless.tool.call",
                        turn = turn + 1,
                        tool_name = %call.name,
                        tool_call_id = %call.id,
                        args = %self.truncate_for_log(&call.arguments.to_string()),
                        "headless tool call"
                    );
                }

                let tool_result = self.execute_skill(call, ctx).await;

                // Intercept __play_url from embedded music backends
                let tool_result = if let Ok(mut parsed) =
                    serde_json::from_str::<serde_json::Value>(&tool_result)
                {
                    if let Some(url) = parsed.get("__play_url").and_then(|v| v.as_str()) {
                        let title = parsed
                            .get("__play_title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        info!(
                            event = "headless.play_url",
                            url = %url,
                            title = %title,
                            "embedded backend requested playback"
                        );
                        let play_req = voicev1::PlayRequest {
                            source_url: url.to_string(),
                            title: title.clone(),
                            requested_by: ctx.caller_name.clone(),
                            notice: String::new(),
                        };
                        if let Err(e) = client.play(tonic::Request::new(play_req)).await {
                            warn!("gRPC Play failed: {e}");
                        }
                        // Strip __play_url / __play_title from result
                        if let Some(obj) = parsed.as_object_mut() {
                            obj.remove("__play_url");
                            obj.remove("__play_title");
                        }
                    }
                    serde_json::to_string(&parsed).unwrap_or(tool_result)
                } else {
                    tool_result
                };

                if Self::OBS_TOOL {
                    info!(
                        event = "headless.tool.result",
                        turn = turn + 1,
                        tool_name = %call.name,
                        tool_call_id = %call.id,
                        result = %self.truncate_for_log(&tool_result),
                        "headless tool result"
                    );
                }

                messages.push(json!({
                    "role":"tool",
                    "tool_call_id": call.id,
                    "name": call.name,
                    "content": tool_result
                }));
            }
        }

        Err(HeadlessToolLoopTimeoutError.into())
    }

    /// Shared helper: build system prompt, user context, and tools
    fn build_llm_base_context(
        &self,
        ctx: &CallerContext,
    ) -> (String, String, Vec<serde_json::Value>) {
        let system_prompt = self.prompts.system.content.clone();

        // Build online clients list for LLM context
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

    /// Build LLM request for omni models with audio input
    fn build_omni_llm_request(
        &self,
        ctx: &CallerContext,
        audio_data: String,
        text_prompt: Option<String>,
    ) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let (system_prompt, user_ctx, tools) = self.build_llm_base_context(ctx);

        let mut content = vec![json!({
            "type": "input_audio",
            "input_audio": {
                "data": audio_data
            }
        })];

        if let Some(prompt) = text_prompt {
            content.push(json!({
                "type": "text",
                "text": prompt
            }));
        }

        let messages = vec![
            json!({"role": "system", "content": system_prompt}),
            json!({"role": "system", "content": user_ctx}),
            json!({"role": "user", "content": content}),
        ];
        (messages, tools)
    }

    /// Handle user input for omni models (skip TTS, use audio input)
    async fn handle_omni_user_input(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        ctx: CallerContext,
        audio_data: String,
        source: &str,
    ) -> Result<()> {
        if Self::OBS_CHAT {
            info!(
                event = "headless.omni.user_input",
                source = source,
                invoker = %ctx.caller_name,
                uid = %ctx.caller_uid,
                clid = ctx.caller_id,
                "omni model: processing audio input"
            );
        }

        let (messages, tools) = self.build_omni_llm_request(&ctx, audio_data, None);
        let (_, content) = self
            .execute_llm_with_tools(client, &ctx, messages, tools)
            .await?;

        if let Some(content) = content {
            self.send_reply(client, &ctx, &content).await?;
            return Ok(());
        }

        Err(anyhow!("empty omni llm response"))
    }

    async fn collect_stream_reply(&self, messages: Vec<serde_json::Value>) -> Result<String> {
        let mut stream = self.llm.chat_stream(messages, vec![]).await?;
        let mut reply = String::new();
        while let Some(event) = stream.next().await {
            match event? {
                LlmStreamEvent::Token(token) => {
                    reply.push_str(&token);
                }
                LlmStreamEvent::ToolCalls => {
                    return Err(anyhow!("collect_stream_reply got unexpected tool_calls"));
                }
                LlmStreamEvent::Done => {
                    break;
                }
            }
        }
        if reply.trim().is_empty() {
            return Err(anyhow!("llm stream returned empty reply"));
        }
        Ok(reply)
    }

    async fn collect_stream_reply_and_speak(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        messages: Vec<serde_json::Value>,
        tools: Vec<serde_json::Value>,
    ) -> Result<String> {
        let Some(speech_provider) = self.speech_provider.as_ref() else {
            return Err(anyhow!("speech provider unavailable"));
        };

        if Self::OBS_LLM {
            info!(
                event = "headless.llm.stream_start",
                messages_count = messages.len(),
                tools_count = tools.len(),
                "stream tts: llm request started"
            );
        }

        let mut stream = self.llm.chat_stream(messages, tools).await?;
        let trace_id = format!(
            "tts-stream-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        let (tx, rx) = mpsc::channel::<voicev1::TtsAudioChunk>(8);
        let first_token_time = Arc::new(AtomicI64::new(0));
        let first_token_time_clone = first_token_time.clone();
        let start_time = std::time::Instant::now();

        let send_fut = async {
            let mut reply = String::new();
            let mut chunker = StreamingSentenceChunker::new(
                Self::STREAM_TTS_MIN_CHARS,
                Self::STREAM_TTS_WEAK_PUNCT_MIN_CHARS,
                Self::STREAM_TTS_MAX_CHARS,
            );
            while let Some(event) = stream.next().await {
                match event? {
                    LlmStreamEvent::Token(token) => {
                        if token.is_empty() {
                            continue;
                        }
                        if first_token_time_clone.load(Ordering::Relaxed) == 0 {
                            let elapsed = start_time.elapsed().as_millis() as i64;
                            first_token_time_clone.store(elapsed, Ordering::Relaxed);
                            if Self::OBS_LLM {
                                info!(
                                    event = "headless.llm.first_token",
                                    latency_ms = elapsed,
                                    "first token received"
                                );
                            }
                        }
                        reply.push_str(&token);
                        for segment in chunker.push_token(&token) {
                            self.enqueue_tts_segment(&tx, speech_provider, &trace_id, &segment)
                                .await?;
                        }
                    }
                    LlmStreamEvent::ToolCalls => {
                        warn!("stream_tts: llm returned tool_calls, aborting stream");
                        let _ = tx
                            .send(voicev1::TtsAudioChunk {
                                payload: vec![],
                                codec: "mp3".to_string(),
                                end_of_stream: true,
                                trace_id: trace_id.clone(),
                            })
                            .await;
                        return Err(anyhow!("streaming aborted due to tool_calls"));
                    }
                    LlmStreamEvent::Done => break,
                }
            }
            for segment in chunker.finish() {
                self.enqueue_tts_segment(&tx, speech_provider, &trace_id, &segment)
                    .await?;
            }
            if reply.trim().is_empty() {
                return Err(anyhow!("llm stream returned empty reply"));
            }
            let total_time = start_time.elapsed().as_millis() as i64;
            if Self::OBS_LLM {
                info!(
                    event = "headless.llm.stream_end",
                    total_ms = total_time,
                    first_token_ms = first_token_time_clone.load(Ordering::Relaxed),
                    reply_len = reply.chars().count(),
                    reply = %self.truncate_for_log(&reply),
                    "stream tts: llm reply completed"
                );
            }
            tx.send(voicev1::TtsAudioChunk {
                payload: vec![],
                codec: "mp3".to_string(),
                end_of_stream: true,
                trace_id,
            })
            .await
            .map_err(|e| anyhow!("send tts eos failed: {e}"))?;
            Ok::<String, anyhow::Error>(reply)
        };
        let stream_fut = async {
            let rsp = client
                .stream_tts_audio(tonic::Request::new(ReceiverStream::new(rx)))
                .await?;
            let body = rsp.into_inner();
            if !body.ok {
                return Err(anyhow!("stream_tts_audio rejected: {}", body.message));
            }
            Ok::<(), anyhow::Error>(())
        };
        let (reply, _) = tokio::try_join!(send_fut, stream_fut)?;
        Ok(reply)
    }

    async fn enqueue_tts_segment(
        &self,
        tx: &mpsc::Sender<voicev1::TtsAudioChunk>,
        speech_provider: &OpenAiSpeechProvider,
        trace_id: &str,
        segment: &str,
    ) -> Result<()> {
        let audio = speech_provider.synthesize(segment).await?;
        debug!(segment, audio_bytes = audio.len(), "enqueue tts segment");
        let codec = detect_audio_format(&audio);
        tx.send(voicev1::TtsAudioChunk {
            payload: audio,
            codec: codec.to_string(),
            end_of_stream: false,
            trace_id: trace_id.to_string(),
        })
        .await
        .map_err(|e| anyhow!("send tts chunk failed: {e}"))?;
        Ok(())
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

    async fn speak_reply(
        &self,
        client: &mut VoiceServiceClient<Channel>,
        text: &str,
    ) -> Result<()> {
        if !self.config.headless.tts.enabled {
            return Ok(());
        }
        let Some(speech_provider) = self.speech_provider.as_ref() else {
            error!("tts unavailable: speech provider not initialized");
            return Err(anyhow!("tts unavailable: speech provider not initialized"));
        };
        let trace_id = format!(
            "tts-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        let segments = self.split_tts_segments(text);
        let (tx, rx) = mpsc::channel::<voicev1::TtsAudioChunk>(8);
        let send_fut = async {
            for segment in segments {
                let audio = speech_provider.synthesize(&segment).await?;
                let codec = detect_audio_format(&audio);
                tx.send(voicev1::TtsAudioChunk {
                    payload: audio,
                    codec: codec.to_string(),
                    end_of_stream: false,
                    trace_id: trace_id.clone(),
                })
                .await
                .map_err(|e| anyhow!("send tts chunk failed: {e}"))?;
            }
            tx.send(voicev1::TtsAudioChunk {
                payload: vec![],
                codec: "mp3".to_string(),
                end_of_stream: true,
                trace_id,
            })
            .await
            .map_err(|e| anyhow!("send tts eos failed: {e}"))?;
            Ok::<(), anyhow::Error>(())
        };
        let stream_fut = async {
            let rsp = client
                .stream_tts_audio(tonic::Request::new(ReceiverStream::new(rx)))
                .await?;
            let body = rsp.into_inner();
            if !body.ok {
                return Err(anyhow!("stream_tts_audio rejected: {}", body.message));
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::try_join!(send_fut, stream_fut)?;
        Ok(())
    }

    fn split_tts_segments(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = String::new();
        let mut buf_char_count = 0usize;
        for ch in text.chars() {
            buf.push(ch);
            buf_char_count += 1;
            let punct_boundary = matches!(ch, '。' | '！' | '？' | '.' | '!' | '?' | ';' | '；');
            let len_boundary = buf_char_count >= Self::TTS_SEGMENT_SOFT_LIMIT;
            if punct_boundary || len_boundary {
                let s = buf.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
                buf.clear();
                buf_char_count = 0;
            }
        }
        let tail = buf.trim();
        if !tail.is_empty() {
            out.push(tail.to_string());
        }
        if out.is_empty() {
            out.push(text.trim().to_string());
        }
        out
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
