use crate::skills::{ExecutionContext, Platform, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

fn ts_client_to_json(clients: &[tsclient_rs::ClientInfo]) -> Vec<Value> {
    clients
        .iter()
        .map(|c| {
            let groups: Vec<u32> = c.server_groups.iter().filter_map(|g| g.parse().ok()).collect();
            json!({
                "clid": c.id,
                "nickname": c.nickname,
                "dbid": 0,
                "groups": groups,
                "channel_id": c.channel_id
            })
        })
        .collect()
}

pub struct GetClientList;

#[async_trait]
impl Skill for GetClientList {
    fn name(&self) -> &'static str {
        "get_client_list"
    }
    fn description(&self) -> &'static str {
        "Get the list of online clients."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }
    async fn execute(&self, _args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clients = ctx.adapter.list_clients().await?;
        let json_clients = ts_client_to_json(&clients);
        Ok(json!({"status": "ok", "clients": json_clients}))
    }

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!(
            "GetClientList: unified execution, platform={:?}",
            ctx.platform
        );

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
                let json_clients = ts_client_to_json(&clients);

                let reply = if json_clients.is_empty() {
                    "No online users on TS server".to_string()
                } else {
                    let names: Vec<_> = json_clients
                        .iter()
                        .map(|c| c["nickname"].as_str().unwrap_or("unknown"))
                        .collect();
                    format!(
                        "TS server online users ({}): {}",
                        names.len(),
                        names.join(", ")
                    )
                };

                Ok(json!({
                    "status": "ok",
                    "message": reply,
                    "clients": json_clients,
                    "platform": "teamspeak"
                }))
            }
        }
    }
}

pub struct GetClientInfo;

#[async_trait]
impl Skill for GetClientInfo {
    fn name(&self) -> &'static str {
        "get_client_info"
    }
    fn description(&self) -> &'static str {
        "Get detailed information about a specific online client by their client ID, including connection time, IP address, version, and more."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID to query." }
            },
            "required": ["clid"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing required parameter: clid"))?
            as u32;

        let info = ctx.adapter.get_client_info(clid).await?;
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
                let ts_adapter = ctx
                    .ts_adapter
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("TeamSpeak adapter not available"))?;
                let clid = args["clid"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("Missing required parameter: clid"))?
                    as u32;

                let clients = ts_adapter.list_clients().await?;
                let client = clients
                    .iter()
                    .find(|c| c.id as u32 == clid)
                    .ok_or_else(|| anyhow::anyhow!("Client {} is not online or does not exist", clid))?;
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
