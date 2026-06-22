use crate::skills::{ExecutionContext, Platform, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

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
        let clients: Vec<_> = ctx.clients.iter().map(|r| r.value().clone()).collect();
        let json_clients: Vec<_> = clients
            .iter()
            .map(|c| {
                json!({
                    "clid": c.clid,
                    "nickname": c.nickname,
                    "dbid": c.cldbid,
                    "groups": c.server_groups
                })
            })
            .collect();

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
                // NC 请求查询 TS 在线列表
                if let Some(ref ts_clients) = ctx.ts_clients {
                    let clients: Vec<_> = ts_clients.iter().map(|r| r.value().clone()).collect();
                    let json_clients: Vec<_> = clients
                        .iter()
                        .map(|c| {
                            json!({
                                "clid": c.clid,
                                "nickname": c.nickname,
                                "dbid": c.cldbid,
                                "groups": c.server_groups
                            })
                        })
                        .collect();

                    let reply = if json_clients.is_empty() {
                        "TS服务器当前没有在线用户".to_string()
                    } else {
                        let names: Vec<_> = json_clients
                            .iter()
                            .map(|c| c["nickname"].as_str().unwrap_or("unknown"))
                            .collect();
                        format!("TS服务器在线用户 ({})：{}", names.len(), names.join(", "))
                    };

                    return Ok(json!({
                        "status": "ok",
                        "message": reply,
                        "clients": json_clients,
                        "platform": "teamspeak"
                    }));
                }
                Err(anyhow::anyhow!("TeamSpeak clients list not available"))
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
        "Get detailed information about a specific online client by their client ID."
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
            .ok_or_else(|| anyhow::anyhow!("缺少必要参数: clid"))? as u32;

        // 确认目标客户端在线
        if !ctx.clients.contains_key(&clid) {
            let msg = format!("客户端 {} 不在线或不存在", clid);
            return Ok(json!({"status": "error", "message": msg}));
        }

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
                // NC 请求查询 TS 指定用户信息
                let clid = args["clid"]
                    .as_u64()
                    .ok_or_else(|| anyhow::anyhow!("缺少必要参数: clid"))?
                    as u32;

                if let Some(ref ts_clients) = ctx.ts_clients {
                    let Some(client) = ts_clients.get(&clid) else {
                        return Ok(json!({
                            "status": "error",
                            "message": format!("客户端 {} 不在线或不存在", clid)
                        }));
                    };
                    let reply = format!(
                        "TS用户信息 - 昵称:{}, ID:{}, 数据库ID:{}, 服务器分组:{:?}",
                        client.nickname, client.clid, client.cldbid, client.server_groups
                    );

                    return Ok(json!({
                        "status": "ok",
                        "message": reply,
                        "platform": "teamspeak"
                    }));
                }
                Err(anyhow::anyhow!("TeamSpeak clients list not available"))
            }
        }
    }
}



