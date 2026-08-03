use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::text_util::split_message;
use super::tsbot::voice::v1 as voicev1;
use super::types::now_unix_ms;

pub async fn ts3_actor(
    client: Arc<tsclient_rs::Client>,
    mut audio_rx: mpsc::Receiver<(Vec<u8>, i32)>,
    mut notice_rx: mpsc::Receiver<(i32, u32, String)>,
    events_tx: broadcast::Sender<voicev1::Event>,
    shutdown_token: CancellationToken,
    bot_respond_to_private: bool,
    bot_trigger_prefixes: Vec<String>,
    bot_default_reply_mode: String,
) -> Result<()> {
    let mut out_buf: VecDeque<(Vec<u8>, i32)> = VecDeque::with_capacity(400);

    let mut send_tick = tokio::time::interval(Duration::from_millis(20));

    // 先注册 text handler，避免丢消息
    let events_tx_t = events_tx.clone();
    let respond_private = bot_respond_to_private;
    let reply_mode = bot_default_reply_mode.clone();
    client.on_text_message(Arc::new(move |event: tsclient_rs::Event| {
        if let tsclient_rs::Event::TextMessage(ref msg) = event {
            let target_mode = match msg.target_mode {
                1..=3 => msg.target_mode,
                mode => {
                    warn!(target_mode = mode, "忽略未知类型的 TeamSpeak 文本消息");
                    return;
                }
            };
            let Ok(invoker_client_id) = u32::try_from(msg.invoker_id) else {
                warn!(
                    invoker_id = msg.invoker_id,
                    "忽略调用者 ID 无效的 TeamSpeak 文本消息"
                );
                return;
            };
            let raw_content = msg.message.trim().to_string();
            let (msg_content, should_trigger_llm) = if target_mode == 1 && respond_private {
                (raw_content, true)
            } else {
                match crate::router::strip_trigger_prefix(&raw_content, &bot_trigger_prefixes) {
                    Some(stripped) => (stripped.to_string(), true),
                    None => (raw_content, false),
                }
            };
            let (reply_target_mode, reply_target_client_id) = if target_mode == 1 {
                (1, invoker_client_id)
            } else {
                match reply_mode.as_str() {
                    "channel" => (2, 0),
                    "server" => (3, 0),
                    _ => (1, invoker_client_id),
                }
            };
            let _ = events_tx_t.send(voicev1::Event {
                unix_ms: now_unix_ms(),
                payload: Some(voicev1::event::Payload::Chat(voicev1::ChatEvent {
                    target_mode,
                    invoker_unique_id: msg.invoker_uid.clone(),
                    invoker_name: msg.invoker_name.clone(),
                    message: msg_content,
                    invoker_avatar_hash: String::new(),
                    invoker_description: String::new(),
                    should_trigger_llm,
                    should_respond: should_trigger_llm,
                    reply_target_mode,
                    reply_target_client_id,
                    invoker_client_id,
                })),
            });
        }
    }));

    // 启动时 listClients 一次，填充 name 映射供 voice handler 使用
    let client_names: Arc<HashMap<i32, String>> =
        Arc::new(match tsclient_rs::listClients(&client).await {
            Ok(clients) => clients.into_iter().map(|c| (c.id, c.nickname)).collect(),
            Err(e) => {
                warn!("初始化 TeamSpeak 客户端名称失败: {e}");
                HashMap::new()
            }
        });

    // voice data → AudioFrameEvent
    let events_tx_v = events_tx.clone();
    let voice_names = client_names;
    client.on_voice_data(Arc::new(move |event: tsclient_rs::Event| {
        if let tsclient_rs::Event::VoiceData(ref vd) = event {
            let Ok(from_client_id) = u32::try_from(vd.client_id) else {
                warn!(
                    client_id = vd.client_id,
                    "忽略调用者 ID 无效的 TeamSpeak 音频帧"
                );
                return;
            };
            let from_client_name = voice_names.get(&vd.client_id).cloned().unwrap_or_default();
            let _ = events_tx_v.send(voicev1::Event {
                unix_ms: now_unix_ms(),
                payload: Some(voicev1::event::Payload::Audio(voicev1::AudioFrameEvent {
                    from_client_id,
                    from_client_name,
                    codec: vd.codec,
                    is_whisper: false,
                    frame: vd.data.to_vec(),
                })),
            });
        }
    }));

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => {
                break;
            }

            pkt = audio_rx.recv() => {
                if let Some(p) = pkt {
                    if out_buf.len() >= 800 {
                        out_buf.pop_front();
                    }
                    out_buf.push_back(p);
                } else {
                    break;
                }
            }

            msg = notice_rx.recv() => {
                if let Some((mode, target, text)) = msg {
                    let target_mode = if mode == 1 || mode == 2 || mode == 3 { mode } else { 2 };
                    let target = if target_mode == 1 { target } else { 0 };
                    for chunk in split_message(&text, super::text_util::MAX_MESSAGE_BYTES) {
                        if let Err(e) = tsclient_rs::sendTextMessage(
                            &client,
                            target_mode,
                            target as u64,
                            &chunk,
                        ).await {
                            warn!("sendTextMessage failed: {e}");
                        }
                    }
                } else {
                    break;
                }
            }

            _ = send_tick.tick() => {
                if let Some((data, codec)) = out_buf.pop_front() {
                    client.send_voice(data, codec);
                }
            }
        }
    }

    Ok(())
}
