use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::{broadcast, mpsc, watch, Mutex};
use tokio_stream::wrappers::BroadcastStream;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use tsproto_packets::packets::{Direction, Flags, OutCommand, OutPacket, PacketType};

use super::playback::playback_loop;
use super::serverquery::{
    serverquery_set_client_description, ts3_escape_value, ServerQueryRuntimeConfig,
};
use super::tsbot::voice::v1 as voicev1;
use super::types::{emit_log, emit_playback, PersistedVoiceState, SharedStatus};
use voicev1::voice_service_server::VoiceService;

pub struct PlaybackControl {
    pub cancel: tokio_util::sync::CancellationToken,
    pub paused_tx: watch::Sender<bool>,
    pub handle: tokio::task::JoinHandle<()>,
}

pub struct VoiceServiceImpl {
    status: Arc<Mutex<SharedStatus>>,
    playback: Arc<Mutex<Option<PlaybackControl>>>,
    ts3_audio_tx: mpsc::Sender<OutPacket>,
    ts3_notice_tx: mpsc::Sender<(i32, u32, String)>,
    ts3_cmd_tx: mpsc::Sender<OutCommand>,
    events_tx: broadcast::Sender<voicev1::Event>,
    persist_tx: mpsc::Sender<PersistedVoiceState>,
    sq_config: Option<ServerQueryRuntimeConfig>,
    nickname: String,
    bot_respond_to_private: bool,
    bot_default_reply_mode: String,
    bot_trigger_prefixes: Vec<String>,
}

impl VoiceServiceImpl {
    pub fn new(
        status: Arc<Mutex<SharedStatus>>,
        ts3_audio_tx: mpsc::Sender<OutPacket>,
        ts3_notice_tx: mpsc::Sender<(i32, u32, String)>,
        ts3_cmd_tx: mpsc::Sender<OutCommand>,
        events_tx: broadcast::Sender<voicev1::Event>,
        persist_tx: mpsc::Sender<PersistedVoiceState>,
        sq_config: Option<ServerQueryRuntimeConfig>,
        nickname: String,
        bot_respond_to_private: bool,
        bot_default_reply_mode: String,
        bot_trigger_prefixes: Vec<String>,
    ) -> Self {
        Self {
            status,
            playback: Arc::new(Mutex::new(None)),
            ts3_audio_tx,
            ts3_notice_tx,
            ts3_cmd_tx,
            events_tx,
            persist_tx,
            sq_config,
            nickname,
            bot_respond_to_private,
            bot_default_reply_mode,
            bot_trigger_prefixes,
        }
    }

    fn default_reply_mode(&self) -> i32 {
        match self.bot_default_reply_mode.as_str() {
            "channel" => 2,
            "server" => 3,
            _ => 1,
        }
    }

    async fn stop_internal(&self) {
        let mut pb = self.playback.lock().await;
        if let Some(p) = pb.take() {
            p.cancel.cancel();
            let handle = p.handle;
            handle.abort();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), handle).await;
        }
    }
}

#[tonic::async_trait]
impl VoiceService for VoiceServiceImpl {
    async fn ping(
        &self,
        _req: Request<voicev1::Empty>,
    ) -> std::result::Result<Response<voicev1::PingResponse>, Status> {
        Ok(Response::new(voicev1::PingResponse {
            version: "0.1.0".to_string(),
        }))
    }

