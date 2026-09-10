use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::tsbot::voice::v1 as voicev1;

/// 客户端目录：clid -> nickname，随 listClients 周期刷新
type ClientDirectory = Arc<Mutex<HashMap<i32, String>>>;

/// 输出缓冲上限：满时暂停读取上游（背压），不再丢弃旧帧
const OUT_BUF_MAX: usize = 400;

/// actor 事件输出通道：控制事件（chat/log）与音频事件分离，音频洪峰不影响聊天
pub struct ActorEventChannels {
    pub control_tx: broadcast::Sender<voicev1::Event>,
    pub audio_tx: broadcast::Sender<voicev1::Event>,
}

async fn refresh_client_directory(directory: &ClientDirectory, client: &tsclient_rs::Client) {
    match tsclient_rs::listClients(client).await {
        Ok(clients) => {
            let mut dir = directory.lock().expect("client directory poisoned");
            dir.clear();
            for c in clients {
                dir.insert(c.id, c.nickname);
            }
        }
        Err(e) => warn!("刷新 TeamSpeak 客户端目录失败: {e}"),
    }
}

pub async fn ts3_actor(
    client: Arc<tsclient_rs::Client>,
    mut audio_rx: tokio::sync::mpsc::Receiver<(Vec<u8>, i32)>,
    channels: ActorEventChannels,
    shutdown_token: CancellationToken,
    bridge_state: super::VoiceBridgeState,
) -> Result<()> {
    let mut out_buf: VecDeque<(Vec<u8>, i32)> = VecDeque::with_capacity(400);

    let mut send_tick = tokio::time::interval(Duration::from_millis(20));

    // 先注册 text handler，避免丢消息；只搬运原始文本，触发策略由 router 层决定
    let control_tx_t = channels.control_tx.clone();
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
            let _ = control_tx_t.send(voicev1::Event {
                payload: Some(voicev1::event::Payload::Chat(voicev1::ChatEvent {
                    target_mode,
                    invoker_unique_id: msg.invoker_uid.clone(),
                    invoker_name: msg.invoker_name.clone(),
                    message: msg.message.clone(),
                    invoker_client_id,
                })),
            });
        }
    }));

    // text handler 注册完成后置位 actor 就绪，避免文本被过早路由到 bridge 而丢失
    bridge_state.set_actor_ready(true);

    // 建立客户端目录：clid -> nickname，供 voice handler 与周期刷新使用
    let client_directory: ClientDirectory = Arc::new(Mutex::new(HashMap::new()));
    refresh_client_directory(&client_directory, &client).await;

    // 进出频道通知即时更新目录，消除 clid 复用时的陈旧名称窗口
    {
        let enter_directory = client_directory.clone();
        client.on_client_enter(Arc::new(move |event: tsclient_rs::Event| {
            if let tsclient_rs::Event::ClientEnter(ref info) = event {
                let mut dir = enter_directory.lock().expect("client directory poisoned");
                dir.insert(info.id, info.nickname.clone());
            }
        }));
        let leave_directory = client_directory.clone();
        client.on_client_leave(Arc::new(move |event: tsclient_rs::Event| {
            if let tsclient_rs::Event::ClientLeave(ref info) = event {
                let mut dir = leave_directory.lock().expect("client directory poisoned");
                dir.remove(&info.id);
            }
        }));
    }

    // voice data → AudioFrameEvent
    let audio_tx_v = channels.audio_tx.clone();
    let voice_directory = client_directory.clone();
    client.on_voice_data(Arc::new(move |event: tsclient_rs::Event| {
        if let tsclient_rs::Event::VoiceData(ref vd) = event {
            let Ok(from_client_id) = u32::try_from(vd.client_id) else {
                warn!(
                    client_id = vd.client_id,
                    "忽略调用者 ID 无效的 TeamSpeak 音频帧"
                );
                return;
            };
            let from_client_name = voice_directory
                .lock()
                .expect("client directory poisoned")
                .get(&vd.client_id)
                .cloned()
                .unwrap_or_default();
            let _ = audio_tx_v.send(voicev1::Event {
                payload: Some(voicev1::event::Payload::Audio(voicev1::AudioFrameEvent {
                    from_client_id,
                    from_client_name,
                    codec: vd.codec,
                    frame: vd.data.to_vec(),
                })),
            });
        }
    }));

    // 周期刷新客户端目录，保证 clid 复用后名称不陈旧
    let mut directory_refresh_tick = tokio::time::interval(Duration::from_secs(60));
    directory_refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => {
                break;
            }

            _ = directory_refresh_tick.tick() => {
                refresh_client_directory(&client_directory, &client).await;
            }

            pkt = audio_rx.recv(), if out_buf.len() < OUT_BUF_MAX => {
                if let Some(p) = pkt {
                    out_buf.push_back(p);
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
