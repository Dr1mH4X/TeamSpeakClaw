//! TS3 actor：音频发送（唯一 pacer）+ 客户端目录（唯一写者，经命令通道）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::tsbot::voice::v1 as voicev1;
use super::SpeakerRecordHook;

/// 输出缓冲上限：满时暂停读取上游（背压），不再丢弃旧帧
const OUT_BUF_MAX: usize = 400;
/// 单 pacer 帧间隔
const VOICE_FRAME_MS: u64 = 20;
/// 落后超过该值则重定位发送时刻，禁止倾泻
const PACER_RELOCATE_LAG: Duration = Duration::from_millis(200);
/// 目录命令通道容量
const DIR_CMD_CAPACITY: usize = 64;

struct DirEntry {
    name: String,
    /// 本地写入序号；快照 merge 时用于保留更新的 enter/upsert
    seq: u64,
}

#[derive(Default)]
struct DirectoryState {
    clients: HashMap<i32, DirEntry>,
}

type ClientDirectory = Arc<Mutex<DirectoryState>>;

/// 目录变更命令：actor 是唯一写者
enum DirCmd {
    Upsert {
        clid: i32,
        name: String,
    },
    Remove {
        clid: i32,
    },
    Snapshot {
        clients: Vec<(i32, String)>,
        started_seq: u64,
    },
}

/// actor 事件输出通道：控制事件与音频事件分离
pub struct ActorEventChannels {
    pub control_tx: broadcast::Sender<voicev1::Event>,
    pub audio_tx: broadcast::Sender<voicev1::Event>,
}

/// 发送节奏：空闲复位、稳态 +20ms、落后 >200ms 重定位
struct VoicePacer {
    next_send_at: Option<Instant>,
    frame: Duration,
    max_lag: Duration,
}

impl VoicePacer {
    fn new() -> Self {
        Self {
            next_send_at: None,
            frame: Duration::from_millis(VOICE_FRAME_MS),
            max_lag: PACER_RELOCATE_LAG,
        }
    }

    fn due(&mut self, now: Instant, has_packet: bool) -> bool {
        if !has_packet {
            self.next_send_at = None;
            return false;
        }
        match self.next_send_at {
            None => {
                self.next_send_at = Some(now);
                true
            }
            Some(next) => {
                if now.saturating_duration_since(next) > self.max_lag {
                    self.next_send_at = Some(now);
                    true
                } else {
                    now >= next
                }
            }
        }
    }

    fn advance(&mut self) {
        if let Some(next) = self.next_send_at.take() {
            self.next_send_at = Some(next + self.frame);
        }
    }
}

fn should_record_speaker(hook: &SpeakerRecordHook, clid: u32, name: &str) -> bool {
    if hook.bot_clid != 0 && clid == hook.bot_clid {
        return false;
    }
    if !hook.musicbot_name.is_empty()
        && name
            .to_ascii_lowercase()
            .contains(&hook.musicbot_name.to_ascii_lowercase())
    {
        return false;
    }
    true
}

fn read_dir_name(directory: &ClientDirectory, clid: i32) -> String {
    directory
        .lock()
        .expect("client directory poisoned")
        .clients
        .get(&clid)
        .map(|e| e.name.clone())
        .unwrap_or_default()
}

#[cfg(test)]
fn read_dir_name_from_state(state: &DirectoryState, clid: i32) -> String {
    state
        .clients
        .get(&clid)
        .map(|e| e.name.clone())
        .unwrap_or_default()
}

/// actor 唯一写者：应用目录命令；快照 merge，不覆盖 started_seq 之后的本地事件
fn apply_dir_cmd(state: &mut DirectoryState, cmd: DirCmd, seq: &AtomicU64) {
    match cmd {
        DirCmd::Upsert { clid, name } => {
            let s = seq.fetch_add(1, Ordering::SeqCst) + 1;
            state.clients.insert(clid, DirEntry { name, seq: s });
        }
        DirCmd::Remove { clid } => {
            state.clients.remove(&clid);
        }
        DirCmd::Snapshot {
            clients,
            started_seq,
        } => {
            let snap_ids: HashSet<i32> = clients.iter().map(|(id, _)| *id).collect();
            for (id, name) in clients {
                match state.clients.entry(id) {
                    std::collections::hash_map::Entry::Occupied(mut e) => {
                        if e.get().seq <= started_seq {
                            e.get_mut().name = name;
                        }
                    }
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(DirEntry { name, seq: 0 });
                    }
                }
            }
            state
                .clients
                .retain(|id, e| snap_ids.contains(id) || e.seq > started_seq);
        }
    }
}

