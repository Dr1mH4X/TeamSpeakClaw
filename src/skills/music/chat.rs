use crate::adapter::headless::{TextMessageTarget, TsEvent};
use crate::skills::{unified_ts_adapter, UnifiedExecutionContext};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info};

/// 聊天音乐后端串行锁：避免多个 LLM 轮次同时向 bot 发命令
static CHAT_MUSIC_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// 发私信给在线音乐 bot，并等待其在指定 target 上回复
pub(crate) async fn send_and_await_reply(
    ctx: &UnifiedExecutionContext,
    bot_cmd: &str,
    reply_target: TextMessageTarget,
    bot_label: &str,
) -> Result<Value> {
    let target_name = ctx
        .config
        .music_backend
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("MusicControl registered but music_backend is None"))?
        .musicbot_name
        .as_str();

    let ts_adapter = unified_ts_adapter(ctx)?;
    let clients = ts_adapter.list_clients().await?;
    let audiobot = clients
        .iter()
        .find(|c| ctx.config.is_music_bot_name(&c.nickname))
        .ok_or_else(|| anyhow::anyhow!("Music bot '{}' not found online", target_name))?;
    let audiobot_id =
        u32::try_from(audiobot.id).context("Music bot returned an invalid client ID")?;

    let _guard = CHAT_MUSIC_LOCK.lock().await;
    let mut ts_rx = ts_adapter.subscribe();

    ts_adapter
        .send_text_message(1, audiobot_id, bot_cmd)
        .await?;

    let reply = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ts_rx.recv().await {
                Ok(TsEvent::TextMessage(msg))
                    if msg.invoker_id == audiobot_id && msg.target_mode == reply_target =>
                {
                    return msg.message;
                }
                Ok(_) => continue,
                Err(e) => {
                    debug!("TS event channel error while waiting for {bot_label} reply: {e}");
                    return String::new();
                }
            }
        }
    })
    .await;

    match reply {
        Ok(content) if !content.is_empty() => {
            info!("{bot_label} replied: {content}");
            Ok(content.into())
        }
        Ok(_) => Ok(json!({
            "status": "ok",
            "sent_to": bot_label,
            "command": bot_cmd
        })),
        Err(_) => {
            debug!("Timed out waiting for {bot_label} reply");
            Ok(json!({
                "status": "timeout",
                "sent_to": bot_label,
                "command": bot_cmd
            }))
        }
    }
}
