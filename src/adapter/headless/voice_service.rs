use std::{io::ErrorKind, process::Stdio};

use anyhow::{anyhow, Context};
use audiopus::coder::Encoder;
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tonic::{Request, Response, Status};
use tracing::{error, warn};

use super::speech::detect_audio_format;
use super::tsbot::voice::v1 as voicev1;
use super::types::emit_log;
use voicev1::voice_service_server::VoiceService;

pub struct VoiceServiceImpl {
    ts3_audio_tx: mpsc::Sender<(Vec<u8>, i32)>,
    ts3_notice_tx: mpsc::Sender<(i32, u32, String)>,
    control_tx: broadcast::Sender<voicev1::Event>,
    audio_tx: broadcast::Sender<voicev1::Event>,
    bot_default_reply_mode: String,
    tts_stream_lock: tokio::sync::Mutex<()>,
}

impl VoiceServiceImpl {
    pub fn new(
        ts3_audio_tx: mpsc::Sender<(Vec<u8>, i32)>,
        ts3_notice_tx: mpsc::Sender<(i32, u32, String)>,
        control_tx: broadcast::Sender<voicev1::Event>,
        audio_tx: broadcast::Sender<voicev1::Event>,
        bot_default_reply_mode: String,
    ) -> Self {
        Self {
            ts3_audio_tx,
            ts3_notice_tx,
            control_tx,
            audio_tx,
            bot_default_reply_mode,
            tts_stream_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn default_reply_mode(&self) -> i32 {
        match self.bot_default_reply_mode.as_str() {
            "channel" => 2,
            "server" => 3,
            _ => 1,
        }
    }
}

struct ChildKillOnDrop {
    child: Option<Child>,
}

impl ChildKillOnDrop {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }
}

impl Drop for ChildKillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

fn map_subscribed_event(
    result: std::result::Result<voicev1::Event, BroadcastStreamRecvError>,
    include_chat: bool,
    include_log: bool,
) -> Option<std::result::Result<voicev1::Event, Status>> {
    match result {
        Ok(event) => {
            let included = match event.payload.as_ref() {
                Some(voicev1::event::Payload::Chat(_)) => include_chat,
                Some(voicev1::event::Payload::Log(_)) => include_log,
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
            warn!(skipped, "voice audio stream lagged; dropped audio frames");
            None
        }
    }
}

async fn stream_tts_audio_loop(
    mut stream: tonic::Streaming<voicev1::TtsAudioChunk>,
    ts3_audio_tx: mpsc::Sender<(Vec<u8>, i32)>,
) -> anyhow::Result<()> {
    let encoder = Encoder::new(
        audiopus::SampleRate::Hz48000,
        audiopus::Channels::Stereo,
        audiopus::Application::Audio,
    )
    .map_err(|e| anyhow!("opus encoder init failed: {e}"))?;

    let frame_samples_per_channel = 48_000 / 50;
    let frame_bytes = frame_samples_per_channel * 2 * 2;

    loop {
        let chunk = match stream.message().await.context("recv tts chunk failed")? {
            Some(c) => c,
            None => break,
        };

        if chunk.end_of_stream {
            break;
        }

        if chunk.payload.is_empty() {
            continue;
        }

        let input_format = if chunk.codec.eq_ignore_ascii_case("wav") {
            "wav"
        } else if chunk.codec.eq_ignore_ascii_case("mp3") || chunk.codec.is_empty() {
            detect_audio_format(&chunk.payload)
        } else {
            warn!("unsupported tts codec: {}, skipping", chunk.codec);
            continue;
        };

        let child_result = tokio::process::Command::new("ffmpeg")
            .arg("-nostdin")
            .arg("-loglevel")
            .arg("error")
            .arg("-f")
            .arg(input_format)
            .arg("-i")
            .arg("pipe:0")
            .arg("-f")
            .arg("s16le")
            .arg("-ar")
            .arg("48000")
            .arg("-ac")
            .arg("2")
            .arg("pipe:1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let child = match child_result {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to start ffmpeg: {e}, skipping chunk");
                continue;
            }
        };

        let mut child = ChildKillOnDrop::new(child);

        if let Some(stderr) = child.child.as_mut().and_then(|c| c.stderr.take()) {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!("stream_tts ffmpeg: {line}");
                }
            });
        }

        let stdin_result = child.child.as_mut().and_then(|c| c.stdin.take());
        let mut stdin = match stdin_result {
            Some(s) => s,
            None => {
                warn!("ffmpeg stdin missing, skipping chunk");
                continue;
            }
        };

        let mut stdout = child
            .child
            .as_mut()
            .and_then(|c| c.stdout.take())
            .ok_or_else(|| anyhow!("ffmpeg stdout missing"))?;
        let payload = chunk.payload;
        let stdin_task = tokio::spawn(async move {
            let result = stdin.write_all(&payload).await;
            drop(stdin);
            result
        });

        let mut pcm = vec![0u8; frame_bytes];
        let mut float_buf = vec![0f32; frame_samples_per_channel * 2];
        let mut opus_out = [0u8; 1275];

        loop {
            match stdout.read_exact(&mut pcm).await {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
                Err(e) => {
                    warn!("read ffmpeg pcm failed: {e}, skipping chunk");
                    break;
                }
            }

            for i in 0..float_buf.len() {
                let lo = pcm[i * 2];
                let hi = pcm[i * 2 + 1];
                float_buf[i] = i16::from_le_bytes([lo, hi]) as f32 / 32768.0;
            }

            let len = match encoder.encode_float(&float_buf, &mut opus_out) {
                Ok(l) => l,
                Err(e) => {
                    warn!("opus encode failed: {e}, skipping chunk");
                    break;
                }
            };
            if let Err(e) = ts3_audio_tx.send((opus_out[..len].to_vec(), 5)).await {
                warn!("send ts3 audio failed: {e}");
                break;
            }
        }

        if let Some(mut c) = child.child.take() {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }

        match stdin_task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("write ffmpeg stdin failed: {e}"),
            Err(e) => warn!("ffmpeg stdin task failed: {e}"),
        }
    }

    Ok(())
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
            1..=3 => r.target_mode,
            _ => self.default_reply_mode(),
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
            &self.control_tx,
            2,
            format!(
                "send_notice accepted: target_mode={} target_client_id={}",
                mode, target
            ),
        );

        Ok(Response::new(voicev1::CommandResponse {
            ok: true,
            message: "ok".to_string(),
        }))
    }

    async fn stream_tts_audio(
        &self,
        req: Request<tonic::Streaming<voicev1::TtsAudioChunk>>,
    ) -> std::result::Result<Response<voicev1::CommandResponse>, Status> {
        let _guard = self.tts_stream_lock.lock().await;
        let result = stream_tts_audio_loop(req.into_inner(), self.ts3_audio_tx.clone()).await;

        match result {
            Ok(()) => Ok(Response::new(voicev1::CommandResponse {
                ok: true,
                message: "ok".to_string(),
            })),
            Err(e) => {
                error!(%e, "stream tts audio loop failed");
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
                let include_log = cfg.include_log;
                async move { map_subscribed_event(r, include_chat, include_log) }
            });

        if !include_audio {
            return Ok(Response::new(
                Box::pin(control_stream) as Self::SubscribeEventsStream
            ));
        }

        let audio_stream = BroadcastStream::new(self.audio_tx.subscribe())
            .filter_map(|r| async move { map_audio_event(r).map(Ok) });
        let merged = futures::stream::select(control_stream, audio_stream);
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
        let result = map_subscribed_event(Err(BroadcastStreamRecvError::Lagged(7)), true, true);

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
            unix_ms: 1,
            payload: Some(voicev1::event::Payload::Audio(
                voicev1::AudioFrameEvent::default(),
            )),
        };
        let result = map_audio_event(Ok(event.clone()));

        assert_eq!(result, Some(event));
    }
}