    async fn play(
        &self,
        req: Request<voicev1::PlayRequest>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        let r = req.into_inner();

        if !r.notice.is_empty() {
            let mut mode = self.default_reply_mode();
            if mode == 1 {
                // PlayRequest 未提供私聊目标，避免发送无效 private 消息。
                mode = 2;
            }
            let target = 0;
            let _ = self
                .ts3_notice_tx
                .try_send((mode, target, r.notice.clone()));
        }

        {
            let mut st = self.status.lock().await;
            st.now_playing_title = r.title.clone();
            st.now_playing_source_url = r.source_url.clone();
            st.state = 2;
        }

        emit_playback(
            &self.events_tx,
            1,
            r.title.clone(),
            r.source_url.clone(),
            "",
        );

        self.stop_internal().await;

        let (paused_tx, paused_rx) = watch::channel(false);
        let cancel = tokio_util::sync::CancellationToken::new();

        let status = self.status.clone();
        let tx = self.ts3_audio_tx.clone();
        let events_tx = self.events_tx.clone();
        let title = r.title.clone();
        let source_url = r.source_url;
        let cancel_child = cancel.clone();

        let handle = tokio::spawn(async move {
            let r = playback_loop(source_url.clone(), tx, paused_rx, cancel_child, status).await;
            match r {
                Ok(()) => {
                    emit_playback(&events_tx, 2, title, source_url, "");
                }
                Err(e) => {
                    use tracing::error;
                    error!(%e, "playback loop failed");
                    emit_playback(&events_tx, 3, title, source_url, format!("{e}"));
                }
            }
        });

        let mut pb = self.playback.lock().await;
        *pb = Some(PlaybackControl {
            cancel,
            paused_tx,
            handle,
        });

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "accepted".to_string(),
        }))
    }

    async fn pause(
        &self,
        _req: Request<voicev1::Empty>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        {
            let mut st = self.status.lock().await;
            if st.state == 2 {
                st.state = 3;
            }
        }

        if let Some(pb) = self.playback.lock().await.as_ref() {
            let _ = pb.paused_tx.send(true);
        }

        emit_log(&self.events_tx, 2, "paused");

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }

    async fn set_client_description(
        &self,
        req: Request<voicev1::SetClientDescriptionRequest>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        let r = req.into_inner();
        let desc = r.description;
        let msg = format!("set client_description requested (len={})", desc.len());
        emit_log(&self.events_tx, 2, msg.clone());
        info!("{msg}");
        if desc.len() > 700 {
            return Ok(Response::new(voicev1::CommandResponse {
                ok: false,
                message: "description too long".to_string(),
            }));
        }

        let cleaned = desc.replace(['\r', '\n', '\t'], " ");
        let compact = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");

        let encoded = ts3_escape_value(&compact);

        if encoded.len() != desc.len() {
            info!(
                "client_description encoded: orig_len={} encoded_len={}",
                desc.len(),
                encoded.len()
            );
        }

        if let Some(ref sq_cfg) = self.sq_config {
            match serverquery_set_client_description(sq_cfg, &self.nickname, &encoded).await {
                Ok(()) => {
                    return Ok(Response::new(voicev1::CommandResponse {
                        ok: true,
                        message: "ok".to_string(),
                    }));
                }
                Err(e) => {
                    let msg = format!("serverquery set description failed: {e}");
                    emit_log(&self.events_tx, 3, msg.clone());
                    warn!("{msg}");
                    return Ok(Response::new(voicev1::CommandResponse {
                        ok: false,
                        message: msg,
                    }));
                }
            }
        }

        let mut cmd = OutCommand::new(
            Direction::C2S,
            Flags::empty(),
            PacketType::Command,
            "clientupdate",
        );
        cmd.write_arg("client_description", &encoded);

        self.ts3_cmd_tx
            .send(cmd)
            .await
            .map_err(|e| Status::internal(format!("send failed: {e}")))?;

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }

    async fn resume(
        &self,
        _req: Request<voicev1::Empty>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        {
            let mut st = self.status.lock().await;
            if st.state == 3 {
                st.state = 2;
            }
        }

        if let Some(pb) = self.playback.lock().await.as_ref() {
            let _ = pb.paused_tx.send(false);
        }

        emit_log(&self.events_tx, 2, "resumed");

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }

    async fn stop(
        &self,
        _req: Request<voicev1::Empty>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        self.stop_internal().await;

        {
            let mut st = self.status.lock().await;
            st.state = 1;
            st.now_playing_title.clear();
            st.now_playing_source_url.clear();
        }

        emit_log(&self.events_tx, 2, "stopped");

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }

    async fn skip(
        &self,
        _req: Request<voicev1::Empty>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        self.stop(_req).await
    }

    async fn send_notice(
        &self,
        req: Request<voicev1::NoticeRequest>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        let r = req.into_inner();
        if r.message.is_empty() {
            return Ok(Response::new(voicev1::CommandResponse {
                ok: false,
                message: "empty message".to_string(),
            }));
        }

        let mode = match r.target_mode {
            1 | 2 | 3 => r.target_mode,
            _ => self.default_reply_mode(),
        };
        let mut target = r.target_client_id;

        if mode == 1 {
            if !self.bot_respond_to_private {
                return Ok(Response::new(voicev1::CommandResponse {
                    ok: false,
                    message: "private reply disabled by bot.respond_to_private".to_string(),
                }));
            }
            if target == 0 {
                return Ok(Response::new(voicev1::CommandResponse {
                    ok: false,
                    message: "target_client_id is required for private message".to_string(),
                }));
            }
        } else {
            target = 0;
        }

        if self
            .ts3_notice_tx
            .try_send((mode, target, r.message))
            .is_err()
        {
            return Ok(Response::new(voicev1::CommandResponse {
                ok: false,
                message: "notice queue is full".to_string(),
            }));
        }

        emit_log(
            &self.events_tx,
            2,
            format!(
                "send_notice accepted: target_mode={} target_client_id={} trigger_prefixes={}",
                mode,
                target,
                self.bot_trigger_prefixes.len()
            ),
        );

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }

    async fn set_volume(
        &self,
        req: Request<voicev1::SetVolumeRequest>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        let v = req.into_inner().volume_percent.clamp(0, 200);
        let snapshot = {
            let mut st = self.status.lock().await;
            st.volume_percent = v;
            PersistedVoiceState::from_status(&st)
        };
        let _ = self.persist_tx.try_send(snapshot);

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }

    async fn get_status(
        &self,
        _req: Request<voicev1::Empty>,
    ) -> std::result::Result<Response<voicev1::StatusResponse>, Status> {
        let st = self.status.lock().await;
        Ok(Response::new(voicev1::StatusResponse {
            state: st.state,
            now_playing_title: st.now_playing_title.clone(),
            now_playing_source_url: st.now_playing_source_url.clone(),
            volume_percent: st.volume_percent,
        }))
    }

    async fn set_audio_fx(
        &self,
        req: Request<voicev1::SetAudioFxRequest>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        let r = req.into_inner();
        let snapshot = {
            let mut st = self.status.lock().await;

            if let Some(p) = r.pan {
                st.fx_pan = p.clamp(-1.0, 1.0);
            }
            if let Some(w) = r.width {
                st.fx_width = w.clamp(0.0, 3.0);
            }
            if let Some(s) = r.swap_lr {
                st.fx_swap_lr = s;
            }

            if let Some(b) = r.bass_db {
                st.fx_bass_db = b.clamp(0.0, 18.0);
            }
            if let Some(m) = r.reverb_mix {
                st.fx_reverb_mix = m.clamp(0.0, 1.0);
            }

            PersistedVoiceState::from_status(&st)
        };
        let _ = self.persist_tx.try_send(snapshot);

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }

    async fn get_audio_fx(
        &self,
        _req: Request<voicev1::Empty>,
    ) -> std::result::Result<Response<voicev1::AudioFxResponse>, Status> {
        let st = self.status.lock().await;
        Ok(Response::new(voicev1::AudioFxResponse {
            pan: st.fx_pan,
            width: st.fx_width,
            swap_lr: st.fx_swap_lr,
            bass_db: st.fx_bass_db,
            reverb_mix: st.fx_reverb_mix,
        }))
    }

    async fn subscribe_events(
        &self,
        req: Request<voicev1::SubscribeRequest>,
    ) -> std::result::Result<Response<Self::SubscribeEventsStream>, Status> {
        let cfg = req.into_inner();
        let rx = self.events_tx.subscribe();
        let stream = BroadcastStream::new(rx).filter_map(move |r| {
            let include_chat = cfg.include_chat;
            let include_playback = cfg.include_playback;
            let include_log = cfg.include_log;
            let include_audio = cfg.include_audio;
            async move {
                match r {
                    Ok(ev) => {
                        let ok = match ev.payload {
                            Some(voicev1::event::Payload::Chat(_)) => include_chat,
                            Some(voicev1::event::Payload::Playback(_)) => include_playback,
                            Some(voicev1::event::Payload::Log(_)) => include_log,
                            Some(voicev1::event::Payload::Audio(_)) => include_audio,
                            None => false,
                        };
                        if ok {
                            Some(Ok(ev))
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                }
            }
        });
        Ok(Response::new(
            Box::pin(stream) as Self::SubscribeEventsStream
        ))
    }

    type SubscribeEventsStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = std::result::Result<voicev1::Event, Status>> + Send>,
    >;
}
