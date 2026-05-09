use serde::Serialize;
use serde_json::Value;

/// Unified response from music backends.
///
/// When `play_url` is `Some`, the bridge should trigger playback via gRPC Play
/// and strip `play_url`/`play_title` before passing the result to the LLM.
#[derive(Debug, Serialize)]
pub struct MusicBackendResponse {
    /// Structured data for the LLM.
    pub data: Value,
    /// Audio URL for the bridge to play (embedded backends like ncm_api).
    pub play_url: Option<String>,
    /// Song title for the playback UI.
    pub play_title: Option<String>,
}

impl MusicBackendResponse {
    /// Simple response without playback (for external backends).
    pub fn data_only(data: Value) -> Self {
        Self {
            data,
            play_url: None,
            play_title: None,
        }
    }

    /// Response with a playback URL (for embedded backends).
    pub fn with_play_url(data: Value, url: String, title: String) -> Self {
        Self {
            data,
            play_url: Some(url),
            play_title: Some(title),
        }
    }

    /// Serialize to JSON string for the tool result.
    ///
    /// If `play_url` is present, it is included as `__play_url` / `__play_title`
    /// in the JSON so the bridge can intercept it.
    pub fn to_json_string(&self) -> String {
        match &self.play_url {
            Some(url) => {
                let mut map = match &self.data {
                    Value::Object(m) => m.clone(),
                    other => {
                        let mut m = serde_json::Map::new();
                        m.insert("result".to_string(), other.clone());
                        m
                    }
                };
                map.insert("__play_url".to_string(), Value::String(url.clone()));
                if let Some(title) = &self.play_title {
                    map.insert("__play_title".to_string(), Value::String(title.clone()));
                }
                serde_json::to_string(&Value::Object(map)).unwrap_or_default()
            }
            None => serde_json::to_string(&self.data).unwrap_or_default(),
        }
    }
}
