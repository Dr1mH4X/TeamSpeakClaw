use anyhow::Result;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::Duration;

fn shared_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default()
    })
}

pub(crate) struct HttpBackend {
    base_url: String,
    client: reqwest::Client,
}

impl HttpBackend {
    pub(crate) fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: shared_client().clone(),
        }
    }

    pub(crate) async fn post(&self, path: &str, body: Option<Value>) -> Result<Value> {
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

    pub(crate) async fn put(&self, path: &str, body: Value) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self.client.put(&url).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!("HTTP {} from {}: {}", status, path, text));
        }
        Ok(serde_json::from_str(&text).unwrap_or(json!({"raw": text})))
    }

    pub(crate) async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
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

pub(crate) async fn execute(action: &str, args: &Value, base_url: &str) -> Result<Value> {
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

        ts if ts.starts_with("ts_") => Err(anyhow::anyhow!(
            "Action '{}' is only available with the ts3audiobot backend. \
             Current backend is tsbot_backend.",
            action
        )),

        _ => Err(anyhow::anyhow!("Unknown action: {}", action)),
    }
}
