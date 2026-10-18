//! Exa MCP 响应规范化。
//!
//! 匿名的 Exa MCP 端点会为每个搜索结果返回一个文本块，
//! 采用 `标题 / URL / 发布时间 / 高亮摘要` 的布局格式，
//! 各结果之间以 `---` 分隔。消费端会将该文本转换为与所有其他提供方所使用的相同的源数据模型。

use super::render::{sanitize_external_text, SearchOutcome, SearchSource, MAX_SOURCES};
use url::Url;

pub(crate) fn parse_exa_text(text: &str) -> SearchOutcome {
    let normalized = text.replace("\r\n", "\n");
    let mut sources = Vec::new();
    for block in normalized.split("\n---\n") {
        if let Some(source) = parse_block(block) {
            sources.push(source);
        }
    }

    if sources.is_empty() {
        return SearchOutcome::new(
            Some(sanitize_external_text(text)).filter(|text| !text.is_empty()),
            Vec::new(),
            false,
        );
    }

    let truncated = sources.len() > MAX_SOURCES;
    SearchOutcome::new(None, sources, truncated)
}

fn parse_block(block: &str) -> Option<SearchSource> {
    let mut title = None;
    let mut url = None;
    let mut published_at = None;
    let mut in_highlights = false;
    let mut highlight_lines = Vec::new();

    for line in block.lines() {
        if in_highlights {
            highlight_lines.push(line);
            continue;
        }
        if let Some(value) = line.strip_prefix("Title:") {
            title = non_empty(value);
            continue;
        }
        if let Some(value) = line.strip_prefix("URL:") {
            url = parse_http_url(value.trim());
            continue;
        }
        if let Some(value) = line.strip_prefix("Published:") {
            published_at = non_empty(value).filter(|value| value.as_str() != "N/A");
            continue;
        }
        if let Some(value) = line.strip_prefix("Highlights:") {
            in_highlights = true;
            if !value.trim().is_empty() {
                highlight_lines.push(value);
            }
        }
    }

    let highlights = non_empty(&highlight_lines.join("\n"));
    Some(SearchSource {
        url: url?,
        title,
        snippet: highlights.map(|text| sanitize_external_text(&text)),
        published_at,
    })
    .filter(|source| {
        source
            .snippet
            .as_deref()
            .is_some_and(|text| !text.is_empty())
    })
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn parse_http_url(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_title_url_published_and_highlights() {
        let outcome = parse_exa_text(
            "Title: Rust\nURL: https://www.rust-lang.org/\nPublished: 2026-01-02\nAuthor: N/A\nHighlights:\nRust is fast.\n\nIt is safe.\n---\nTitle: Docs\nURL: https://doc.rust-lang.org/\nPublished: N/A\nHighlights:\nThe Rust book.",
        );
        assert_eq!(outcome.sources.len(), 2);
        assert_eq!(outcome.sources[0].title.as_deref(), Some("Rust"));
        assert_eq!(outcome.sources[0].url, "https://www.rust-lang.org/");
        assert_eq!(
            outcome.sources[0].published_at.as_deref(),
            Some("2026-01-02")
        );
        assert_eq!(
            outcome.sources[0].snippet.as_deref(),
            Some("Rust is fast.\n\nIt is safe.")
        );
        assert_eq!(outcome.sources[1].published_at, None);
    }

    #[test]
    fn drops_entries_without_url_or_snippet() {
        let outcome = parse_exa_text(
            "Title: No URL\nHighlights:\ntext\n---\nTitle: No snippet\nURL: https://a.test/\n---\nTitle: Kept\nURL: https://b.test/\nHighlights:\nkept",
        );
        assert_eq!(outcome.sources.len(), 1);
        assert_eq!(outcome.sources[0].url, "https://b.test/");
    }

    #[test]
    fn rejects_non_http_urls() {
        let outcome = parse_exa_text(
            "Title: Script\nURL: javascript:alert(1)\nHighlights:\ntext\n---\nTitle: File\nURL: file:///etc/passwd\nHighlights:\ntext",
        );
        assert!(outcome.sources.is_empty());
        assert!(outcome.content.is_some());
    }

    #[test]
    fn falls_back_to_unstructured_text_when_layout_drifts() {
        let outcome = parse_exa_text("a provider answer without the expected headers");
        assert!(outcome.sources.is_empty());
        assert_eq!(
            outcome.content.as_deref(),
            Some("a provider answer without the expected headers")
        );
    }

    #[test]
    fn caps_the_number_of_sources() {
        let text = (0..25)
            .map(|index| {
                format!("Title: t{index}\nURL: https://e.test/{index}\nHighlights:\nsnippet")
            })
            .collect::<Vec<_>>()
            .join("\n---\n");
        let outcome = parse_exa_text(&text);
        assert_eq!(outcome.sources.len(), MAX_SOURCES);
        assert!(outcome.truncated);
    }
}
