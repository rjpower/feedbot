//! EPUB export, so an article can leave the browser and land on an e-reader.

use crate::db::Article;
use anyhow::{Context, Result};
use epub_builder::{EpubBuilder, EpubContent, ReferenceType, ZipLibrary};
use std::sync::LazyLock;

/// EPUB 2 wants XHTML, and Ammonia hands back HTML5 (`<br>`, not `<br/>`).
static VOID_TAG: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)<(area|base|br|col|embed|hr|img|input|link|meta|param|source|track|wbr)\b([^>]*?)\s*/?>",
    )
    .unwrap()
});

/// Ammonia guarantees quoted attributes and escaped entities, so a regex is
/// enough to close the void elements it leaves open.
///
/// It also serializes U+00A0 as `&nbsp;`, and XHTML defines only five entities
/// — `amp`, `lt`, `gt`, `quot`, `apos`. An e-reader's XML parser rejects the
/// whole chapter over one non-breaking space, so spell it numerically.
pub(crate) fn to_xhtml(html: &str) -> String {
    VOID_TAG
        .replace_all(html, "<$1$2/>")
        .replace("&nbsp;", "&#160;")
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const STYLE: &str = r#"
body { font-family: Georgia, serif; line-height: 1.6; margin: 0 6%; }
h1 { font-size: 1.5em; line-height: 1.25; margin: 1em 0 0.2em; }
.byline { color: #555; font-size: 0.85em; font-style: italic; margin-bottom: 2em; }
img { max-width: 100%; height: auto; }
blockquote { margin-left: 1em; padding-left: 1em; border-left: 3px solid #ccc; }
pre { white-space: pre-wrap; font-size: 0.85em; }
"#;

fn chapter(a: &Article) -> String {
    let mut meta = Vec::new();
    if let Some(b) = &a.byline {
        meta.push(escape(b));
    }
    if let Some(ts) = a.published_at
        && let Some(dt) = chrono::DateTime::from_timestamp(ts, 0)
    {
        meta.push(dt.format("%-d %B %Y").to_string());
    }
    meta.push(escape(&a.site_name));

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>{title}</title><meta http-equiv="Content-Type" content="text/html; charset=utf-8"/></head>
<body>
<h1>{title}</h1>
<p class="byline">{meta}</p>
{body}
<hr/>
<p class="byline"><a href="{url}">{url}</a></p>
</body>
</html>"#,
        title = escape(&a.title),
        meta = meta.join(" · "),
        body = to_xhtml(a.content_html.as_deref().unwrap_or("")),
        url = escape(&a.url),
    )
}

/// One `.epub` containing every article given, in order.
pub fn build(articles: &[Article], title: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(!articles.is_empty(), "nothing to export");

    let zip = ZipLibrary::new().map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut b = EpubBuilder::new(zip).map_err(|e| anyhow::anyhow!("{e}"))?;
    b.metadata("title", title)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    b.metadata("generator", "feedbot")
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Multi-article exports get a table of contents; a single article doesn't
    // need one standing between the reader and the text.
    let author = articles
        .iter()
        .filter_map(|a| a.byline.clone())
        .next()
        .unwrap_or_else(|| articles[0].site_name.clone());
    b.metadata("author", author)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    b.stylesheet(STYLE.as_bytes())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if articles.len() > 1 {
        b.inline_toc();
    }

    for (i, a) in articles.iter().enumerate() {
        let xhtml = chapter(a);
        let content = EpubContent::new(format!("article_{i}.xhtml"), xhtml.as_bytes())
            .title(a.title.clone())
            .reftype(ReferenceType::Text);
        b.add_content(content).map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    let mut out = Vec::new();
    b.generate(&mut out)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("generating epub")?;
    Ok(out)
}

/// A filename an e-reader won't choke on.
pub fn safe_filename(title: &str) -> String {
    let slug: String = title
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = slug.chars().take(60).collect::<String>();
    if slug.is_empty() {
        "article".into()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn article(title: &str) -> Article {
        Article {
            id: 1,
            site_id: 1,
            site_name: "The Digital Antiquarian".into(),
            url: "https://filfre.net/2026/07/maxis/".into(),
            title: title.into(),
            byline: Some("Jimmy Maher".into()),
            excerpt: None,
            word_count: 3,
            published_at: Some(1_783_000_000),
            fetched_at: 0,
            read_at: None,
            starred: false,
            content_html: Some(
                r#"<p>Hi&nbsp;<br> there</p><img src="https://x/i.png" alt="a &amp; b"><hr>"#
                    .into(),
            ),
        }
    }

    /// The five entities XHTML actually defines. Anything else fails an
    /// e-reader's XML parser, which is strict where a browser is forgiving.
    fn illegal_entities(s: &str) -> Vec<String> {
        regex::Regex::new(r"&([a-zA-Z][a-zA-Z0-9]*);")
            .unwrap()
            .captures_iter(s)
            .map(|c| c[1].to_string())
            .filter(|n| !matches!(n.as_str(), "amp" | "lt" | "gt" | "quot" | "apos"))
            .collect()
    }

    #[test]
    fn void_elements_are_closed_for_xhtml() {
        let x = to_xhtml(r#"<p>a<br>b</p><img src="u.png" alt="x"><hr>"#);
        assert_eq!(x, r#"<p>a<br/>b</p><img src="u.png" alt="x"/><hr/>"#);
    }

    #[test]
    fn already_closed_tags_are_left_alone() {
        assert_eq!(to_xhtml("<br/>"), "<br/>");
        assert_eq!(to_xhtml("<br />"), "<br/>");
    }

    #[test]
    fn non_void_tags_are_untouched() {
        let s = "<p>hello</p><div><span>x</span></div>";
        assert_eq!(to_xhtml(s), s);
    }

    #[test]
    fn nbsp_becomes_numeric_but_xml_entities_survive() {
        assert_eq!(to_xhtml("<p>a&nbsp;b</p>"), "<p>a&#160;b</p>");
        assert_eq!(
            to_xhtml("<p>a &amp; b &lt;c&gt;</p>"),
            "<p>a &amp; b &lt;c&gt;</p>"
        );
    }

    #[test]
    fn chapters_contain_only_entities_xhtml_defines() {
        let c = chapter(&article("Tom & Jerry"));
        assert_eq!(illegal_entities(&c), Vec::<String>::new(), "in:\n{c}");
    }

    #[test]
    fn titles_are_escaped_into_the_chapter() {
        let c = chapter(&article("Tom & Jerry <script>"));
        assert!(c.contains("Tom &amp; Jerry &lt;script&gt;"));
        assert!(!c.contains("<script>"));
    }

    #[test]
    fn builds_a_nonempty_zip() {
        let bytes = build(&[article("A Post")], "A Post").unwrap();
        assert!(bytes.len() > 500);
        assert_eq!(&bytes[..2], b"PK", "epub is a zip");
    }

    #[test]
    fn builds_a_multi_article_epub() {
        let bytes = build(&[article("One"), article("Two")], "Unread").unwrap();
        assert_eq!(&bytes[..2], b"PK");
    }

    #[test]
    fn refuses_an_empty_export() {
        assert!(build(&[], "Empty").is_err());
    }

    #[test]
    fn filenames_are_slugified() {
        assert_eq!(
            safe_filename("Al-Qadim: Master of None"),
            "Al-Qadim-Master-of-None"
        );
        assert_eq!(safe_filename("../../etc/passwd"), "etc-passwd");
        assert_eq!(safe_filename("???"), "article");
    }
}
