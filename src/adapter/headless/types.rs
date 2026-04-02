use std::fs;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use tsproto_packets::packets::{OutCommand, OutPacket};

use super::tsbot::voice::v1 as voicev1;

#[derive(Default)]
pub struct SharedStatus {
    pub state: i32,
    pub now_playing_title: String,
    pub now_playing_source_url: String,
    pub volume_percent: i32,
    pub fx_pan: f32,
    pub fx_width: f32,
    pub fx_swap_lr: bool,
    pub fx_bass_db: f32,
    pub fx_reverb_mix: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedVoiceState {
    pub volume_percent: i32,
    pub fx_pan: f32,
    pub fx_width: f32,
    pub fx_swap_lr: bool,
    pub fx_bass_db: f32,
    pub fx_reverb_mix: f32,
}

impl Default for PersistedVoiceState {
    fn default() -> Self {
        Self {
            volume_percent: 100,
            fx_pan: 0.0,
            fx_width: 1.0,
            fx_swap_lr: false,
            fx_bass_db: 0.0,
            fx_reverb_mix: 0.0,
        }
    }
}

impl PersistedVoiceState {
    pub fn from_status(st: &SharedStatus) -> Self {
        Self {
            volume_percent: st.volume_percent,
            fx_pan: st.fx_pan,
            fx_width: st.fx_width,
            fx_swap_lr: st.fx_swap_lr,
            fx_bass_db: st.fx_bass_db,
            fx_reverb_mix: st.fx_reverb_mix,
        }
    }
}

pub struct VoiceServiceHandle {
    pub status: Arc<Mutex<SharedStatus>>,
    pub events_tx: broadcast::Sender<voicev1::Event>,
    pub ts3_audio_tx: mpsc::Sender<OutPacket>,
    pub ts3_notice_tx: mpsc::Sender<(i32, String)>,
    pub ts3_cmd_tx: mpsc::Sender<OutCommand>,
    pub cancel: CancellationToken,
}

pub fn load_persisted_voice_state(path: &Path) -> Option<PersistedVoiceState> {
    let raw = fs::read_to_string(path).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str::<PersistedVoiceState>(raw).ok()
}

pub fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => 0,
    }
}

pub fn emit_log(events_tx: &broadcast::Sender<voicev1::Event>, level: i32, msg: impl Into<String>) {
    let _ = events_tx.send(voicev1::Event {
        unix_ms: now_unix_ms(),
        payload: Some(voicev1::event::Payload::Log(voicev1::LogEvent {
            level,
            message: msg.into(),
        })),
    });
}

pub fn emit_playback(
    events_tx: &broadcast::Sender<voicev1::Event>,
    ty: i32,
    title: impl Into<String>,
    source_url: impl Into<String>,
    detail: impl Into<String>,
) {
    let _ = events_tx.send(voicev1::Event {
        unix_ms: now_unix_ms(),
        payload: Some(voicev1::event::Payload::Playback(voicev1::PlaybackEvent {
            r#type: ty,
            title: title.into(),
            source_url: source_url.into(),
            detail: detail.into(),
        })),
    });
}
