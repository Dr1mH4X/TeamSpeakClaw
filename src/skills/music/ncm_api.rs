use crate::config::MusicNcmApiConfig;
use crate::skills::music::{PLAY_TITLE_KEY, PLAY_URL_KEY};
use anyhow::Result;
use ncm_api_rs::{create_client, Query};
use rand::Rng;
use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use tracing::{info, warn};

// ── 全局状态 ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SongInfo {
    id: String,
    name: String,
    artist: String,
}

/// 播放模式（对齐 ts3audiobot）
/// - 0: 顺序播放（播放完停止）
/// - 1: 顺序循环（列表循环）
/// - 2: 随机播放（随机，播完停止）
/// - 3: 随机循环（随机，循环）
#[derive(Debug, Clone, PartialEq)]
enum PlayMode {
    Sequential = 0,
    SequentialLoop = 1,
    Random = 2,
    RandomLoop = 3,
}

impl PlayMode {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Sequential),
            1 => Some(Self::SequentialLoop),
            2 => Some(Self::Random),
            3 => Some(Self::RandomLoop),
            _ => None,
        }
    }

    fn is_random(&self) -> bool {
        matches!(self, Self::Random | Self::RandomLoop)
    }

    fn is_loop(&self) -> bool {
        matches!(self, Self::SequentialLoop | Self::RandomLoop)
    }
}

#[derive(Debug)]
struct NcmPlayerState {
    queue: Vec<SongInfo>,
    current_index: usize,
    mode: PlayMode,
}

impl NcmPlayerState {
    const fn new() -> Self {
        Self {
            queue: Vec::new(),
            current_index: 0,
            mode: PlayMode::Sequential,
        }
    }
}

static PLAYER_STATE: OnceLock<Mutex<NcmPlayerState>> = OnceLock::new();

fn player_state() -> &'static Mutex<NcmPlayerState> {
    PLAYER_STATE.get_or_init(|| Mutex::new(NcmPlayerState::new()))
}

// ── NCM API 客户端 ────────────────────────────────────────────

static NCM_CLIENT: OnceLock<ncm_api_rs::ApiClient> = OnceLock::new();

fn get_client(cookie: &str) -> &'static ncm_api_rs::ApiClient {
    NCM_CLIENT.get_or_init(|| {
        let cookie_opt = if cookie.is_empty() {
            None
        } else {
            Some(cookie.to_string())
        };
        create_client(cookie_opt)
    })
}

// ── 入口 ──────────────────────────────────────────────────────

pub(crate) async fn execute(action: &str, args: &Value, cfg: &MusicNcmApiConfig) -> Result<Value> {
    match action {
        "search" => search(args, cfg).await,
        "play" => play(args, cfg).await,
        "queue_netease" => queue_netease(args, cfg).await,
        "next" => next(cfg).await,
        "previous" => previous(cfg).await,
        "repeat" => set_mode(args),
        "shuffle" => set_mode_from_shuffle(args),
        "pause" | "stop" => Ok(serde_json::json!({
            "message": "ncm_api backend does not support pause/stop. Use the bot's playback controls."
        })),
        "seek" => Ok(serde_json::json!({
            "message": "ncm_api backend does not support seek."
        })),
        "volume" | "fx" => Ok(serde_json::json!({
            "message": "ncm_api backend does not support volume/fx. Use the bot's playback controls."
        })),
        _ => Err(anyhow::anyhow!(
            "Action '{}' is not supported by the ncm_api backend.",
            action
        )),
    }
}

// ── search ────────────────────────────────────────────────────

