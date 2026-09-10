use crate::skills::{required_u32, unified_ts_adapter, Platform, Skill, UnifiedExecutionContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

/// TS-only 管理操作：NapCat 调用者没有 TS clid，自操作保护与目标语义均不成立
fn require_ts_platform(ctx: &UnifiedExecutionContext, skill: &str) -> Result<()> {
    if ctx.platform == Platform::NapCat {
        return Err(anyhow::anyhow!(
            "Skill '{}' does not support the NapCat platform",
            skill
        ));
    }
    Ok(())
}

/// 检查是否可以对目标执行操作
async fn validate_target(ctx: &UnifiedExecutionContext, clid: u32) -> Result<()> {
    if clid == ctx.caller_id {
        return Err(anyhow::anyhow!("Cannot perform this action on yourself"));
    }

    let ts_adapter = unified_ts_adapter(ctx)?;
    let clients = ts_adapter.list_clients().await?;
    let target = clients
        .iter()
        .find(|c| u32::try_from(c.id).ok() == Some(clid))
        .ok_or_else(|| anyhow::anyhow!("Client {} is not online or does not exist", clid))?;
    let target_groups = crate::adapter::headless::parse_server_groups(&target.server_groups);

    if !ctx.gate.can_target(
        &ctx.caller_groups,
        ctx.caller_channel_group_id,
        &target_groups,
    ) {
        return Err(anyhow::anyhow!(
            "No permission to perform this action on that user"
        ));
    }

    Ok(())
}

async fn validate_channel_exists(ctx: &UnifiedExecutionContext, channel_id: u32) -> Result<()> {
    if channel_id == 0 {
        return Err(anyhow::anyhow!("Target channel ID must be greater than 0"));
    }

    let ts_adapter = unified_ts_adapter(ctx)?;
    let channels = ts_adapter.list_channels().await?;
    let exists = channels.iter().any(|c| c.id == channel_id as u64);
    if !exists {
        return Err(anyhow::anyhow!(
            "Target channel does not exist: {}",
            channel_id
        ));
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
    async fn execute(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        require_ts_platform(ctx, "kick_client")?;
        let clid = required_u32(&args, "clid")?;
        let reason = args["reason"].as_str().unwrap_or("Kicked by bot");

        validate_target(ctx, clid).await?;

        unified_ts_adapter(ctx)?.kick(clid, reason).await?;
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
    async fn execute(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        require_ts_platform(ctx, "ban_client")?;
        let clid = required_u32(&args, "clid")?;
        let time = args["time"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing time"))?;
        let reason = args["reason"].as_str().unwrap_or("Banned by bot");

        validate_target(ctx, clid).await?;

        unified_ts_adapter(ctx)?.ban(clid, time, reason).await?;
        Ok(json!({"status": "ok", "message": "Client banned"}))
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

    async fn execute(&self, args: Value, ctx: &UnifiedExecutionContext) -> Result<Value> {
        require_ts_platform(ctx, "move_client")?;
        let clid = required_u32(&args, "clid")?;
        let channel_id = required_u32(&args, "channel_id")?;

        validate_target(ctx, clid).await?;
        validate_channel_exists(ctx, channel_id).await?;

        unified_ts_adapter(ctx)?
            .move_client(clid, channel_id)
            .await?;
        Ok(json!({
            "status": "ok",
            "message": format!("Client {} moved to channel {}", clid, channel_id)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AclConfig, AppConfig};
    use crate::permission::PermissionGate;
    use std::sync::Arc;

    fn nc_ctx() -> UnifiedExecutionContext {
        UnifiedExecutionContext {
            platform: Platform::NapCat,
            ts_adapter: None,
            nc_adapter: None,
            caller_id: 0,
            caller_id_nc: 42,
            caller_name: "qq-user".to_string(),
            caller_groups: vec![],
            caller_channel_group_id: 0,
            nc_group_id: None,
            gate: Arc::new(PermissionGate::new(AclConfig::default())),
            config: Arc::new(AppConfig::default()),
        }
    }

    #[tokio::test]
    async fn moderation_skills_reject_napcat_callers() {
        let ctx = nc_ctx();
        let err = KickClient
            .execute(json!({"clid": 1}), &ctx)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("does not support the NapCat platform"));

        let err = BanClient
            .execute(json!({"clid": 1, "time": 0}), &ctx)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("does not support the NapCat platform"));

        let err = MoveClient
            .execute(json!({"clid": 1, "channel_id": 2}), &ctx)
            .await
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("does not support the NapCat platform"));
    }
}
