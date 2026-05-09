use crate::config::MusicBackendConfig;
use anyhow::Result;
use serde_json::Value;

pub(crate) async fn execute(action: &str, args: &Value, cfg: &MusicBackendConfig) -> Result<Value> {
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

async fn search(args: &Value, cfg: &MusicBackendConfig) -> Result<Value> {
    let keywords = args["keywords"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing keywords"))?;
    let limit = args["limit"].as_u64().unwrap_or(10);

    let url = format!("{}/cloudsearch", cfg.ncm_api_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let mut req = client
        .get(&url)
        .query(&[("keywords", keywords), ("type", "1")])
        .query(&[("limit", limit.to_string().as_str())]);

    if !cfg.ncm_cookie.is_empty() {
        req = req.header("Cookie", &cfg.ncm_cookie);
    }

    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;

    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "NCM API search failed ({}): {}",
            status,
            text
        ));
    }

    let body: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("Failed to parse NCM API response: {e}"))?;

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

async fn play(args: &Value, cfg: &MusicBackendConfig) -> Result<Value> {
    let song_id = args["song_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing song_id"))?;

    let client = reqwest::Client::new();
    let base = cfg.ncm_api_url.trim_end_matches('/');

    // 1. Get song details (title, artist)
    let detail_url = format!("{}/song/detail", base);
    let mut detail_req = client.get(&detail_url).query(&[("ids", song_id)]);
    if !cfg.ncm_cookie.is_empty() {
        detail_req = detail_req.header("Cookie", &cfg.ncm_cookie);
    }
    let detail_resp = detail_req.send().await?;
    let detail_body: Value = detail_resp.json().await?;

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
    let url_endpoint = format!("{}/song/url/v1", base);
    let mut url_req = client
        .get(&url_endpoint)
        .query(&[("id", song_id), ("level", "exhigh")]);
    if !cfg.ncm_cookie.is_empty() {
        url_req = url_req.header("Cookie", &cfg.ncm_cookie);
    }
    let url_resp = url_req.send().await?;
    let url_body: Value = url_resp.json().await?;

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
            result["__play_url"] = serde_json::Value::String(url.to_string());
            result["__play_title"] = serde_json::Value::String(title);
            Ok(result)
        }
        None => {
            let code = url_body["data"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|d| d["code"].as_i64())
                .unwrap_or(0);
            Err(anyhow::anyhow!(
                "Song {} unavailable (code={}). May require VIP or be region-locked.",
                song_id,
                code
            ))
        }
    }
}
