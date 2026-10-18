//! `web_fetch` 的 HTML 到 markdown 转换。
//!
//! 在序列化之前，活动元素、隐藏元素和非内容元素都会被移除。
//! 转换受到源字符数和元素深度的限制；对于转换器无法安全表示的页面，
//! 将以固定的 omission marker 代替。

use scraper::{ElementRef, Html, Node, Selector};
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
            "table" => self.render_table(&element),
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

    fn render_table(&mut self, table: &ElementRef<'_>) {
        let Ok(row_selector) = Selector::parse("tr") else {
            return;
        };
        let Ok(cell_selector) = Selector::parse("th, td") else {
            return;
        };
        let rows = table
            .select(&row_selector)
            .map(|row| {
                row.select(&cell_selector)
                    .map(|cell| plain_text(&cell, self.base))
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
    element.attr("style").is_some_and(|style| {
        let style = style
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        style.contains("display:none")
            || style.contains("visibility:hidden")
            || style.contains("visibility:collapse")
    })
}

fn plain_text(element: &ElementRef<'_>, base: &Url) -> String {
    let mut renderer = Renderer {
        out: String::new(),
        base,
        list_stack: Vec::new(),
        pre_depth: 0,
        pending_space: false,
        depth_exceeded: false,
    };
    for child in element.children() {
        renderer.render_node(child, 0);
    }
    normalize_output(&renderer.out).replace('\n', " ")
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
