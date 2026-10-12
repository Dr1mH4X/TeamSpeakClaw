//! 运行期上下文快照：调用者、在线名单等易变事实按命名有序字段拼成一段文本，
//! 由 `LlmEngine` 放进本回合的 user 消息，不进入 system 提示词。

/// 字段位置：数值升序拼接，同序按名字升序
pub mod field_order {
    pub const CALLER: i32 = 100;
    pub const ONLINE: i32 = 200;
}

/// 有序命名字段集合：文本为空的字段被丢弃，其余按 `(order, name)` 升序以换行拼接
#[derive(Default)]
pub struct RuntimeContext {
    fields: Vec<(&'static str, i32, String)>,
}

impl RuntimeContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn field(mut self, name: &'static str, order: i32, text: impl Into<String>) -> Self {
        self.fields.push((name, order, text.into()));
        self
    }

    pub fn render(&self) -> String {
        let mut ordered: Vec<&(&'static str, i32, String)> = self.fields.iter().collect();
        ordered.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(right.0)));
        ordered
            .into_iter()
            .map(|(_, _, text)| text.as_str())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// TeamSpeak 调用者与在线名单快照；`online` 是 `list_clients_json` 的 JSON 数组文本
pub fn ts_context(invoker_name: &str, clid: u32, channel_id: u64, online: &str) -> String {
    RuntimeContext::new()
        .field(
            "invoker",
            field_order::CALLER,
            format!(
                r#"invoker: {{"name":"{invoker_name}","clid":{clid},"channel_id":{channel_id}}}"#
            ),
        )
        .field("online", field_order::ONLINE, format!("Online: {online}"))
        .render()
}

/// NapCat 调用者与（可选的）TeamSpeak 在线名单快照
pub fn nc_context(sender_name: &str, user_id: i64, group_id: Option<i64>, online: &str) -> String {
    let caller = match group_id {
        Some(group_id) => format!("User: {sender_name} (QQ: {user_id}, Group: {group_id})"),
        None => format!("User: {sender_name} (QQ: {user_id}, Private Chat)"),
    };
    let online = if online.is_empty() {
        String::new()
    } else {
        format!("Online: {online}")
    };
    RuntimeContext::new()
        .field("caller", field_order::CALLER, caller)
        .field("online", field_order::ONLINE, online)
        .render()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_render_in_order_and_empty_fields_are_dropped() {
        let rendered = RuntimeContext::new()
            .field("online", field_order::ONLINE, "Online: []")
            .field("invoker", field_order::CALLER, "invoker: {}")
            .field("note", 150, "")
            .render();
        assert_eq!(rendered, "invoker: {}\nOnline: []");
    }

    #[test]
    fn ts_context_keeps_the_invoker_snapshot_on_one_line() {
        let rendered = ts_context("alice", 7, 42, r#"[{"name":"alice"}]"#);
        assert_eq!(
            rendered,
            "invoker: {\"name\":\"alice\",\"clid\":7,\"channel_id\":42}\nOnline: [{\"name\":\"alice\"}]"
        );
    }

    #[test]
    fn nc_context_names_the_group_or_private_chat_and_omits_absent_online_list() {
        assert_eq!(
            nc_context("bob", 10001, Some(-20002), ""),
            "User: bob (QQ: 10001, Group: -20002)"
        );
        assert_eq!(
            nc_context("bob", 10001, None, r#"[{"name":"alice"}]"#),
            "User: bob (QQ: 10001, Private Chat)\nOnline: [{\"name\":\"alice\"}]"
        );
    }
}
