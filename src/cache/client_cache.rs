use crate::adapter::TsAdapter;
use crate::config::AppConfig;
use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub clid: u32,
    pub cldbid: u32,
    pub nickname: String,
    pub server_groups: Vec<u32>,
#[allow(dead_code)]
    pub last_seen: Instant,
}

pub struct ClientCache {
    config: Arc<ArcSwap<AppConfig>>,
    pub clients: DashMap<u32, ClientInfo>,
}

impl ClientCache {
    pub fn new(config: Arc<ArcSwap<AppConfig>>) -> Self {
        Self {
            config,
            clients: DashMap::new(),
        }
    }

    pub fn get_client(&self, clid: u32) -> Option<ClientInfo> {
        self.clients.get(&clid).map(|r| r.clone())
    }

    pub async fn run_refresh_loop(&self, adapter: Arc<TsAdapter>) {
        loop {
            let interval = self.config.load().cache.refresh_interval_secs;
            if interval == 0 {
                sleep(Duration::from_secs(60)).await;
                continue;
            }
            sleep(Duration::from_secs(interval)).await;

            // Refresh client list
            if let Err(e) = adapter.send_raw("clientlist -uid -groups").await {
                tracing::error!("Failed to refresh client cache: {e}");
            }
        }
    }

    pub fn update_client(&self, clid: u32, cldbid: u32, nickname: String, server_groups: Vec<u32>) {
        self.clients.insert(
            clid,
            ClientInfo {
                clid,
                cldbid,
                nickname,
                server_groups,
                last_seen: Instant::now(),
            },
        );
    }

    pub fn remove_client(&self, clid: u32) {
        self.clients.remove(&clid);
    }

    pub fn list_clients(&self) -> Vec<ClientInfo> {
        self.clients
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }
}
