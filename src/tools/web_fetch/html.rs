//! `web_fetch` 的 HTML 到 markdown 转换。
//!
//! 在序列化之前，活动元素、隐藏元素和非内容元素都会被移除。
//! 转换受到源字符数和元素深度的限制；对于转换器无法安全表示的页面，
//! 将以固定的 omission marker 代替。

use scraper::{ElementRef, Html, Node};
use url::Url;

const MAX_DEPTH: usize = 512;
const OMITTED: &str = "[HTML content omitted: unable to convert safely.]";
const EMPTY: &str = "[No readable text content.]";

pub(crate) struct HtmlRender {
    pub(crate) text: String,
    pub(crate) truncated: bool,
}

pub(crate) fn html_to_markdown(html: &str, base: &Url, max_input_chars: usize) -> HtmlRender {
    let source_chars = html.chars().count();
    let source = html.chars().take(max_input_chars).collect::<String>();
    let mut renderer = Renderer {
        out: String::new(),
        base,
        list_stack: Vec::new(),
        pre_depth: 0,
        pending_space: false,
        depth_exceeded: false,
        in_table_cell: false,
    };
    let document = Html::parse_document(&source);
    renderer.render_element(document.root_element(), 0);

    let text = if renderer.depth_exceeded {
        OMITTED.to_string()
    } else {
        let normalized = normalize_output(&renderer.out);
        if normalized.is_empty() {
            EMPTY.to_string()
        } else {
            normalized
        }
    };
    HtmlRender {
        text,
        truncated: source_chars > max_input_chars || renderer.depth_exceeded,
    }
}

struct Renderer<'a> {
    out: String,
    base: &'a Url,
    list_stack: Vec<bool>,
    pre_depth: usize,
    pending_space: bool,
    depth_exceeded: bool,
    in_table_cell: bool,
}

