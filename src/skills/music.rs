use crate::adapter::command::cmd_send_text;
use crate::skills::{ExecutionContext, Skill};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct MusicControl;

// ─────────────────────────────────────────────
// HTTP 后端客户端（NeteaseTSBot-backend OpenAPI）
// ─────────────────────────────────────────────

struct HttpBackend {
    base_url: String,
    client: reqwest::Client,
}

impl HttpBackend {
    fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
        }
    }

    async fn post(&self, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let req = self.client.post(&url);
        let resp = if let Some(b) = body {
            req.json(&b)
        } else {
            req
        }
        .send()
        .await?;

        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!("HTTP {} from {}: {}", status, path, text));
        }
        Ok(serde_json::from_str(&text).unwrap_or(json!({"raw": text})))
    }

    async fn put(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.put(&url).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!("HTTP {} from {}: {}", status, path, text));
        }
        Ok(serde_json::from_str(&text).unwrap_or(json!({"raw": text})))
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.get(&url).query(query).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!("HTTP {} from {}: {}", status, path, text));
        }
        Ok(serde_json::from_str(&text).unwrap_or(json!({"raw": text})))
    }
}

// ─────────────────────────────────────────────
// Skill 实现
// ─────────────────────────────────────────────

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
                        // ── 播放控制 ──
                        "play", "pause", "next", "previous", "skip", "seek",
                        // ── 搜索 & 入队 ──
                        "search",
                        "queue_netease",   // 网易云：按 song_id 加入队列
                        "queue_qqmusic",   // QQ音乐：按 song_mid 加入队列
                        // ── 模式 ──
                        "repeat", "shuffle",
                        // ── 音量 & 音效 ──
                        "volume", "fx",
                        // ── TS3AudioBot 兼容（仅 ts3audiobot 后端生效）──
                        "ts_play", "ts_add", "ts_gedan", "ts_gedanid",
                        "ts_playid", "ts_addid", "ts_mode", "ts_login"
                    ]
                },
                // 搜索关键词
                "keywords": {
                    "type": "string",
                    "description": "Search keywords for 'search' action."
                },
                // 网易云入队
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
                // QQ音乐入队
                "song_mid": {
                    "type": "string",
                    "description": "QQ Music song mid for 'queue_qqmusic'."
                },
                "quality": {
                    "type": "string",
                    "description": "Audio quality for QQ Music: '128', '320', 'flac'. Default '320'."
                },
                // seek
                "seek_time": {
                    "type": "number",
                    "description": "Seek position in seconds for 'seek' action."
                },
                // repeat
                "repeat_mode": {
                    "type": "string",
                    "description": "Repeat mode: 'none', 'one', 'all'.",
                    "enum": ["none", "one", "all"]
                },
                // shuffle
                "shuffle_enabled": {
                    "type": "boolean",
                    "description": "Enable or disable shuffle for 'shuffle' action."
                },
                // volume
                "volume_percent": {
                    "type": "integer",
                    "description": "Volume 0-100 for 'volume' action."
                },
                // fx
                "fx_pan": { "type": "number", "description": "Stereo pan (-1.0 ~ 1.0)." },
                "fx_bass_db": { "type": "number", "description": "Bass boost in dB." },
                "fx_reverb_mix": { "type": "number", "description": "Reverb mix 0.0 ~ 1.0." },
                // search
                "limit": { "type": "integer", "description": "Search result limit for 'search' action." },
                // TS3AudioBot 兼容参数
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

        // 读取后端配置
        let backend_cfg = &ctx.config.music_backend;

        match backend_cfg.backend.as_str() {
            "tsbot_backend" => execute_http(action, &args, &backend_cfg.base_url).await,
            // 默认 / "ts3audiobot"
            _ => execute_ts3audiobot(action, &args, ctx).await,
        }
    }
}

// ─────────────────────────────────────────────
// HTTP 后端执行（tsbot-backend）
// ─────────────────────────────────────────────

