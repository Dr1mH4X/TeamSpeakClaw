use crate::adapter::command::{cmd_ban, cmd_kick};
use crate::skills::{ExecutionContext, Skill};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

/// 检查是否可以对目标执行操作
/// 返回目标的组信息（如果存在）和权限检查结果
fn validate_target(ctx: &ExecutionContext, clid: u32) -> Result<Vec<u32>> {
    // 自操作防护
    if clid == ctx.caller_id {
        return Err(anyhow::anyhow!("不能对自己执行此操作"));
    }

    // 获取目标的组信息
    let target_groups = ctx
        .clients
        .get(&clid)
        .map(|c| c.server_groups.clone())
        .unwrap_or_default();

    // 检查是否可以对目标执行操作
    if !ctx.gate.can_target(&ctx.caller_groups, &target_groups) {
        return Err(anyhow::anyhow!("无权对该用户执行此操作"));
    }

    Ok(target_groups)
}

pub struct KickClient;

#[async_trait]
impl Skill for KickClient {
    fn name(&self) -> &'static str {
        "kick_client"
    }
    fn description(&self) -> &'static str {
        "Kick a client from the server."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID to kick." },
                "reason": { "type": "string", "description": "Kick reason." }
            },
            "required": ["clid"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing clid"))? as u32;
        let reason = args["reason"].as_str().unwrap_or("Kicked by bot");

        // 权限和自操作检查
        validate_target(ctx, clid)?;

        ctx.adapter.send_raw(&cmd_kick(clid, reason)).await?;
        Ok(json!({"status": "ok", "message": "Client kicked"}))
    }
}

pub struct BanClient;

#[async_trait]
impl Skill for BanClient {
    fn name(&self) -> &'static str {
        "ban_client"
    }
    fn description(&self) -> &'static str {
        "Ban a client from the server."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID to ban." },
                "time": { "type": "integer", "description": "Ban duration in seconds (0 for permanent)." },
                "reason": { "type": "string", "description": "Ban reason." }
            },
            "required": ["clid", "time"]
        })
    }
    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing clid"))? as u32;
        let time = args["time"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing time"))?;
        let reason = args["reason"].as_str().unwrap_or("Banned by bot");

        // 权限和自操作检查
        validate_target(ctx, clid)?;

        ctx.adapter.send_raw(&cmd_ban(clid, time, reason)).await?;
        Ok(json!({"status": "ok", "message": "Client banned"}))
    }
}
