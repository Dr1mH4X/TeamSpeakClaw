use crate::adapter::command::cmd_send_text;
use crate::error::Result;
use crate::skills::{ExecutionContext, Skill};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct MusicControl;

#[async_trait]
impl Skill for MusicControl {
    fn name(&self) -> &'static str {
        "music_control"
    }

    fn description(&self) -> &'static str {
        "Control the TS3AudioBot music player. Use this when the user wants to play music, \
         add songs, switch tracks, change play mode, or manage playlists on NetEase Music (网易云). \
         Finds the TS3AudioBot client and sends it the appropriate command."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "The action to perform.",
                    "enum": ["play", "add", "gedan", "gedanid", "playid", "addid", "next", "mode", "login"]
                },
                "value": {
                    "type": "string",
                    "description": "Song name, playlist name, song ID, playlist ID, or mode number (0-3). Not required for 'next' and 'login'."
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let action = args["action"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing action"))?;
        let value = args["value"].as_str().unwrap_or("");

        // 构造发给 TS3AudioBot 的命令
        let bot_cmd = match action {
            "play" => format!("!yun play {}", value),
            "add" => format!("!yun add {}", value),
            "gedan" => format!("!yun gedan {}", value),
            "gedanid" => format!("!yun gedanid {}", value),
            "playid" => format!("!yun playid {}", value),
            "addid" => format!("!yun addid {}", value),
            "next" => "!yun next".to_string(),
            "mode" => format!("!yun mode {}", value),
            "login" => "!yun login".to_string(),
            _ => return Err(anyhow::anyhow!("Unknown action: {}", action).into()),
        };

        // 在缓存里找 TS3AudioBot 的 clid
        let clients = ctx.cache.list_clients();
        let audiobot = clients
            .iter()
            .find(|c| c.nickname == "TS3AudioBot")
            .ok_or_else(|| anyhow::anyhow!("TS3AudioBot not found online"))?;

        // 发私信给 TS3AudioBot
        ctx.adapter
            .send_raw(&cmd_send_text(1, audiobot.clid, &bot_cmd))
            .await?;

        Ok(json!({
            "status": "ok",
            "sent_to": "TS3AudioBot",
            "command": bot_cmd
        }))
    }
}
