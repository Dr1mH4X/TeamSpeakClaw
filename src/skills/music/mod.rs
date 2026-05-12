pub mod ncm_api;
pub mod ts3audiobot;
pub mod tsbot_http;
pub mod unm;

use crate::config::{MusicBackendConfig, MusicNcmApiConfig};
use crate::skills::{ExecutionContext, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

/// Internal control field: audio URL for the bridge to play.
/// Must be stripped before returning tool results to the LLM.
pub(crate) const PLAY_URL_KEY: &str = "__play_url";
/// Internal control field: song title for the playback UI.
/// Must be stripped before returning tool results to the LLM.
pub(crate) const PLAY_TITLE_KEY: &str = "__play_title";

async fn dispatch_backend(
    action: &str,
    args: &Value,
    cfg: &MusicBackendConfig,
    ncm_cfg: &MusicNcmApiConfig,
    ts_ctx: Option<&ExecutionContext<'_>>,
) -> Result<Value> {
    match cfg.backend.as_str() {
        "tsbot_backend" => tsbot_http::execute(action, args, &cfg.base_url).await,
        "ncm_api" => ncm_api::execute(action, args, ncm_cfg).await,
        "ts3audiobot" => {
            let ctx = ts_ctx
                .ok_or_else(|| anyhow::anyhow!("ts3audiobot backend requires TeamSpeak context"))?;
            ts3audiobot::execute(action, args, ctx).await
        }
        other => Err(anyhow::anyhow!(
            "Unknown music backend '{}'. Valid options: ts3audiobot, tsbot_backend, ncm_api",
            other
        )),
    }
}

pub struct MusicControl {
    backend: String,
}

impl MusicControl {
    pub fn new(backend: &str) -> Self {
        Self {
            backend: backend.to_string(),
        }
    }
}

#[async_trait]
impl Skill for MusicControl {
    fn name(&self) -> &'static str {
        "music_control"
    }

    fn description(&self) -> &'static str {
        match self.backend.as_str() {
            "ncm_api" => "Control the music player. Search, play, and manage a song queue from NetEase Music (网易云). \
                          Supports playback control (play/pause/next/previous), queue management, repeat/shuffle modes, \
                          volume and audio effects.",
            "ts3audiobot" => "Control the TS3AudioBot music player via chat commands. \
                             Use ts_* actions to play songs, manage playlists, and switch modes.",
            "tsbot_backend" => "Control the NeteaseTSBot music player. Search and play songs from NetEase Music and QQ Music, \
                                manage the queue, and control playback.",
            _ => "Control the music player.",
        }
    }

    fn parameters(&self) -> Value {
        match self.backend.as_str() {
            "ncm_api" => ncm_api_schema(),
            "ts3audiobot" => ts3audiobot_schema(),
            "tsbot_backend" => tsbot_schema(),
            _ => generic_schema(),
        }
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing action"))?;

        dispatch_backend(action, &args, &ctx.config.music_backend, &ctx.config.music_ncm_api, Some(ctx)).await
    }

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!(
            "MusicControl: unified execution, platform={:?}",
            ctx.platform
        );

        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing action"))?;

        let cfg = &ctx.config.music_backend;
        let ncm_cfg = &ctx.config.music_ncm_api;

        match ctx.platform {
            crate::skills::Platform::TeamSpeak => {
                let ts_ctx = ctx.to_ts_ctx()?;
                dispatch_backend(action, &args, cfg, ncm_cfg, Some(&ts_ctx)).await
            }
            crate::skills::Platform::NapCat => {
                let ts_ctx = ctx.to_ts_ctx()?;
                info!("MusicControl: NC request forwarded to TS");
                dispatch_backend(action, &args, cfg, ncm_cfg, Some(&ts_ctx)).await
            }
        }
    }
}

// ── Schema generators ──────────────────────────────────────────

