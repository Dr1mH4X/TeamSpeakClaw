use std::sync::Arc;

use crate::config::AppConfig;
use crate::adapter::headless::speech::OpenAiSpeechProvider;
use crate::adapter::headless::tsbot::voice::v1 as voicev1;
use crate::adapter::headless::tsbot::voice::v1::voice_service_client::VoiceServiceClient;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{error, warn};

/// 通过 headless gRPC 服务播放 TTS（共享实现）
///
/// 该函数封装了 TTS 流式播放和分段逻辑，避免在多个 router 中重复实现。
///
/// # 参数
/// - `channel`: headless gRPC 通道（可选）
/// - `config`: 应用配置
/// - `trace_id`: 跟踪标识符（例如 "sq-tts" 或 "nc-tts"）
/// - `text`: 要合成的文本
pub async fn speak_via_headless(
    channel: Option<Channel>,
    config: Arc<AppConfig>,
    trace_id: String,
    text: &str,
) {
    let Some(channel) = channel else {
        warn!("headless channel not available, cannot play TTS");
        return;
    };

    let speech_provider = match OpenAiSpeechProvider::new(config, String::new()) {
        Ok(sp) => sp,
        Err(e) => {
            error!("failed to create speech provider: {e}");
            return;
        }
    };

    let mut client = VoiceServiceClient::new(channel);

    // 先停止当前播放
    let _ = client.stop(tonic::Request::new(voicev1::Empty {})).await;

    let segments = split_tts_segments(text);
    let (tx, rx) = mpsc::channel::<voicev1::TtsAudioChunk>(8);

    let send_fut = async {
        for segment in segments {
            let audio = match speech_provider.synthesize(&segment).await {
                Ok(a) => a,
                Err(e) => {
                    error!("tts synthesize failed: {e}");
                    continue;
                }
            };
            let codec = crate::adapter::headless::speech::detect_audio_format(&audio);
            if let Err(e) = tx
                .send(voicev1::TtsAudioChunk {
                    payload: audio,
                    codec: codec.to_string(),
                    end_of_stream: false,
                    trace_id: trace_id.clone(),
                })
                .await
            {
                error!("send tts chunk failed: {e}");
                break;
            }
        }
        let _ = tx
            .send(voicev1::TtsAudioChunk {
                payload: vec![],
                codec: "mp3".to_string(),
                end_of_stream: true,
                trace_id: trace_id.clone(),
            })
            .await;
    };

    let stream_fut = async {
        let rsp = client
            .stream_tts_audio(tonic::Request::new(ReceiverStream::new(rx)))
            .await;
        match rsp {
            Ok(r) => {
                let body = r.into_inner();
                if !body.ok {
                    error!("stream_tts_audio rejected: {}", body.message);
                }
            }
            Err(e) => {
                error!("stream_tts_audio failed: {e}");
            }
        }
    };

    let _ = tokio::join!(send_fut, stream_fut);
}

/// 将文本分割为 TTS 合成片段
///
/// 根据标点和长度限制进行分段，避免在单个 TTS 请求中发送过长的文本。
pub fn split_tts_segments(text: &str) -> Vec<String> {
    const TTS_SEGMENT_SOFT_LIMIT: usize = 120;
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut buf_char_count = 0usize;
    for ch in text.chars() {
        buf.push(ch);
        buf_char_count += 1;
        let punct_boundary = matches!(ch, '。' | '！' | '？' | '.' | '!' | '?' | ';' | '；');
        let len_boundary = buf_char_count >= TTS_SEGMENT_SOFT_LIMIT;
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