impl Renderer<'_> {
    fn render_node(&mut self, node: ego_tree::NodeRef<'_, Node>, depth: usize) {
        match node.value() {
            Node::Text(text) => self.push_text(text),
            Node::Element(_) => {
                if let Some(element) = ElementRef::wrap(node) {
                    self.render_element(element, depth);
                }
            }
            _ => {}
        }
    }

    fn render_element(&mut self, element: ElementRef<'_>, depth: usize) {
        if depth > MAX_DEPTH {
            self.depth_exceeded = true;
            return;
        }
        if should_skip(&element) {
            return;
        }
        let name = element.value().name();
        match name {
            "br" => self.line(),
            "hr" => {
                self.blank_line();
                self.out.push_str("---");
                self.line();
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "aside"
            | "nav" | "figure" | "figcaption" | "form" | "fieldset" | "dl" | "dt" | "dd" => {
                self.blank_line();
                self.render_children(element, depth);
                self.blank_line();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.blank_line();
                let level = name.as_bytes()[1] - b'0';
                self.out.push_str(&"#".repeat(level as usize));
                self.out.push(' ');
                self.render_children(element, depth);
                self.blank_line();
            }
            "ul" | "ol" => {
                self.blank_line();
                self.list_stack.push(name == "ol");
                self.render_children(element, depth);
                self.list_stack.pop();
                self.blank_line();
            }
            "li" => {
                self.line();
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                let ordered = self.list_stack.last().copied().unwrap_or(false);
                self.out.push_str(&indent);
                self.out.push_str(if ordered { "1. " } else { "- " });
                self.render_children(element, depth);
                self.line();
            }
            "pre" => {
                self.blank_line();
                self.out.push_str("```\n");
                self.pre_depth += 1;
                self.render_children(element, depth);
                self.pre_depth -= 1;
                self.out.push_str("\n```");
                self.blank_line();
            }
            "code" => {
                if self.pre_depth == 0 {
                    self.flush_space();
                    self.out.push('`');
                    self.render_children(element, depth);
                    self.out.push('`');
                } else {
                    self.render_children(element, depth);
                }
            }
            "strong" | "b" => {
                self.flush_space();
                self.out.push_str("**");
                self.render_children(element, depth);
                self.out.push_str("**");
            }
            "em" | "i" => {
                self.flush_space();
                self.out.push('*');
                self.render_children(element, depth);
                self.out.push('*');
            }
            "blockquote" => {
                self.blank_line();
                self.render_children(element, depth);
                self.blank_line();
            }
            "a" => self.render_link(element, depth),
            "img" => self.render_image(&element),
            "table" => self.render_table(&element, depth),
            _ => self.render_children(element, depth),
        }
    }

    fn render_children(&mut self, element: ElementRef<'_>, depth: usize) {
        for child in element.children() {
            self.render_node(child, depth + 1);
        }
    }

    fn render_link(&mut self, element: ElementRef<'_>, depth: usize) {
        let href = element
            .attr("href")
            .and_then(|href| resolve_link(self.base, href));
        if let Some(href) = href {
            self.flush_space();
            self.out.push('[');
            self.render_children(element, depth);
            self.out.push_str("](");
            self.out.push_str(&href);
            self.out.push(')');
        } else {
            self.render_children(element, depth);
        }
    }

    fn render_image(&mut self, element: &ElementRef<'_>) {
        let Some(src) = element
            .attr("src")
            .and_then(|src| resolve_link(self.base, src))
        else {
            return;
        };
        let alt = element.attr("alt").unwrap_or("image").replace(']', "\\]");
        self.flush_space();
        self.out.push_str("![");
        self.out.push_str(&alt);
        self.out.push_str("](");
        self.out.push_str(&src);
        self.out.push(')');
    }

    fn render_table(&mut self, table: &ElementRef<'_>, depth: usize) {
        if self.in_table_cell {
            self.render_table_text(table, depth);
            return;
        }

        let rows = table_rows(table, depth)
            .into_iter()
            .map(|(row, row_depth)| {
                table_cells(&row)
                    .into_iter()
                    .map(|cell| self.render_table_cell(cell, row_depth + 1))
                    .collect::<Vec<_>>()
            })
            .filter(|cells| !cells.is_empty())
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return;
        }
        self.blank_line();
        for (index, cells) in rows.iter().enumerate() {
            self.out.push('|');
            for cell in cells {
                self.out.push(' ');
                self.out.push_str(&cell.replace('|', "\\|"));
                self.out.push_str(" |");
            }
            self.line();
            if index == 0 {
                self.out.push('|');
                for _ in cells {
                    self.out.push_str(" --- |");
                }
                self.line();
            }
        }
        self.blank_line();
    }

    /// 表格单元格内的嵌套表格压平为纯文本：保留各单元格的文字，不输出
    /// GFM 定界符与分隔行，外层单元格统一转义字面 `|`。
    fn render_table_text(&mut self, table: &ElementRef<'_>, depth: usize) {
        let mut rendered = false;
        for (row, row_depth) in table_rows(table, depth) {
            for cell in table_cells(&row) {
                let text = self.render_table_cell(cell, row_depth + 1);
                if text.is_empty() {
                    continue;
                }
                if !self.out.is_empty() && !self.out.ends_with(' ') && !self.out.ends_with('\n') {
                    self.out.push(' ');
                }
                self.out.push_str(&text);
                rendered = true;
            }
        }
        if rendered {
            self.pending_space = true;
        }
    }

    fn render_table_cell(&mut self, cell: ElementRef<'_>, depth: usize) -> String {
        let outer_out = std::mem::take(&mut self.out);
        let outer_list_stack = std::mem::take(&mut self.list_stack);
        let outer_pre_depth = self.pre_depth;
        let outer_pending_space = self.pending_space;
        let outer_in_table_cell = self.in_table_cell;
        self.pre_depth = 0;
        self.pending_space = false;
        self.in_table_cell = true;
        self.render_children(cell, depth);
        let text = normalize_output(&self.out).replace('\n', " ");
        self.out = outer_out;
        self.list_stack = outer_list_stack;
        self.pre_depth = outer_pre_depth;
        self.pending_space = outer_pending_space;
        self.in_table_cell = outer_in_table_cell;
        text
    }

    fn push_text(&mut self, text: &str) {
        if self.pre_depth > 0 {
            self.out.push_str(text);
            return;
        }
        for ch in text.chars() {
            if ch.is_whitespace() {
                self.pending_space = true;
                continue;
            }
            self.flush_space();
            self.out.push(ch);
        }
    }

    fn line(&mut self) {
        self.pending_space = false;
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn blank_line(&mut self) {
        self.pending_space = false;
        if self.out.is_empty() {
            return;
        }
        if !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        if !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn flush_space(&mut self) {
        if self.pending_space
            && !self.out.is_empty()
            && !self.out.ends_with(' ')
            && !self.out.ends_with('\n')
            && !self.out.ends_with('(')
            && !self.out.ends_with('[')
        {
            self.out.push(' ');
        }
        self.pending_space = false;
    }
}

fn table_rows<'a>(table: &ElementRef<'a>, depth: usize) -> Vec<(ElementRef<'a>, usize)> {
    let mut rows = Vec::new();
    for child in table.child_elements() {
        match child.value().name() {
            "thead" | "tbody" | "tfoot" => {
                if should_skip(&child) {
                    continue;
                }
                for row in child.child_elements() {
                    if row.value().name() == "tr" && !should_skip(&row) {
                        rows.push((row, depth + 2));
                    }
                }
            }
            "tr" if !should_skip(&child) => rows.push((child, depth + 1)),
            _ => {}
        }
    }
    rows
}

