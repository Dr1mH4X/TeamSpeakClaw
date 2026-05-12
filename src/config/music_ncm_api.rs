use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct MusicNcmApiConfig {
    pub ncm_cookie: String,
    pub unm_enabled: bool,
    pub unm_sources: String,
    pub unm_enable_flac: bool,
    pub unm_search_mode: String,
    pub unm_proxy_uri: String,
    pub unm_joox_cookie: String,
    pub unm_qq_cookie: String,
}

impl Default for MusicNcmApiConfig {
    fn default() -> Self {
        Self {
            ncm_cookie: String::new(),
            unm_enabled: true,
            unm_sources: "ytdlp,bilibili,kugou".to_string(),
            unm_enable_flac: false,
            unm_search_mode: "fast-first".to_string(),
            unm_proxy_uri: String::new(),
            unm_joox_cookie: String::new(),
            unm_qq_cookie: String::new(),
        }
    }
}
