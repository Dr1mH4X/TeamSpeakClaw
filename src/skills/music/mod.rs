pub mod command;
pub mod ncm_api;
pub mod ts3audiobot;
pub mod tsbot_http;

use crate::skills::{ExecutionContext, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

pub struct MusicControl;

#[async_trait]
impl Skill for MusicControl {
    fn name(&self) -> &'static str {
        "music_control"
    }

    fn description(&self) -> &'static str {
        "Control the music player. Supports playback control (play/pause/next/previous/seek), \
         searching and queuing songs from NetEase Music (网易云) or QQ Music (QQ音乐), \
         adjusting volume and audio effects (FX), and setting repeat/shuffle modes. \
         The backend is configured automatically; just call the appropriate action."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "The action to perform.",
                    "enum": [
                        "play", "pause", "next", "previous", "skip", "seek",
                        "search",
                        "queue_netease",
                        "queue_qqmusic",
                        "repeat", "shuffle",
                        "volume", "fx",
                        "ts_play", "ts_add", "ts_gedan", "ts_gedanid",
                        "ts_playid", "ts_addid", "ts_mode", "ts_login"
                    ]
                },
                "keywords": {
                    "type": "string",
                    "description": "Search keywords for 'search' action."
                },
                "song_id": {
                    "type": "string",
                    "description": "NetEase song ID for 'queue_netease' or 'play' action."
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
                "repeat_mode": {
                    "type": "string",
                    "description": "Repeat mode: 'none', 'one', 'all'.",
                    "enum": ["none", "one", "all"]
                },
                "shuffle_enabled": {
                    "type": "boolean",
                    "description": "Enable or disable shuffle for 'shuffle' action."
                },
                "volume_percent": {
                    "type": "integer",
                    "description": "Volume 0-100 for 'volume' action."
                },
                "fx_pan": { "type": "number", "description": "Stereo pan (-1.0 ~ 1.0)." },
                "fx_bass_db": { "type": "number", "description": "Bass boost in dB." },
                "fx_reverb_mix": { "type": "number", "description": "Reverb mix 0.0 ~ 1.0." },
                "limit": { "type": "integer", "description": "Search result limit for 'search' action." },
                "value": {
                    "type": "string",
                    "description": "Generic value for ts_* actions (song name, playlist name, ID, mode number, etc.)."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing action"))?;

        let backend_cfg = &ctx.config.music_backend;

        match backend_cfg.backend.as_str() {
            "tsbot_backend" => tsbot_http::execute(action, &args, &backend_cfg.base_url).await,
            "ncm_api" => ncm_api::execute(action, &args, backend_cfg).await,
            _ => ts3audiobot::execute(action, &args, ctx).await,
        }
    }

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!(
            "MusicControl: unified execution, platform={:?}",
            ctx.platform
        );

        let backend_cfg = &ctx.config.music_backend;

        match ctx.platform {
            crate::skills::Platform::TeamSpeak => {
                let action = args["action"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing action"))?;

                let ts_ctx = ctx.to_ts_ctx()?;

                match backend_cfg.backend.as_str() {
                    "tsbot_backend" => {
                        tsbot_http::execute(action, &args, &backend_cfg.base_url).await
                    }
                    "ncm_api" => ncm_api::execute(action, &args, backend_cfg).await,
                    _ => ts3audiobot::execute(action, &args, &ts_ctx).await,
                }
            }
            crate::skills::Platform::NapCat => {
                let action = args["action"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing action"))?;

                let ts_ctx = ctx.to_ts_ctx()?;
                info!("MusicControl: NC request forwarded to TS");

                match backend_cfg.backend.as_str() {
                    "tsbot_backend" => {
                        tsbot_http::execute(action, &args, &backend_cfg.base_url).await
                    }
                    "ncm_api" => ncm_api::execute(action, &args, backend_cfg).await,
                    _ => ts3audiobot::execute(action, &args, &ts_ctx).await,
                }
            }
        }
    }
}
