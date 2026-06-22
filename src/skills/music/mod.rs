pub mod ts3audiobot;
pub mod tsbot_http;

use crate::config::MusicBackendConfig;
use crate::skills::{ExecutionContext, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

async fn dispatch_backend(
    action: &str,
    args: &Value,
    cfg: &MusicBackendConfig,
    ts_ctx: Option<&ExecutionContext<'_>>,
) -> Result<Value> {
    match cfg.backend.as_str() {
        "tsbot_backend" => tsbot_http::execute(action, args, &cfg.base_url).await,
        "ts3audiobot" => {
            let ctx = ts_ctx
                .ok_or_else(|| anyhow::anyhow!("ts3audiobot backend requires TeamSpeak context"))?;
            ts3audiobot::execute(action, args, ctx).await
        }
        other => Err(anyhow::anyhow!(
            "Unknown music backend '{}'. Valid options: ts3audiobot, tsbot_backend",
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
            "ts3audiobot" => "Control the TS3AudioBot music player via chat commands. \
                             Use ts_* actions to play songs, manage playlists, and switch modes.",
            "tsbot_backend" => "Control the NeteaseTSBot music player. Search and play songs from NetEase Music and QQ Music, \
                                manage the queue, and control playback.",
            _ => "Control the music player.",
        }
    }

    fn parameters(&self) -> Value {
        match self.backend.as_str() {
            "ts3audiobot" => ts3audiobot_schema(),
            "tsbot_backend" => tsbot_schema(),
            _ => ts3audiobot_schema(),
        }
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing action"))?;

        dispatch_backend(
            action,
            &args,
            &ctx.config.music_backend,
            Some(ctx),
        )
        .await
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

        match ctx.platform {
            crate::skills::Platform::TeamSpeak => {
                let ts_ctx = ctx.to_ts_ctx()?;
                dispatch_backend(action, &args, cfg, Some(&ts_ctx)).await
            }
            crate::skills::Platform::NapCat => {
                let ts_ctx = ctx.to_ts_ctx()?;
                info!("MusicControl: NC request forwarded to TS");
                dispatch_backend(action, &args, cfg, Some(&ts_ctx)).await
            }
        }
    }
}

// ── Schema generators ──────────────────────────────────────────

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