fn table_cells<'a>(row: &ElementRef<'a>) -> Vec<ElementRef<'a>> {
    row.child_elements()
        .filter(|cell| matches!(cell.value().name(), "th" | "td") && !should_skip(cell))
        .collect()
}

fn should_skip(element: &ElementRef<'_>) -> bool {
    let name = element.value().name();
    if matches!(
        name,
        "script"
            | "style"
            | "noscript"
            | "template"
            | "iframe"
            | "object"
            | "embed"
            | "svg"
            | "canvas"
            | "head"
            | "title"
            | "meta"
            | "link"
            | "base"
    ) {
        return true;
    }
    if element.attr("hidden").is_some()
        || element
            .attr("aria-hidden")
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
        || (name == "input"
            && element
                .attr("type")
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("hidden")))
    {
        return true;
    }
    element.attr("style").is_some_and(style_declares_hidden)
}

fn style_declares_hidden(style: &str) -> bool {
    let mut display = None;
    let mut visibility = None;

    for declaration in StyleDeclarations::new(style) {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let property = property.trim();
        let (value, important) = strip_important(value.trim());
        if property.eq_ignore_ascii_case("display") {
            record_declaration(&mut display, value.eq_ignore_ascii_case("none"), important);
        } else if property.eq_ignore_ascii_case("visibility") {
            record_declaration(
                &mut visibility,
                value.eq_ignore_ascii_case("hidden") || value.eq_ignore_ascii_case("collapse"),
                important,
            );
        }
    }

    display.is_some_and(|declaration| declaration.hidden)
        || visibility.is_some_and(|declaration| declaration.hidden)
}

/// 同名属性的有效声明：`!important` 压过普通声明，同优先级下后写覆盖先写。
#[derive(Clone, Copy)]
struct EffectiveDeclaration {
    hidden: bool,
    important: bool,
}

fn record_declaration(slot: &mut Option<EffectiveDeclaration>, hidden: bool, important: bool) {
    let replace = match *slot {
        None => true,
        Some(current) => important || !current.important,
    };
    if replace {
        *slot = Some(EffectiveDeclaration { hidden, important });
    }
}

fn strip_important(value: &str) -> (&str, bool) {
    let value = value.trim_end();
    let Some(position) = value.rfind('!') else {
        return (value, false);
    };
    let (head, marker) = value.split_at(position);
    if marker[1..].trim().eq_ignore_ascii_case("important") {
        (head.trim_end(), true)
    } else {
        (value, false)
    }
}

/// 按 CSS 声明边界切分内联样式。引号、转义、函数括号与注释内的分号
/// 不属于声明分隔符；注释替换为空格，避免把相邻 token 拼成一个值。
struct StyleDeclarations<'a> {
    style: &'a str,
    offset: usize,
}

impl<'a> StyleDeclarations<'a> {
    fn new(style: &'a str) -> Self {
        Self { style, offset: 0 }
    }
}