async fn fetch_clients(client: &tsclient_rs::Client) -> Result<Vec<(i32, String)>> {
    let clients = tsclient_rs::listClients(client)
        .await
        .map_err(|e| anyhow::anyhow!("listClients failed: {e}"))?;
    Ok(clients.into_iter().map(|c| (c.id, c.nickname)).collect())
}

/// 周期 listClients：独立 task，结果经 DirCmd 交给 actor，不占用发送分支
fn spawn_directory_refresher(
    client: Arc<tsclient_rs::Client>,
    dir_tx: mpsc::Sender<DirCmd>,
    seq: Arc<AtomicU64>,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tick.tick() => {}
            }
            let started_seq = seq.load(Ordering::SeqCst);
            match fetch_clients(&client).await {
                Ok(clients) => {
                    if dir_tx
                        .send(DirCmd::Snapshot {
                            clients,
                            started_seq,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => warn!("刷新 TeamSpeak 客户端目录失败: {e}"),
            }
        }
    });
}

pub async fn ts3_actor(
    client: Arc<tsclient_rs::Client>,
    mut audio_rx: mpsc::Receiver<(Vec<u8>, i32)>,
    channels: ActorEventChannels,
    shutdown_token: CancellationToken,
    bridge_state: super::VoiceBridgeState,
    record_hook: Option<SpeakerRecordHook>,
) -> Result<()> {
    let mut out_buf: VecDeque<(Vec<u8>, i32)> = VecDeque::with_capacity(OUT_BUF_MAX);
    let mut pacer = VoicePacer::new();
    let mut send_interval = tokio::time::interval(Duration::from_millis(VOICE_FRAME_MS));
    send_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let control_tx_t = channels.control_tx.clone();
    client.on_text_message(Arc::new(move |event: tsclient_rs::Event| {
        if let tsclient_rs::Event::TextMessage(ref msg) = event {
            let target_mode = match msg.target_mode {
                1..=3 => msg.target_mode,
                mode => {
                    warn!(mode, "忽略未知类型的 TeamSpeak 文本消息");
                    return;
                }
            };
            let Ok(invoker_client_id) = u32::try_from(msg.invoker_id) else {
                warn!(
                    invoker_client_id = msg.invoker_id,
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

    bridge_state.set_actor_ready(true);

    let client_directory: ClientDirectory = Arc::new(Mutex::new(DirectoryState::default()));
    let dir_seq = Arc::new(AtomicU64::new(0));
    let (dir_tx, mut dir_rx) = mpsc::channel::<DirCmd>(DIR_CMD_CAPACITY);

    // 启动时先拉一次目录，再交给周期 task（先 await 再加锁，避免 Guard 跨 await）
    match fetch_clients(&client).await {
        Ok(clients) => {
            let mut state = client_directory.lock().expect("client directory poisoned");
            apply_dir_cmd(
                &mut state,
                DirCmd::Snapshot {
                    clients,
                    started_seq: 0,
                },
                &dir_seq,
            );
        }
        Err(e) => warn!("初始化 TeamSpeak 客户端目录失败: {e}"),
    }
    spawn_directory_refresher(
        client.clone(),
        dir_tx.clone(),
        dir_seq.clone(),
        shutdown_token.clone(),
    );

    // enter/leave 只发命令，不直接写目录
    {
        let enter_tx = dir_tx.clone();
        client.on_client_enter(Arc::new(move |event: tsclient_rs::Event| {
            if let tsclient_rs::Event::ClientEnter(ref info) = event {
                let _ = enter_tx.try_send(DirCmd::Upsert {
                    clid: info.id,
                    name: info.nickname.clone(),
                });
            }
        }));
        let leave_tx = dir_tx.clone();
        client.on_client_leave(Arc::new(move |event: tsclient_rs::Event| {
            if let tsclient_rs::Event::ClientLeave(ref info) = event {
                let _ = leave_tx.try_send(DirCmd::Remove { clid: info.id });
            }
        }));
    }

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
            let from_client_name = read_dir_name(&voice_directory, vd.client_id);
            if let Some(hook) = record_hook.as_ref() {
                if matches!(vd.codec, 4 | 5)
                    && should_record_speaker(hook, from_client_id, &from_client_name)
                {
                    let _ = hook.rings.push_opus_frame(
                        from_client_id,
                        &from_client_name,
                        vd.codec,
                        &vd.data,
                        Instant::now(),
                    );
                }
            }
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

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => {
                break;
            }

            Some(cmd) = dir_rx.recv() => {
                let mut state = client_directory.lock().expect("client directory poisoned");
                apply_dir_cmd(&mut state, cmd, &dir_seq);
            }

            pkt = audio_rx.recv(), if out_buf.len() < OUT_BUF_MAX => {
                if let Some(p) = pkt {
                    out_buf.push_back(p);
                } else {
                    break;
                }
            }

            _ = send_interval.tick() => {
                let now = Instant::now();
                if pacer.due(now, !out_buf.is_empty()) {
                    if let Some((data, codec)) = out_buf.pop_front() {
                        client.send_voice(data, codec);
                        pacer.advance();
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::headless::SpeakerRings;
    use std::time::Duration;

    fn hook(bot_clid: u32, musicbot_name: &str) -> SpeakerRecordHook {
        SpeakerRecordHook {
            rings: Arc::new(SpeakerRings::new(Duration::from_secs(30))),
            bot_clid,
            musicbot_name: musicbot_name.to_string(),
        }
    }

    #[test]
    fn record_hook_skips_bot_and_musicbot() {
        let hook = hook(7, "TS3AudioBot");
        assert!(!should_record_speaker(&hook, 7, "claw"));
        assert!(!should_record_speaker(&hook, 3, "ts3audiobot-music"));
        assert!(should_record_speaker(&hook, 3, "alice"));
        assert!(should_record_speaker(&hook, 0, "alice"));
    }

    #[test]
    fn record_hook_allows_when_bot_clid_unknown() {
        let hook = hook(0, "");
        assert!(should_record_speaker(&hook, 7, "anyone"));
    }

    #[test]
    fn pacer_resets_on_idle_then_fires_immediately() {
        let mut pacer = VoicePacer::new();
        let t0 = Instant::now();
        assert!(pacer.due(t0, true));
        pacer.advance();
        assert!(!pacer.due(t0 + Duration::from_millis(5), false));
        assert!(pacer.next_send_at.is_none());
        assert!(pacer.due(t0 + Duration::from_millis(10), true));
    }

    #[test]
    fn pacer_relocates_when_lag_exceeds_200ms() {
        let mut pacer = VoicePacer::new();
        let t0 = Instant::now();
        assert!(pacer.due(t0, true));
        pacer.advance();
        let late = t0 + Duration::from_millis(250);
        assert!(pacer.due(late, true));
        assert_eq!(pacer.next_send_at, Some(late));
    }

    #[test]
    fn pacer_advances_monotonically_by_frame() {
        let mut pacer = VoicePacer::new();
        let t0 = Instant::now();
        assert!(pacer.due(t0, true));
        pacer.advance();
        assert_eq!(pacer.next_send_at, Some(t0 + Duration::from_millis(20)));
        assert!(!pacer.due(t0 + Duration::from_millis(10), true));
        assert!(pacer.due(t0 + Duration::from_millis(20), true));
        pacer.advance();
        assert_eq!(pacer.next_send_at, Some(t0 + Duration::from_millis(40)));
    }

    #[test]
    fn snapshot_merge_keeps_local_upsert_after_started_seq() {
        let seq = AtomicU64::new(0);
        let mut state = DirectoryState::default();
        apply_dir_cmd(
            &mut state,
            DirCmd::Snapshot {
                clients: vec![(1, "old".into())],
                started_seq: 0,
            },
            &seq,
        );
        apply_dir_cmd(
            &mut state,
            DirCmd::Upsert {
                clid: 2,
                name: "newcomer".into(),
            },
            &seq,
        );
        let started = seq.load(Ordering::SeqCst);
        apply_dir_cmd(
            &mut state,
            DirCmd::Snapshot {
                clients: vec![(1, "old".into())],
                started_seq: started.saturating_sub(1),
            },
            &seq,
        );
        assert_eq!(read_dir_name_from_state(&state, 2), "newcomer");
        assert_eq!(read_dir_name_from_state(&state, 1), "old");
    }

    #[test]
    fn snapshot_merge_drops_stale_absent_clients() {
        let seq = AtomicU64::new(0);
        let mut state = DirectoryState::default();
        apply_dir_cmd(
            &mut state,
            DirCmd::Upsert {
                clid: 9,
                name: "ghost".into(),
            },
            &seq,
        );
        apply_dir_cmd(
            &mut state,
            DirCmd::Snapshot {
                clients: vec![(1, "alice".into())],
                started_seq: seq.load(Ordering::SeqCst),
            },
            &seq,
        );
        assert_eq!(read_dir_name_from_state(&state, 9), "");
        assert_eq!(read_dir_name_from_state(&state, 1), "alice");
    }
}
