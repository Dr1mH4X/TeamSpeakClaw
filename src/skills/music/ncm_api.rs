use crate::config::MusicNcmApiConfig;
use crate::skills::music::{PLAY_TITLE_KEY, PLAY_URL_KEY};
use anyhow::Result;
use ncm_api_rs::{create_client, Query};
use serde_json::Value;
use std::sync::OnceLock;
use tracing::{info, warn};

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

pub(crate) async fn execute(action: &str, args: &Value, cfg: &MusicNcmApiConfig) -> Result<Value> {
    match action {
        "search" => search(args, cfg).await,
        "play" => play(args, cfg).await,
        "pause" | "stop" => Ok(serde_json::json!({
            "message": "ncm_api backend does not support pause/stop. Use the bot's playback controls."
        })),
        "next" | "previous" | "skip" => Ok(serde_json::json!({
            "message": format!("ncm_api backend does not support '{}'. Queue songs individually.", action)
        })),
        _ => Err(anyhow::anyhow!(
            "Action '{}' is not supported by the ncm_api backend.",
            action
        )),
    }
}

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

async fn play(args: &Value, cfg: &MusicNcmApiConfig) -> Result<Value> {
    let song_id = args["song_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing song_id"))?;

    let client = get_client(&cfg.ncm_cookie);

    // 1. Get song details (title, artist)
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

    // 2. Get song URL
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
        Some(url) => {
            let title = if artist_name.is_empty() {
                song_name
            } else {
                format!("{} - {}", song_name, artist_name)
            };
            let mut result = serde_json::json!({
                "status": "playing",
                "song_id": song_id,
                "title": title,
            });
            result[PLAY_URL_KEY] = serde_json::Value::String(url.to_string());
            result[PLAY_TITLE_KEY] = serde_json::Value::String(title);
            Ok(result)
        }
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
                        let title = if artist_name.is_empty() {
                            song_name
                        } else {
                            format!("{} - {}", song_name, artist_name)
                        };
                        let mut result = serde_json::json!({
                            "status": "playing",
                            "song_id": song_id,
                            "title": title,
                            "source": "unm",
                        });
                        result[PLAY_URL_KEY] = serde_json::Value::String(unm_url);
                        result[PLAY_TITLE_KEY] = serde_json::Value::String(title);
                        Ok(result)
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