impl Iterator for StyleDeclarations<'_> {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        while self.offset <= self.style.len() {
            let start = self.offset;
            let mut declaration = String::new();
            let mut quote = None;
            let mut escaped = false;
            let mut paren_depth = 0usize;
            let mut in_comment = false;
            let mut chars = self.style[start..].char_indices().peekable();
            let mut end = self.style.len();

            while let Some((index, ch)) = chars.next() {
                if in_comment {
                    if ch == '*' && chars.peek().is_some_and(|(_, next)| *next == '/') {
                        chars.next();
                        in_comment = false;
                    }
                    continue;
                }
                if escaped {
                    declaration.push(ch);
                    escaped = false;
                    continue;
                }
                if let Some(quote_char) = quote {
                    declaration.push(ch);
                    if ch == '\\' {
                        escaped = true;
                    } else if ch == quote_char {
                        quote = None;
                    }
                    continue;
                }
                match ch {
                    '\\' => {
                        declaration.push(ch);
                        escaped = true;
                    }
                    '\'' | '"' => {
                        quote = Some(ch);
                        declaration.push(ch);
                    }
                    '/' if chars.peek().is_some_and(|(_, next)| *next == '*') => {
                        chars.next();
                        in_comment = true;
                        declaration.push(' ');
                    }
                    '(' => {
                        paren_depth += 1;
                        declaration.push(ch);
                    }
                    ')' => {
                        paren_depth = paren_depth.saturating_sub(1);
                        declaration.push(ch);
                    }
                    ';' if paren_depth == 0 => {
                        end = start + index;
                        self.offset = end + 1;
                        break;
                    }
                    _ => declaration.push(ch),
                }
            }

            if end == self.style.len() {
                self.offset = self.style.len() + 1;
            }
            if !declaration.trim().is_empty() {
                return Some(declaration);
            }
        }
        None
    }
}