async fn search(args: &Value, cfg: &MusicNcmApiConfig) -> Result<Value> {
    let keywords = args["keywords"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing keywords"))?;
    let limit = args["limit"].as_u64().unwrap_or(10);

    let client = get_client(&cfg.ncm_cookie);

    let query = Query::new()
        .param("keywords", keywords)
        .param("type", "1")
        .param("limit", &limit.to_string());

    let resp = client
        .cloudsearch(&query)
        .await
        .map_err(|e| anyhow::anyhow!("NCM API search failed: {}", e))?;

    let body = &resp.body;

    let songs = body["result"]["songs"].as_array();
    match songs {
        Some(arr) => {
            let items: Vec<Value> = arr
                .iter()
                .map(|s| {
                    let name = s["name"].as_str().unwrap_or("");
                    let id = s["id"]
                        .as_number()
                        .map(|n| n.to_string())
                        .unwrap_or_default();
                    let artists = s["ar"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x["name"].as_str())
                                .collect::<Vec<_>>()
                                .join(" / ")
                        })
                        .unwrap_or_default();
                    let duration_ms = s["dt"].as_i64().unwrap_or(0);
                    let duration_sec = duration_ms / 1000;
                    serde_json::json!({
                        "id": id,
                        "name": name,
                        "artist": artists,
                        "duration": format!("{}:{:02}", duration_sec / 60, duration_sec % 60)
                    })
                })
                .collect();
            Ok(serde_json::json!({ "songs": items }))
        }
        None => Ok(serde_json::json!({ "songs": [], "raw": body })),
    }
}

// ── play（不加入队列，直接播放）──────────────────────────────

async fn play(args: &Value, cfg: &MusicNcmApiConfig) -> Result<Value> {
    let song_id = args["song_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing song_id"))?;

    let (song_name, artist_name, url) = fetch_song(song_id, cfg).await?;

    let title = format_title(&song_name, &artist_name);
    let mut result = serde_json::json!({
        "status": "playing",
        "song_id": song_id,
        "title": title,
    });
    result[PLAY_URL_KEY] = serde_json::Value::String(url);
    result[PLAY_TITLE_KEY] = serde_json::Value::String(title);
    Ok(result)
}

// ── queue_netease ─────────────────────────────────────────────

async fn queue_netease(args: &Value, cfg: &MusicNcmApiConfig) -> Result<Value> {
    let song_id = args["song_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing song_id"))?;
    let title = args["title"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing title"))?;
    let artist = args["artist"].as_str().unwrap_or("");
    let play_now = args["play_now"].as_bool().unwrap_or(false);

    let song_info = SongInfo {
        id: song_id.to_string(),
        name: title.to_string(),
        artist: artist.to_string(),
    };

    // 添加到队列并获取插入位置
    let insert_index = {
        let mut state = player_state().lock().unwrap();
        let idx = state.queue.len();
        state.queue.push(song_info);
        if play_now {
            state.current_index = idx;
        }
        idx
    };

    info!(
        "Queue: added song {} ({}) at index {}",
        title, song_id, insert_index
    );

    if play_now {
        info!("play_now=true, playing song immediately");
        let (_, _, url) = fetch_song(song_id, cfg).await?;
        let display_title = format_title(title, artist);
        let mut result = serde_json::json!({
            "status": "playing",
            "song_id": song_id,
            "title": display_title,
        });
        result[PLAY_URL_KEY] = serde_json::Value::String(url);
        result[PLAY_TITLE_KEY] = serde_json::Value::String(display_title);
        return Ok(result);
    }

    Ok(serde_json::json!({
        "status": "queued",
        "song_id": song_id,
        "title": title,
        "queue_position": insert_index,
    }))
}

// ── next ──────────────────────────────────────────────────────