fn ncm_api_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "description": "The action to perform. Use 'search' to find songs, then 'play' with the song_id. \
                               Use 'queue_netease' to add songs to the queue.",
                "enum": [
                    "search", "play", "queue_netease",
                    "next", "previous",
                    "repeat", "shuffle",
                    "pause", "stop", "seek",
                    "volume", "fx"
                ]
            },
            "keywords": {
                "type": "string",
                "description": "Search keywords for 'search' action."
            },
            "song_id": {
                "type": "string",
                "description": "NetEase song ID. Required for 'play' and 'queue_netease'."
            },
            "title": {
                "type": "string",
                "description": "Song title (required for queue_netease)."
            },
            "artist": {
                "type": "string",
                "description": "Artist name."
            },
            "play_now": {
                "type": "boolean",
                "description": "If true, play immediately instead of appending to queue."
            },
            "limit": {
                "type": "integer",
                "description": "Search result limit for 'search' action."
            },
            "mode": {
                "type": "integer",
                "description": "Play mode: 0=sequential, 1=sequential loop, 2=random, 3=random loop.",
                "enum": [0, 1, 2, 3]
            },
            "repeat_mode": {
                "type": "string",
                "description": "Repeat mode: 'none'(=0), 'one'(=1), 'all'(=1). Prefer 'mode' for full control.",
                "enum": ["none", "one", "all"]
            },
            "shuffle_enabled": {
                "type": "boolean",
                "description": "Enable or disable shuffle for 'shuffle' action."
            },
            "seek_time": {
                "type": "number",
                "description": "Seek position in seconds for 'seek' action."
            },
            "volume_percent": {
                "type": "integer",
                "description": "Volume 0-100 for 'volume' action."
            },
            "fx_pan": { "type": "number", "description": "Stereo pan (-1.0 ~ 1.0)." },
            "fx_bass_db": { "type": "number", "description": "Bass boost in dB." },
            "fx_reverb_mix": { "type": "number", "description": "Reverb mix 0.0 ~ 1.0." }
        },
        "required": ["action"]
    })
}

fn ts3audiobot_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "description": "The action to perform. All ts_* actions send commands to TS3AudioBot via chat.",
                "enum": [
                    "ts_play", "ts_add", "ts_gedan", "ts_gedanid",
                    "ts_playid", "ts_addid", "ts_mode", "ts_login"
                ]
            },
            "value": {
                "type": "string",
                "description": "Action argument: song name, playlist name/ID, mode number (0=sequential, 1=sequential loop, 2=random, 3=random loop), etc."
            }
        },
        "required": ["action"]
    })
}

fn tsbot_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "description": "The action to perform.",
                "enum": [
                    "play", "pause", "next", "previous", "skip", "seek",
                    "search",
                    "queue_netease", "queue_qqmusic"
                ]
            },
            "keywords": {
                "type": "string",
                "description": "Search keywords for 'search' action."
            },
            "song_id": {
                "type": "string",
                "description": "NetEase song ID for 'queue_netease'."
            },
            "title": {
                "type": "string",
                "description": "Song title (required for queue_netease / queue_qqmusic)."
            },
            "artist": {
                "type": "string",
                "description": "Artist name."
            },
            "play_now": {
                "type": "boolean",
                "description": "If true, play immediately instead of appending to queue."
            },
            "song_mid": {
                "type": "string",
                "description": "QQ Music song mid for 'queue_qqmusic'."
            },
            "quality": {
                "type": "string",
                "description": "Audio quality for QQ Music: '128', '320', 'flac'. Default '320'."
            },
            "seek_time": {
                "type": "number",
                "description": "Seek position in seconds for 'seek' action."
            },
            "limit": {
                "type": "integer",
                "description": "Search result limit for 'search' action."
            }
        },
        "required": ["action"]
    })
}

fn generic_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "description": "The action to perform.",
                "enum": ["play", "pause", "next", "previous", "search"]
            }
        },
        "required": ["action"]
    })
}
