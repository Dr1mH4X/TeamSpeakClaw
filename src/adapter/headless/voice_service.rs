use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tonic::{Request, Response, Status};
use tracing::error;

use super::audio_output::AudioOutput;
use super::speech::detect_audio_format;
use super::tsbot::voice::v1 as voicev1;
use voicev1::voice_service_server::VoiceService;

pub struct VoiceServiceImpl {
    audio_output: AudioOutput,
    ts_client: Arc<tsclient_rs::Client>,
    control_tx: broadcast::Sender<voicev1::Event>,
    audio_tx: broadcast::Sender<voicev1::Event>,
    bot_default_reply_mode: String,
    tts_stream_lock: tokio::sync::Mutex<()>,
}

impl VoiceServiceImpl {
    pub fn new(
        audio_output: AudioOutput,
        ts_client: Arc<tsclient_rs::Client>,
        control_tx: broadcast::Sender<voicev1::Event>,
        audio_tx: broadcast::Sender<voicev1::Event>,
        bot_default_reply_mode: String,
    ) -> Self {
        Self {
            audio_output,
            ts_client,
            control_tx,
            audio_tx,
            bot_default_reply_mode,
            tts_stream_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn default_reply_mode(&self) -> i32 {
        crate::config::reply_target_mode(&self.bot_default_reply_mode)
    }
}

fn map_subscribed_event(
    result: std::result::Result<voicev1::Event, BroadcastStreamRecvError>,
    include_chat: bool,
) -> Option<std::result::Result<voicev1::Event, Status>> {
    match result {
        Ok(event) => {
            let included = match event.payload.as_ref() {
                Some(voicev1::event::Payload::Chat(_)) => include_chat,
                // control 通道不承载音频事件；音频走独立广播
                Some(voicev1::event::Payload::Audio(_)) => false,
                None => false,
            };
            included.then_some(Ok(event))
        }
        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
            // 控制事件丢失属于结构性故障，通知订阅方重建
            Some(Err(Status::resource_exhausted(format!(
                "voice control stream lagged by {skipped} messages"
            ))))
        }
    }
}

/// 音频事件丢失只记录 skipped 数量，不中断流——音频洪峰不能拖垮聊天
fn map_audio_event(
    result: std::result::Result<voicev1::Event, BroadcastStreamRecvError>,
) -> Option<voicev1::Event> {
    match result {
        Ok(event) => Some(event),
        Err(BroadcastStreamRecvError::Lagged(skipped)) => {
            tracing::warn!(skipped, "voice audio stream lagged; dropped audio frames");
            None
        }
    }
}

fn normalize_tts_codec(chunk: &voicev1::TtsAudioChunk) -> String {
    if chunk.codec.eq_ignore_ascii_case("wav") {
        "wav".to_string()
    } else if chunk.codec.eq_ignore_ascii_case("mp3") || chunk.codec.is_empty() {
        detect_audio_format(&chunk.payload).to_string()
    } else {
        chunk.codec.to_ascii_lowercase()
    }
}

#[tonic::async_trait]
impl VoiceService for VoiceServiceImpl {
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

        let mode: u8 = match r.target_mode {
            1..=3 => r.target_mode as u8,
            _ => self.default_reply_mode() as u8,
        };
        let mut target = r.target_client_id;

        if mode == 1 {
            if target == 0 {
                return Ok(Response::new(voicev1::CommandResponse {
                    ok: false,
                    message: "target_client_id is required for private message".to_string(),
                }));
            }
        } else {
            target = 0;
        }

        if let Err(e) =
            super::text_util::send_text_message(&self.ts_client, mode, target, &r.message).await
        {
            return Ok(Response::new(voicev1::CommandResponse {
                ok: false,
                message: e.to_string(),
            }));
        }

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }

    async fn stream_tts_audio(
        &self,
        req: Request<tonic::Streaming<voicev1::TtsAudioChunk>>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        // 外部 RPC：单会话；断连/错误 drop session → 消费者 abort 该 job
        let _guard = self.tts_stream_lock.lock().await;
        let mut stream = req.into_inner();
        let mut session = self
            .audio_output
            .open_tts_session()
            .await
            .map_err(|e| Status::internal(format!("open tts session failed: {e}")))?;

        loop {
            let chunk = match stream.message().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => {
                    error!(%e, "recv tts chunk failed");
                    drop(session);
                    return Err(Status::internal(format!("recv tts chunk failed: {e}")));
                }
            };
            if chunk.end_of_stream {
                break;
            }
            if chunk.payload.is_empty() {
                continue;
            }
            let codec = normalize_tts_codec(&chunk);
            if let Err(e) = session.push_encoded(chunk.payload, &codec).await {
                error!(%e, "push tts segment failed");
                drop(session);
                return Err(Status::internal(format!("push tts segment failed: {e}")));
            }
        }

        // 接收语义 finish：段入队完成即返回，不等待播完
        match session.finish().await {
            Ok(()) => Ok(Response::new(voicev1::CommandResponse {
                ok: true,
                message: "ok".to_string(),
            })),
            Err(e) => {
                error!(%e, "stream tts audio finish failed");
                Err(Status::internal(format!("stream_tts_audio failed: {e}")))
            }
        }
    }

    async fn subscribe_events(
        &self,
        req: Request<voicev1::SubscribeRequest>,
    ) -> std::result::Result<
        Response<<VoiceServiceImpl as VoiceService>::SubscribeEventsStream>,
        Status,
    > {
        let cfg = req.into_inner();
        let include_audio = cfg.include_audio;

        let control_stream =
            BroadcastStream::new(self.control_tx.subscribe()).filter_map(move |r| {
                let include_chat = cfg.include_chat;
                async move { map_subscribed_event(r, include_chat) }
            });

        if !include_audio {
            return Ok(Response::new(
                Box::pin(control_stream) as Self::SubscribeEventsStream
            ));
        }

        let audio_stream = BroadcastStream::new(self.audio_tx.subscribe())
            .filter_map(|r| async move { map_audio_event(r).map(Ok) });
        let merged = futures_util::stream::select(control_stream, audio_stream);
        Ok(Response::new(
            Box::pin(merged) as Self::SubscribeEventsStream
        ))
    }

    type SubscribeEventsStream = std::pin::Pin<
        Box<dyn tokio_stream::Stream<Item = std::result::Result<voicev1::Event, Status>> + Send>,
    >;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_lag_becomes_resource_exhausted_status() {
        let result = map_subscribed_event(Err(BroadcastStreamRecvError::Lagged(7)), true);

        let Some(Err(status)) = result else {
            panic!("control lag must produce a stream error");
        };
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        assert!(status.message().contains('7'));
    }

    #[test]
    fn audio_lag_is_skipped_without_error() {
        let result = map_audio_event(Err(BroadcastStreamRecvError::Lagged(3)));

        assert!(result.is_none());
    }

    #[test]
    fn audio_event_passes_through_audio_mapper() {
        let event = voicev1::Event {
            payload: Some(voicev1::event::Payload::Audio(
                voicev1::AudioFrameEvent::default(),
            )),
        };
        let result = map_audio_event(Ok(event.clone()));

        assert_eq!(result, Some(event));
    }

    #[test]
    fn normalize_tts_codec_detects_from_payload() {
        let wav = voicev1::TtsAudioChunk {
            payload: b"RIFF0000WAVE".to_vec(),
            codec: String::new(),
            end_of_stream: false,
            trace_id: "t".into(),
        };
        assert_eq!(normalize_tts_codec(&wav), "wav");

        let mp3 = voicev1::TtsAudioChunk {
            payload: vec![0xff, 0xfb, 0x90, 0x00],
            codec: String::new(),
            end_of_stream: false,
            trace_id: "t".into(),
        };
        assert_eq!(normalize_tts_codec(&mp3), "mp3");
    }
}