async fn next(cfg: &MusicNcmApiConfig) -> Result<Value> {
    let song = {
        let mut state = player_state().lock().unwrap();

        if state.queue.is_empty() {
            return Ok(serde_json::json!({
                "message": "队列中无下一首歌曲"
            }));
        }

        let len = state.queue.len();

        if state.mode == PlayMode::Sequential && !state.mode.is_loop() && !state.mode.is_random() {
            // 模式 0：顺序播放，到末尾停止
            let next_index = state.current_index + 1;
            if next_index >= len {
                return Ok(serde_json::json!({
                    "message": "已是最后一首歌曲"
                }));
            }
            state.current_index = next_index;
        } else if state.mode.is_random() {
            // 模式 2/3：随机
            if len > 1 {
                let mut rng = rand::rng();
                let mut new_index = rng.random_range(0..len);
                if new_index == state.current_index {
                    new_index = (new_index + 1) % len;
                }
                state.current_index = new_index;
            } else if !state.mode.is_loop() {
                // 随机非循环，只有一首歌时停止
                return Ok(serde_json::json!({
                    "message": "队列中只有一首歌曲，无法随机下一首"
                }));
            }
        } else {
            // 模式 1：顺序循环
            let next_index = state.current_index + 1;
            state.current_index = if next_index >= len { 0 } else { next_index };
        }

        state.queue[state.current_index].clone()
    };

    let (_, _, url) = fetch_song(&song.id, cfg).await?;
    let title = format_title(&song.name, &song.artist);
    let mut result = serde_json::json!({
        "status": "playing",
        "song_id": song.id,
        "title": title,
    });
    result[PLAY_URL_KEY] = serde_json::Value::String(url);
    result[PLAY_TITLE_KEY] = serde_json::Value::String(title);
    Ok(result)
}

// ── previous ──────────────────────────────────────────────────

async fn previous(cfg: &MusicNcmApiConfig) -> Result<Value> {
    let song = {
        let mut state = player_state().lock().unwrap();

        if state.queue.is_empty() {
            return Ok(serde_json::json!({
                "message": "队列中无上一首歌曲"
            }));
        }

        let len = state.queue.len();

        if state.mode == PlayMode::Sequential && !state.mode.is_loop() && !state.mode.is_random() {
            // 模式 0：顺序播放，到开头停止
            if state.current_index == 0 {
                return Ok(serde_json::json!({
                    "message": "已是第一首歌曲"
                }));
            }
            state.current_index -= 1;
        } else if state.mode.is_random() {
            // 模式 2/3：随机
            if len > 1 {
                let mut rng = rand::rng();
                let mut new_index = rng.random_range(0..len);
                if new_index == state.current_index {
                    new_index = (new_index + len - 1) % len;
                }
                state.current_index = new_index;
            } else if !state.mode.is_loop() {
                return Ok(serde_json::json!({
                    "message": "队列中只有一首歌曲，无法随机上一首"
                }));
            }
        } else {
            // 模式 1：顺序循环
            if state.current_index == 0 {
                state.current_index = len - 1;
            } else {
                state.current_index -= 1;
            }
        }

        state.queue[state.current_index].clone()
    };

    let (_, _, url) = fetch_song(&song.id, cfg).await?;
    let title = format_title(&song.name, &song.artist);
    let mut result = serde_json::json!({
        "status": "playing",
        "song_id": song.id,
        "title": title,
    });
    result[PLAY_URL_KEY] = serde_json::Value::String(url);
    result[PLAY_TITLE_KEY] = serde_json::Value::String(title);
    Ok(result)
}

// ── repeat（对齐 ts3audiobot 的 mode）────────────────────────

fn set_mode(args: &Value) -> Result<Value> {
    // 支持两种调用方式：
    // 1. repeat_mode: "none"/"one"/"all" → 映射到 mode 0/1/1
    // 2. mode: 0/1/2/3 → 直接使用
    let mode = if let Some(mode_num) = args["mode"].as_u64() {
        match mode_num {
            0..=3 => mode_num as u8,
            _ => return Err(anyhow::anyhow!("Invalid mode: '{}'. Use 0/1/2/3", mode_num)),
        }
    } else if let Some(mode_str) = args["repeat_mode"].as_str() {
        match mode_str {
            "none" => 0,
            "one" => 1,
            "all" => 1, // all 映射到顺序循环
            _ => return Err(anyhow::anyhow!("Invalid repeat_mode: '{}'. Use none/one/all", mode_str)),
        }
    } else {
        return Err(anyhow::anyhow!("Missing 'mode' or 'repeat_mode' parameter"));
    };

    let play_mode = PlayMode::from_u8(mode)
        .ok_or_else(|| anyhow::anyhow!("Invalid mode: '{}'. Use 0/1/2/3", mode))?;

    let mut state = player_state().lock().unwrap();
    state.mode = play_mode;
    info!("Play mode set to: {}", mode);

    let mode_name = match mode {
        0 => "顺序播放",
        1 => "顺序循环",
        2 => "随机播放",
        3 => "随机循环",
        _ => unreachable!(),
    };

    Ok(serde_json::json!({
        "status": "ok",
        "mode": mode,
        "mode_name": mode_name,
    }))
}

