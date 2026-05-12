use anyhow::Result;
use std::borrow::Cow;
use std::sync::OnceLock;
use tracing::{debug, info};
use unm_api_utils::executor::build_full_executor;
use unm_engine::executor::Executor;
use unm_types::config::ConfigManagerBuilder;
use unm_types::{Artist, ContextBuilder, SearchMode, Song};

use crate::config::MusicNcmApiConfig;

static EXECUTOR: OnceLock<Executor> = OnceLock::new();

fn get_executor() -> &'static Executor {
    EXECUTOR.get_or_init(|| {
        debug!("Initializing UNM executor with all engines");
        build_full_executor()
    })
}

pub(crate) async fn search_and_retrieve(
    song_id: &str,
    song_name: &str,
    artist_name: &str,
    cfg: &MusicNcmApiConfig,
) -> Result<String> {
    let executor = get_executor();

    let sources: Vec<Cow<'static, str>> = cfg
        .unm_sources
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(Cow::Owned)
        .collect();

    if sources.is_empty() {
        return Err(anyhow::anyhow!("No UNM sources configured"));
    }

    let search_mode = match cfg.unm_search_mode.as_str() {
        "order-first" => SearchMode::OrderFirst,
        _ => SearchMode::FastFirst,
    };

    let mut ctx_builder = ContextBuilder::default();
    ctx_builder
        .enable_flac(cfg.unm_enable_flac)
        .search_mode(search_mode);

    if !cfg.unm_proxy_uri.is_empty() {
        ctx_builder.proxy_uri(Some(Cow::Owned(cfg.unm_proxy_uri.clone())));
    }

    let mut config_builder = ConfigManagerBuilder::new();
    if !cfg.unm_joox_cookie.is_empty() {
        config_builder.set("joox:cookie", cfg.unm_joox_cookie.clone());
    }
    if !cfg.unm_qq_cookie.is_empty() {
        config_builder.set("qq:cookie", cfg.unm_qq_cookie.clone());
    }
    config_builder.set("ytdl:exe", "yt-dlp");
    ctx_builder.config(Some(config_builder.build()));

    let context = ctx_builder.build()?;

    let song = Song::builder()
        .id(song_id.to_string())
        .name(song_name.to_string())
        .artists(vec![Artist::builder()
            .name(artist_name.to_string())
            .build()])
        .build();

    info!(
        "UNM searching: {} - {} with sources {:?}",
        song_name, artist_name, sources
    );

    let search_result = executor
        .search(&sources, &song, &context)
        .await
        .map_err(|e| anyhow::anyhow!("UNM search failed: {}", e))?;

    let retrieved = executor
        .retrieve(&search_result, &context)
        .await
        .map_err(|e| anyhow::anyhow!("UNM retrieve failed: {}", e))?;

    info!(
        "UNM found: {} from {}",
        song.display_name(),
        retrieved.source
    );

    Ok(retrieved.url)
}