async fn execute_http(action: &str, args: &Value, base_url: &str) -> Result<Value> {
    let http = HttpBackend::new(base_url);

    match action {
        "play" => http.post("/voice/play", None).await,
        "pause" => http.post("/voice/pause", None).await,
        "next" => http.post("/voice/next", None).await,
        "previous" => http.post("/voice/previous", None).await,
        "skip" => http.post("/voice/skip", None).await,

        "seek" => {
            let t = args["seek_time"]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("Missing seek_time"))?;
            http.post("/voice/seek", Some(json!({"time": t}))).await
        }

        "search" => {
            let kw = args["keywords"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing keywords"))?;
            let limit = args["limit"].as_u64().unwrap_or(10).to_string();
            http.get("/search", &[("keywords", kw), ("limit", &limit)])
                .await
        }

        "queue_netease" => {
            let song_id = args["song_id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing song_id"))?;
            let title = args["title"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing title"))?;
            let artist = args["artist"].as_str().unwrap_or("");
            let play_now = args["play_now"].as_bool().unwrap_or(false);
            http.post(
                "/queue/netease",
                Some(json!({
                    "song_id": song_id,
                    "title": title,
                    "artist": artist,
                    "play_now": play_now
                })),
            )
            .await
        }

        "queue_qqmusic" => {
            let song_mid = args["song_mid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing song_mid"))?;
            let title = args["title"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing title"))?;
            let artist = args["artist"].as_str().unwrap_or("");
            let play_now = args["play_now"].as_bool().unwrap_or(false);
            let quality = args["quality"].as_str().unwrap_or("320");
            http.post(
                "/queue/qqmusic",
                Some(json!({
                    "song_mid": song_mid,
                    "title": title,
                    "artist": artist,
                    "play_now": play_now,
                    "quality": quality
                })),
            )
            .await
        }

        "repeat" => {
            let mode = args["repeat_mode"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("Missing repeat_mode"))?;
            http.post("/voice/repeat", Some(json!({"mode": mode})))
                .await
        }

        "shuffle" => {
            let enabled = args["shuffle_enabled"]
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("Missing shuffle_enabled"))?;
            http.post("/voice/shuffle", Some(json!({"enabled": enabled})))
                .await
        }

        "volume" => {
            let vol = args["volume_percent"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("Missing volume_percent"))?;
            http.put("/voice/volume", json!({"volume_percent": vol}))
                .await
        }

        "fx" => {
            let mut body = json!({});
            if let Some(v) = args["fx_pan"].as_f64() {
                body["pan"] = json!(v);
            }
            if let Some(v) = args["fx_bass_db"].as_f64() {
                body["bass_db"] = json!(v);
            }
            if let Some(v) = args["fx_reverb_mix"].as_f64() {
                body["reverb_mix"] = json!(v);
            }
            http.put("/voice/fx", body).await
        }

        // ts_* 系列在 HTTP 后端下不适用
        ts if ts.starts_with("ts_") => Err(anyhow::anyhow!(
            "Action '{}' is only available with the ts3audiobot backend. \
             Current backend is tsbot_backend.",
            action
        )),

        _ => Err(anyhow::anyhow!("Unknown action: {}", action)),
    }
}

// ─────────────────────────────────────────────
// TS3AudioBot 私信后端
// ─────────────────────────────────────────────

async fn execute_ts3audiobot(
    action: &str,
    args: &Value,
    ctx: &ExecutionContext<'_>,
) -> Result<Value> {
    let value = args["value"].as_str().unwrap_or("");

    // 通用 action 映射到 TS3AudioBot 命令，方便 LLM 统一调用
    let bot_cmd = match action {
        "next" => "!yun next".to_string(),
        "ts_login" => "!yun login".to_string(),
        "play" | "ts_play" => format!("!yun play {value}"),
        "ts_add" => format!("!yun add {value}"),
        "ts_gedan" => format!("!yun gedan {value}"),
        "ts_gedanid" => format!("!yun gedanid {value}"),
        "ts_playid" => format!("!yun playid {value}"),
        "ts_addid" => format!("!yun addid {value}"),
        "ts_mode" => format!("!yun mode {value}"),
        // 搜索：直接播放搜索结果第一首
        "search" => {
            let kw = args["keywords"].as_str().unwrap_or(value);
            format!("!yun play {kw}")
        }
        // 模式映射
        "repeat" => {
            let mode = args["repeat_mode"].as_str().unwrap_or("all");
            let mode_num = match mode {
                "none" => "0",
                "one" => "1",
                _ => "2",
            };
            format!("!yun mode {mode_num}")
        }
        "pause" => "!yun pause".to_string(),
        "skip" => "!yun next".to_string(),
        // 无对应实现的操作
        other => {
            return Err(anyhow::anyhow!(
                "Action '{}' is not supported by the ts3audiobot backend.",
                other
            ))
        }
    };

    // 在在线列表中找 TS3AudioBot
    let clients: Vec<_> = ctx.clients.iter().map(|r| r.value().clone()).collect();
    let audiobot = clients
        .iter()
        .find(|c| c.nickname == "TS3AudioBot")
        .ok_or_else(|| anyhow::anyhow!("TS3AudioBot not found online"))?;

    ctx.adapter
        .send_raw(&cmd_send_text(1, audiobot.clid, &bot_cmd))
        .await?;

    Ok(json!({
        "status": "ok",
        "sent_to": "TS3AudioBot",
        "command": bot_cmd
    }))
}