fn set_mode_from_shuffle(args: &Value) -> Result<Value> {
    let enabled = args["shuffle_enabled"]
        .as_bool()
        .ok_or_else(|| anyhow::anyhow!("Missing shuffle_enabled"))?;

    let mut state = player_state().lock().unwrap();
    // shuffle=true → 随机循环(3)，shuffle=false → 顺序播放(0)
    state.mode = if enabled {
        PlayMode::RandomLoop
    } else {
        PlayMode::Sequential
    };
    info!("Shuffle set to: {}, mode now: {:?}", enabled, state.mode);

    Ok(serde_json::json!({
        "status": "ok",
        "shuffle_enabled": enabled,
        "mode": state.mode.clone() as u8,
    }))
}

// ── 辅助函数 ──────────────────────────────────────────────────

/// 获取歌曲详情和播放 URL
async fn fetch_song(song_id: &str, cfg: &MusicNcmApiConfig) -> Result<(String, String, String)> {
    let client = get_client(&cfg.ncm_cookie);

    // 1. 获取歌曲详情（标题、歌手）
    let detail_query = Query::new().param("ids", song_id);
    let detail_resp = client
        .song_detail(&detail_query)
        .await
        .map_err(|e| anyhow::anyhow!("NCM song detail failed: {}", e))?;

    let detail_body = &detail_resp.body;

    let song_name = detail_body["songs"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|s| s["name"].as_str())
        .unwrap_or("")
        .to_string();
    let artist_name = detail_body["songs"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|s| s["ar"].as_array())
        .map(|ar| {
            ar.iter()
                .filter_map(|x| x["name"].as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        })
        .unwrap_or_default();

    // 2. 获取播放 URL
    let url_query = Query::new()
        .param("id", song_id)
        .param("level", "exhigh");
    let url_resp = client
        .song_url_v1(&url_query)
        .await
        .map_err(|e| anyhow::anyhow!("NCM song URL failed: {}", e))?;

    let url_body = &url_resp.body;

    let audio_url = url_body["data"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|d| d["url"].as_str())
        .filter(|u| !u.is_empty());

    match audio_url {
        Some(url) => Ok((song_name, artist_name, url.to_string())),
        None => {
            let code = url_body["data"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|d| d["code"].as_i64())
                .unwrap_or(0);

            if cfg.unm_enabled {
                warn!(
                    "Song {} unavailable from NCM (code={}), trying UNM fallback",
                    song_id, code
                );
                match super::unm::search_and_retrieve(song_id, &song_name, &artist_name, cfg).await
                {
                    Ok(unm_url) => {
                        info!("UNM fallback succeeded for song {}", song_id);
                        Ok((song_name, artist_name, unm_url))
                    }
                    Err(e) => {
                        warn!("UNM fallback failed for song {}: {}", song_id, e);
                        Err(anyhow::anyhow!(
                            "Song {} unavailable (code={}). UNM fallback also failed: {}",
                            song_id,
                            code,
                            e
                        ))
                    }
                }
            } else {
                Err(anyhow::anyhow!(
                    "Song {} unavailable (code={}). May require VIP or be region-locked.",
                    song_id,
                    code
                ))
            }
        }
    }
}

/// 格式化标题
fn format_title(name: &str, artist: &str) -> String {
    if artist.is_empty() {
        name.to_string()
    } else {
        format!("{} - {}", name, artist)
    }
}