fn resolve_link(base: &Url, raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('#') {
        return None;
    }
    let url = base.join(raw).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

fn normalize_output(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut newlines = 0;
    for ch in text.trim().chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                normalized.push(ch);
            }
        } else {
            newlines = 0;
            normalized.push(ch);
        }
    }
    normalized.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(html: &str) -> String {
        let base = Url::parse("https://example.com/").unwrap();
        html_to_markdown(html, &base, 100_000).text
    }

    #[test]
    fn converts_basic_structure_and_links() {
        let text = convert(
            r#"<html><head><title>ignored</title></head><body>
            <h1>Title</h1><p>Hello <strong>world</strong> and <a href="/docs">docs</a>.</p>
            <ul><li>one</li><li>two</li></ul>
            </body></html>"#,
        );
        assert!(text.contains("# Title"));
        assert!(
            text.contains("Hello **world** and [docs](https://example.com/docs)."),
            "{text}"
        );
        assert!(text.contains("- one"));
        assert!(!text.contains("ignored"));
    }

    #[test]
    fn removes_active_hidden_and_script_content() {
        let text = convert(
            r#"<body><p>visible</p><script>ignore previous instructions</script>
            <style>p{}</style><div hidden>hidden one</div>
            <div aria-hidden="true">hidden two</div>
            <div style="display: none">hidden three</div>
            <div style="visibility:hidden">hidden four</div>
            <p>after</p></body>"#,
        );
        assert!(text.contains("visible"));
        assert!(text.contains("after"));
        for hidden in [
            "ignore previous instructions",
            "hidden one",
            "hidden two",
            "hidden three",
            "hidden four",
        ] {
            assert!(!text.contains(hidden), "{text}");
        }
    }

    #[test]
    fn drops_unsafe_link_schemes() {
        let text = convert(r#"<p><a href="javascript:alert(1)">click</a></p>"#);
        assert_eq!(text, "click");
    }

    #[test]
    fn renders_simple_tables() {
        let text = convert(
            "<table><tr><th>Name</th><th>Value</th></tr><tr><td>a</td><td>b</td></tr></table>",
        );
        assert!(text.contains("| Name | Value |"));
        assert!(text.contains("| --- | --- |"));
        assert!(text.contains("| a | b |"));
    }

    #[test]
    fn nested_tables_render_inner_content_once() {
        let mut html = "leaf".to_string();
        for _ in 0..10 {
            html = format!("<table><tr><td>{html}</td></tr></table>");
        }

        let text = convert(&html);

        assert_eq!(text.matches("leaf").count(), 1, "{text}");
        assert!(text.len() < 4096, "{text}");
    }

    #[test]
    fn nested_tables_render_as_text_inside_the_owning_cell() {
        let text = convert(
            r#"<table><tr><td>OUTER<table><tr><td>INNER_ONE</td><td>INNER_TWO</td></tr></table>TAIL</td></tr></table>"#,
        );

        assert!(text.contains("OUTER INNER_ONE INNER_TWO TAIL"), "{text}");
        assert!(!text.contains("\\|"), "{text}");
        assert_eq!(text.matches(" --- ").count(), 1, "{text}");
    }

    #[test]
    fn tables_drop_hidden_sections_rows_cells_and_children() {
        let text = convert(
            r#"<table><tbody hidden><tr><td>HIDDEN_BODY</td></tr></tbody></table>
            <table><tr hidden><td>HIDDEN_ROW</td></tr>
            <tr aria-hidden="true"><td>HIDDEN_ARIA_ROW</td></tr>
            <tr><td hidden>HIDDEN_CELL</td>
            <td style="display: none !important">HIDDEN_STYLE_CELL</td>
            <td><span hidden>HIDDEN_CHILD</span>
            <table hidden><tr><td>HIDDEN_TABLE</td></tr></table>VISIBLE</td></tr></table>"#,
        );

        for hidden in [
            "HIDDEN_BODY",
            "HIDDEN_ROW",
            "HIDDEN_ARIA_ROW",
            "HIDDEN_CELL",
            "HIDDEN_STYLE_CELL",
            "HIDDEN_CHILD",
            "HIDDEN_TABLE",
        ] {
            assert!(!text.contains(hidden), "{text}");
        }
        assert!(text.contains("VISIBLE"), "{text}");
    }

    #[test]
    fn style_text_that_mentions_hidden_values_does_not_hide_content() {
        let text = convert(
            r#"<p style="content: 'display:none; visibility:hidden'">VISIBLE_STYLE_TEXT</p>"#,
        );

        assert_eq!(text, "VISIBLE_STYLE_TEXT");
    }

    #[test]
    fn quoted_semicolons_do_not_create_hidden_declarations() {
        let text = convert(
            r#"<p style="font-family: 'a; visibility:hidden; b'">VISIBLE_QUOTE</p>
            <p style="background: url('x;visibility:hidden'); color: red">VISIBLE_URL</p>
            <p style="color: red /* ; visibility:hidden */">VISIBLE_COMMENT</p>"#,
        );

        for visible in ["VISIBLE_QUOTE", "VISIBLE_URL", "VISIBLE_COMMENT"] {
            assert!(text.contains(visible), "{text}");
        }
    }

    #[test]
    fn effective_visibility_declarations_follow_css_precedence() {
        let text = convert(
            r#"<p style="display:none; display:block">VISIBLE_DISPLAY</p>
            <p style="visibility:hidden; visibility:visible">VISIBLE_VISIBILITY</p>
            <p style="display:none; display:block !important">VISIBLE_IMPORTANT</p>
            <p style="display:none !important; display:block !important">VISIBLE_BOTH_IMPORTANT</p>
            <p style="display:none !important; display:block">HIDDEN_IMPORTANT</p>
            <p style="/* x */ display:none">HIDDEN_COMMENT_DECL</p>"#,
        );

        for visible in [
            "VISIBLE_DISPLAY",
            "VISIBLE_VISIBILITY",
            "VISIBLE_IMPORTANT",
            "VISIBLE_BOTH_IMPORTANT",
        ] {
            assert!(text.contains(visible), "{text}");
        }
        assert!(!text.contains("HIDDEN_IMPORTANT"), "{text}");
        assert!(!text.contains("HIDDEN_COMMENT_DECL"), "{text}");
    }

    #[test]
    fn deeply_nested_tables_share_the_conversion_depth_limit() {
        let mut html = "leaf".to_string();
        for _ in 0..MAX_DEPTH {
            html = format!("<table><tr><td>{html}</td></tr></table>");
        }

        assert_eq!(convert(&html), OMITTED);
    }

    #[test]
    fn reports_source_truncation() {
        let base = Url::parse("https://example.com/").unwrap();
        let rendered = html_to_markdown("<p>abcdef</p>", &base, 4);
        assert!(rendered.truncated);
    }

    #[test]
    fn empty_html_gets_a_fixed_marker() {
        assert_eq!(convert("<html><body></body></html>"), EMPTY);
    }
}
