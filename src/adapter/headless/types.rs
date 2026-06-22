use tokio::sync::broadcast;

use super::tsbot::voice::v1 as voicev1;

pub fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => 0,
    }
}

pub fn emit_log(events_tx: &broadcast::Sender<voicev1::Event>, level: i32, msg: impl Into<String>) {
    let _ = events_tx.send(voicev1::Event {
        unix_ms: now_unix_ms(),
        payload: Some(voicev1::event::Payload::Log(voicev1::LogEvent {
            level,
            message: msg.into(),
        })),
    });
}
