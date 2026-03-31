use crate::adapter::command::{cmd_ban, cmd_channellist, cmd_kick, cmd_move};
use crate::skills::{ExecutionContext, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tracing::info;

/// 检查是否可以对目标执行操作
/// 返回目标的组信息（如果存在）和权限检查结果
fn validate_target(ctx: &ExecutionContext, clid: u32) -> Result<Vec<u32>> {
    // 自操作防护
    if clid == ctx.caller_id {
        return Err(anyhow::anyhow!(ctx.error_prompts.self_target.clone()));
    }

    // 获取目标的组信息
    let target_groups = ctx
        .clients
        .get(&clid)
        .map(|c| c.server_groups.clone())
        .unwrap_or_default();

    // 检查是否可以对目标执行操作
    if !ctx.gate.can_target(
        &ctx.caller_groups,
        ctx.caller_channel_group_id,
        &target_groups,
    ) {
        return Err(anyhow::anyhow!(ctx.error_prompts.target_permission.clone()));
    }

    Ok(target_groups)
}

async fn validate_channel_exists(ctx: &ExecutionContext<'_>, channel_id: u32) -> Result<()> {
    if channel_id == 0 {
        return Err(anyhow::anyhow!("目标频道 ID 必须大于 0"));
    }

    let response = ctx.adapter.send_query(&cmd_channellist()).await?;
    let channel_exists = response
        .lines()
        .filter(|line| !line.starts_with("error id="))
        .flat_map(|line| line.split('|'))
        .any(|entry| {
            entry
                .split_whitespace()
                .filter_map(|token| token.strip_prefix("cid="))
                .filter_map(|cid| cid.parse::<u32>().ok())
                .any(|cid| cid == channel_id)
        });

    if !channel_exists {
        return Err(anyhow::anyhow!("目标频道不存在: {}", channel_id));
    }

    Ok(())
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

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!("KickClient: unified execution, platform={:?}", ctx.platform);
        let ts_ctx = ctx.to_ts_ctx()?;
        self.execute(args, &ts_ctx).await
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

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!("BanClient: unified execution, platform={:?}", ctx.platform);
        let ts_ctx = ctx.to_ts_ctx()?;
        self.execute(args, &ts_ctx).await
    }
}

pub struct MoveClient;

#[async_trait]
impl Skill for MoveClient {
    fn name(&self) -> &'static str {
        "move_client"
    }

    fn description(&self) -> &'static str {
        "Move a client to another channel."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "clid": { "type": "integer", "description": "The client ID to move." },
                "channel_id": { "type": "integer", "description": "The target channel ID." }
            },
            "required": ["clid", "channel_id"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ExecutionContext) -> Result<Value> {
        let clid = args["clid"].as_u64().ok_or_else(|| {
            anyhow::anyhow!(ctx
                .error_prompts
                .missing_parameter
                .replace("{param}", "clid"))
        })? as u32;
        let channel_id = args["channel_id"].as_u64().ok_or_else(|| {
            anyhow::anyhow!(ctx
                .error_prompts
                .missing_parameter
                .replace("{param}", "channel_id"))
        })? as u32;

        validate_target(ctx, clid)?;

        if !ctx.clients.contains_key(&clid) {
            return Err(anyhow::anyhow!(ctx
                .error_prompts
                .client_offline
                .replace("{clid}", &clid.to_string())));
        }

        validate_channel_exists(ctx, channel_id).await?;

        ctx.adapter.send_raw(&cmd_move(clid, channel_id)).await?;
        Ok(json!({
            "status": "ok",
            "message": format!("Client {} moved to channel {}", clid, channel_id)
        }))
    }

    async fn execute_unified(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        info!("MoveClient: unified execution, platform={:?}", ctx.platform);
        let ts_ctx = ctx.to_ts_ctx()?;
        self.execute(args, &ts_ctx).await
    }
}
