use crate::adapter::command::cmd_send_text;
use crate::adapter::serverquery::event::{TextMessageTarget, TsEvent};
use crate::skills::ExecutionContext;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info};

static TS3AUDIOBOT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(crate) async fn execute(
    action: &str,
    args: &Value,
    ctx: &ExecutionContext<'_>,
) -> Result<Value> {
    let value = args["value"].as_str().unwrap_or("");

    let bot_cmd = match action {
        "next" => "!yun next".to_string(),
        "ts_login" => "!yun login".to_string(),
        "play" | "ts_play" => format!("!yun play {value}"),
        "ts_add" => format!("!yun add {value}"),
        "ts_gedan" => format!("!yun gedan {value}"),
        "ts_gedanid" => format!("!yun gedanid {value}"),
        "ts_playid" => format!("!yun playid {value}"),
        "ts_addid" => format!("!yun addid {value}"),
        "ts_mode" => format!("!yun mode {value}"),
        "search" => {
            let kw = args["keywords"].as_str().unwrap_or(value);
            format!("!yun play {kw}")
        }
        "repeat" => {
            let mode = args["repeat_mode"].as_str().unwrap_or("all");
            let mode_num = match mode {
                "none" => "0",
                "one" => "1",
                _ => "2",
            };
            format!("!yun mode {mode_num}")
        }
        "pause" => "!yun pause".to_string(),
        "skip" => "!yun next".to_string(),
        other => {
            return Err(anyhow::anyhow!(
                "Action '{}' is not supported by the ts3audiobot backend.",
                other
            ))
        }
    };

    let clients: Vec<_> = ctx.clients.iter().map(|r| r.value().clone()).collect();
    let audiobot = clients
        .iter()
        .find(|c| c.nickname == "TS3AudioBot")
        .ok_or_else(|| anyhow::anyhow!("TS3AudioBot not found online"))?;

    let _guard = TS3AUDIOBOT_LOCK.lock().await;

    let mut ts_rx = ctx.adapter.subscribe();

    ctx.adapter
        .send_raw(&cmd_send_text(1, audiobot.clid, &bot_cmd))
        .await?;

    let reply = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ts_rx.recv().await {
                Ok(TsEvent::TextMessage(msg))
                    if msg.invoker_name == "TS3AudioBot"
                        && msg.target_mode == TextMessageTarget::Private =>
                {
                    return msg.message;
                }
                Ok(_) => continue,
                Err(e) => {
                    debug!("TS event channel error while waiting for TS3AudioBot reply: {e}");
                    return String::new();
                }
            }
        }
    })
    .await;

    drop(_guard);

    match reply {
        Ok(content) if !content.is_empty() => {
            info!("TS3AudioBot replied: {content}");
            Ok(content.into())
        }
        _ => Ok(json!({
            "status": "ok",
            "sent_to": "TS3AudioBot",
            "command": bot_cmd
        })),
    }
}
