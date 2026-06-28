use crate::skills::{ExecutionContext, Platform, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tracing::info;

pub struct GetClientInfo;

#[async_trait]
impl Skill for GetClientInfo {
    fn name(&self) -> &'static str {
        "get_client_info"
    }
    fn description(&self) -> &'static str {
        "Get detailed information about an online user by their nickname, including connection time, IP address, version, and more."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "nickname": { "type": "string", "description": "The nickname of the online user to query." }
            },
            "required": ["nickname"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let nickname = args["nickname"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: nickname"))?;

        let clients = ctx.adapter.list_clients().await?;
        let client = clients
            .iter()
            .find(|c| c.nickname == nickname)
            .ok_or_else(|| anyhow::anyhow!("Client '{}' is not online or does not exist", nickname))?;
        let clid = client.id as u32;

        let mut info = ctx.adapter.get_client_info(clid).await?;

        if let Some(ts) = info.get("client_connection_connected_time") {
            if let Ok(timestamp) = ts.parse::<i64>() {
                if let Some(connected) = DateTime::from_timestamp(timestamp, 0) {
                    let now = Utc::now();
                    let secs = (now - connected).num_seconds().max(0);

                    let h = secs / 3600;
                    let m = (secs % 3600) / 60;
                    let s = secs % 60;

                    let dur_str = if h > 0 {
                        format!("{h} hours {m} minutes {s} seconds")
                    } else if m > 0 {
                        format!("{m} minutes {s} seconds")
                    } else {
                        format!("{s} seconds")
                    };
                    info.insert("connection_duration".to_string(), dur_str);
                    info.remove("client_connection_connected_time");
                }
            }
        }

        Ok(json!({"status": "ok", "client_info": info}))
    }

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!(
            "GetClientInfo: unified execution, platform={:?}",
            ctx.platform
        );

        let nickname = args["nickname"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: nickname"))?;

        match ctx.platform {
            Platform::TeamSpeak => {
                let ts_ctx = ctx.to_ts_ctx()?;
                return self.execute(args.clone(), &ts_ctx).await;
            }
            Platform::NapCat => {
                let ts_adapter = ctx
                    .ts_adapter
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("TeamSpeak adapter not available"))?;

                let clients = ts_adapter.list_clients().await?;
                let client = clients
                    .iter()
                    .find(|c| c.nickname == nickname)
                    .ok_or_else(|| anyhow::anyhow!("Client '{}' is not online or does not exist", nickname))?;
                let groups: Vec<u32> = client
                    .server_groups
                    .iter()
                    .filter_map(|g| g.parse().ok())
                    .collect();
                let reply = format!(
                    "TS user info - nickname:{}, ID:{}, server groups:{:?}, channel ID:{}",
                    client.nickname, client.id, groups, client.channel_id
                );

                Ok(json!({
                    "status": "ok",
                    "message": reply,
                    "platform": "teamspeak"
                }))
            }
        }
    }
}
