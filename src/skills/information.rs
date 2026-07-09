use crate::skills::{ExecutionContext, Platform, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::{debug, info};

pub struct GetClientInfo;

#[async_trait]
impl Skill for GetClientInfo {
    fn name(&self) -> &'static str {
        "get_client_info"
    }
    fn description(&self) -> &'static str {
        "Get detailed information about an online user by their clid, including connection time, IP address, version, and more."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID of the online user to query." }
            },
            "required": ["clid"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: clid"))?
            as u32;

        let mut info = ctx.adapter.get_client_info(clid).await?;

        debug!(?info, clid, "GetClientInfo raw response");

        if let Some(ts) = info.get("connection_connected_time") {
            if let Ok(ms) = ts.parse::<u64>() {
                let total_secs = ms / 1000;

                let years = total_secs / (365 * 86400);
                let rem = total_secs % (365 * 86400);
                let months = rem / (30 * 86400);
                let rem = rem % (30 * 86400);
                let days = rem / 86400;
                let rem = rem % 86400;
                let hours = rem / 3600;
                let rem = rem % 3600;
                let minutes = rem / 60;
                let seconds = rem % 60;

                let dur_str = format!(
                    "{}{}{}{}{}{}",
                    if years > 0 {
                        format!("{years} years ")
                    } else {
                        String::new()
                    },
                    if months > 0 {
                        format!("{months} months ")
                    } else {
                        String::new()
                    },
                    if days > 0 {
                        format!("{days} days ")
                    } else {
                        String::new()
                    },
                    if hours > 0 {
                        format!("{hours} hours ")
                    } else {
                        String::new()
                    },
                    if minutes > 0 {
                        format!("{minutes} minutes ")
                    } else {
                        String::new()
                    },
                    if seconds > 0
                        || (years == 0 && months == 0 && days == 0 && hours == 0 && minutes == 0)
                    {
                        format!("{seconds} seconds")
                    } else {
                        String::new()
                    },
                );
                let dur_str = dur_str.trim().to_string();

                info.insert("connection_duration".to_string(), dur_str);
                info.remove("connection_connected_time");
            }
        }

        Ok(json!({"status": "ok", "client_info": info}))
    }

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!(
            "GetClientInfo: unified execution, platform={:?}",
            ctx.platform
        );

        match ctx.platform {
            Platform::TeamSpeak => {
                let ts_ctx = ctx.to_ts_ctx()?;
                return self.execute(args.clone(), &ts_ctx).await;
            }
            Platform::NapCat => {
                let clid = args["clid"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("Missing required parameter: clid"))?
                    as u32;

                let ts_adapter = ctx
                    .ts_adapter
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("TeamSpeak adapter not available"))?;

                let clients = ts_adapter.list_clients().await?;
                let client = clients
                    .iter()
                    .find(|c| c.id as u32 == clid)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Client {} is not online or does not exist", clid)
                    })?;
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
