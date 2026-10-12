//! 模型可见提示词的唯一归属地。system 提示词由有序命名段落装配，`{{name}}` 变量在装配期严格解析；
//! 调用者与在线名单这类易变事实由 `context` 生成独立的 user 角色快照，不进入 system 提示词。
//! 段落文本与 TTS 风格提示都是源码常量，不作为用户配置暴露。

pub mod context;
pub mod sections;

use anyhow::{bail, Result};

/// TTS 风格提示：随每段合成请求作为 `user` 消息交给 TTS API，控制语调与语速
pub const TTS_STYLE_PROMPT: &str = "Natural, friendly tone, moderate pace.";

/// 一个第一方提示词段落：`order` 决定拼接位置，同序按 `name` 升序；文本为空的段落不出现
pub struct PromptSection {
    pub name: &'static str,
    pub order: i32,
    pub text: &'static str,
}

/// 装配期变量：未注册或未赋值的 `{{name}}` 引用让装配失败，而不是发出残缺提示词
pub struct PromptVariables {
    date: String,
}

impl PromptVariables {
    /// 取当前本地日期（`YYYY-MM-DD`）
    pub fn now() -> Self {
        Self {
            date: chrono::Local::now().format("%Y-%m-%d").to_string(),
        }
    }

    fn get(&self, name: &str) -> Option<&str> {
        match name {
            "date" => Some(&self.date),
            _ => None,
        }
    }
}

/// 装配 system 提示词：段落按 `(order, name)` 升序，空段落丢弃，段落之间以空行分隔
pub fn system_prompt() -> Result<String> {
    render(sections::SECTIONS, &PromptVariables::now())
}

/// 渲染给定段落；未知变量或残缺的变量引用一律报错
pub fn render(sections: &[PromptSection], variables: &PromptVariables) -> Result<String> {
    let mut ordered: Vec<&PromptSection> = sections.iter().collect();
    ordered.sort_by(|left, right| {
        left.order
            .cmp(&right.order)
            .then_with(|| left.name.cmp(right.name))
    });

    let mut rendered = Vec::with_capacity(ordered.len());
    for section in ordered {
        let text = interpolate(section.name, section.text, variables)?;
        if !text.is_empty() {
            rendered.push(text);
        }
    }
    Ok(rendered.join("\n\n"))
}

/// 解析段落里的 `{{name}}` 引用；替换值不再被扫描
fn interpolate(section: &str, text: &str, variables: &PromptVariables) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after_open = &rest[open + 2..];
        let Some(close) = after_open.find("}}") else {
            bail!("prompt section {section:?} has an unterminated variable reference");
        };
        let name = &after_open[..close];
        if !is_variable_name(name) {
            let reference = ["{{", name, "}}"].concat();
            bail!("prompt section {section:?} has a malformed variable reference {reference:?}");
        }
        let Some(value) = variables.get(name) else {
            let reference = ["{{", name, "}}"].concat();
            bail!("prompt section {section:?} references unregistered variable {reference:?}");
        };
        out.push_str(value);
        rest = &after_open[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// 变量名形如 `[a-z][a-z0-9_]*`
fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(name: &'static str, order: i32, text: &'static str) -> PromptSection {
        PromptSection { name, order, text }
    }

    #[test]
    fn first_party_sections_are_spaced_and_named_uniquely() {
        let mut orders: Vec<i32> = sections::SECTIONS.iter().map(|s| s.order).collect();
        orders.sort_unstable();
        let before = orders.len();
        orders.dedup();
        assert_eq!(before, orders.len(), "section orders must be unique");

        for pair in orders.windows(2) {
            assert!(
                pair[1] - pair[0] >= 10,
                "section orders need room to insert between: {pair:?}"
            );
        }

        let mut names: Vec<&str> = sections::SECTIONS.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "section names must be unique");
    }

    #[test]
    fn system_prompt_resolves_every_variable_and_keeps_section_order() {
        let prompt = system_prompt().unwrap();
        assert!(!prompt.contains("{{"), "every reference must resolve");
        assert!(prompt.starts_with(sections::SECTIONS[0].text));
        assert!(prompt.contains(&PromptVariables::now().date));
    }

    #[test]
    fn sections_render_in_order_and_blank_lines_separate_them() {
        let sections = [
            section("later", 200, "second"),
            section("earlier", 100, "first"),
            section("same_order_a", 300, "a"),
            section("same_order_b", 300, "b"),
            section("empty", 400, ""),
        ];
        let rendered = render(&sections, &PromptVariables::now()).unwrap();
        assert_eq!(rendered, "first\n\nsecond\n\na\n\nb");
    }

    #[test]
    fn unknown_or_malformed_variables_fail_assembly() {
        assert!(render(
            &[section("bad", 100, "{{missing}}")],
            &PromptVariables::now()
        )
        .is_err());
        assert!(render(&[section("bad", 100, "{{Date}}")], &PromptVariables::now()).is_err());
        assert!(render(&[section("bad", 100, "{{}}")], &PromptVariables::now()).is_err());
        assert!(render(&[section("bad", 100, "a {{date")], &PromptVariables::now()).is_err());
    }
}
