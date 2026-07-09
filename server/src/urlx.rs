//! URL normalization and the "is this a top-level article?" heuristic.
//!
//! Two jobs, both fiddly:
//!
//! 1. `dedupe_key` collapses the many spellings of one article into a single
//!    string. Feeds hand us `http://blog.example.com/2026/07/post.html` while
//!    the page itself redirects to `https://www.blog.example.com/2026/07/post.html?m=1`,
//!    and the index page links to it with `#comments` on the end. All one post.
//!
//! 2. `looks_like_article` decides which of the ~600 links on a blog's front
//!    page are posts, as opposed to labels, archives, comment permalinks, and
//!    image attachments. See the tests for the cases that motivated each rule.

use regex::Regex;
use std::sync::LazyLock;
use url::Url;

/// Query parameters that never change which document you get back.
const JUNK_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "utm_id",
    "fbclid",
    "gclid",
    "mc_cid",
    "mc_eid",
    "ref",
    "source",
    "m",           // Blogger's mobile flag
    "showcomment", // Blogger comment permalinks
    "replytocom",  // WordPress comment replies
];

/// Path segments that mark an index, not an article.
const PATH_BLOCKLIST: &[&str] = &[
    "/search",
    "/label/",
    "/tag/",
    "/tags/",
    "/category/",
    "/categories/",
    "/author/",
    "/page/",
    "/feed",
    "/feeds/",
    "/comment",
    "/wp-content/",
    "/wp-json/",
    "/wp-admin/",
    "/wp-includes/",
    "/cdn-cgi/",
    "/xmlrpc",
    "/privacy",
    "/about",
    "/contact",
];

const EXT_BLOCKLIST: &[&str] = &[
    ".jpg", ".jpeg", ".png", ".gif", ".webp", ".svg", ".bmp", ".ico", ".pdf", ".zip", ".gz",
    ".xml", ".rss", ".atom", ".json", ".css", ".js", ".mp3", ".mp4", ".m4a", ".ogg", ".wav",
];

/// A dated permalink: `/2026/07/some-slug`, `/2026/07/some-slug.html`, or
/// `/2026/07/03/some-slug/`. Both Blogger and WordPress default to this shape,
/// and it is what separates a post from `/2016/06/slug/attachment-name/`.
static DATE_PERMALINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(19|20)\d{2}/\d{1,2}(/\d{1,2})?/[^/]+/?$").unwrap());

fn last_segment(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// `/2026/07/03/` is WordPress's day archive, and every post's date links to
/// it. The regex above happily reads `03` as a slug, so demand that a slug
/// contain something other than digits.
fn is_date_permalink(path: &str) -> bool {
    DATE_PERMALINK.is_match(path) && last_segment(path).chars().any(|c| !c.is_ascii_digit())
}

/// A bare slug with enough words to be a title: `/the-life-and-times-of-maxis`.
static SLUG_PERMALINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/[a-z0-9]+(-[a-z0-9]+){2,}/?$").unwrap());

/// Strip `www.` so `filfre.net` and `www.filfre.net` compare equal.
pub fn registrable_host(u: &Url) -> String {
    u.host_str()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_ascii_lowercase()
}

pub fn same_site(a: &Url, b: &Url) -> bool {
    let (ha, hb) = (registrable_host(a), registrable_host(b));
    !ha.is_empty() && ha == hb
}

/// A stable identity for one article, ignoring scheme, `www.`, tracking
/// parameters, comment anchors, and trailing slashes.
pub fn dedupe_key(u: &Url) -> String {
    let host = registrable_host(u);
    let mut path = u.path().to_string();
    if let Some(stripped) = path.strip_suffix("/index.html") {
        path = format!("{stripped}/");
    }
    if path.len() > 1 {
        path = path.trim_end_matches('/').to_string();
    }

    let mut params: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !JUNK_PARAMS.contains(&k.to_ascii_lowercase().as_str()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    params.sort();

    let mut key = format!("{host}{path}");
    if !params.is_empty() {
        let q: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
        key.push('?');
        key.push_str(&q.join("&"));
    }
    key
}

/// The URL we show and re-fetch: junk params dropped, fragment dropped, but
/// scheme and host preserved as the site actually serves them.
pub fn canonicalize(u: &Url) -> Url {
    let mut out = u.clone();
    out.set_fragment(None);
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !JUNK_PARAMS.contains(&k.to_ascii_lowercase().as_str()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        out.set_query(None);
    } else {
        let mut qs = out.query_pairs_mut();
        qs.clear();
        for (k, v) in kept {
            qs.append_pair(&k, &v);
        }
        drop(qs);
    }
    out
}

/// Structural disqualifiers, applied whether or not the site has a custom
/// pattern: wrong host, wrong scheme, an index page, or a static asset.
pub fn is_plausible_target(candidate: &Url, site: &Url) -> bool {
    if !matches!(candidate.scheme(), "http" | "https") {
        return false;
    }
    if !same_site(candidate, site) {
        return false;
    }
    let path = candidate.path().to_ascii_lowercase();
    if path.is_empty() || path == "/" {
        return false;
    }
    if PATH_BLOCKLIST.iter().any(|p| path.contains(p)) {
        return false;
    }
    if EXT_BLOCKLIST.iter().any(|e| path.ends_with(e)) {
        return false;
    }
    true
}

/// Our built-in guess at a top-level post, used when a site defines no
/// `url_pattern` of its own.
pub fn looks_like_article(candidate: &Url, site: &Url) -> bool {
    if !is_plausible_target(candidate, site) {
        return false;
    }
    let path = candidate.path();
    is_date_permalink(path) || SLUG_PERMALINK.is_match(path)
}

/// Both seed platforms use `/YYYY/MM/` permalinks, so when a page publishes no
/// machine-readable date we can still place it in the right month.
pub fn date_from_path(u: &Url) -> Option<chrono::NaiveDate> {
    static P: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^/((?:19|20)\d{2})/(\d{1,2})(?:/(\d{1,2}))?/").unwrap());
    let c = P.captures(u.path())?;
    let y = c.get(1)?.as_str().parse().ok()?;
    let m = c.get(2)?.as_str().parse().ok()?;
    let d = c.get(3).and_then(|x| x.as_str().parse().ok()).unwrap_or(1);
    chrono::NaiveDate::from_ymd_opt(y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn dedupe_key_collapses_the_spellings_of_one_post() {
        let canonical = dedupe_key(&u("https://crpgaddict.blogspot.com/2026/07/al-qadim.html"));
        // Feeds hand out http:// for Blogger.
        assert_eq!(
            canonical,
            dedupe_key(&u("http://crpgaddict.blogspot.com/2026/07/al-qadim.html"))
        );
        // Index pages link with comment anchors and mobile flags.
        assert_eq!(
            canonical,
            dedupe_key(&u(
                "https://crpgaddict.blogspot.com/2026/07/al-qadim.html?showComment=178#c58"
            ))
        );
        assert_eq!(
            canonical,
            dedupe_key(&u(
                "https://crpgaddict.blogspot.com/2026/07/al-qadim.html?m=1"
            ))
        );
        // www. and trailing slashes are noise.
        assert_eq!(
            dedupe_key(&u("https://www.filfre.net/2026/07/maxis/")),
            dedupe_key(&u("https://filfre.net/2026/07/maxis"))
        );
        // Tracking params don't identify a document.
        assert_eq!(
            dedupe_key(&u(
                "https://filfre.net/2026/07/maxis?utm_source=rss&fbclid=x"
            )),
            dedupe_key(&u("https://filfre.net/2026/07/maxis"))
        );
    }

    #[test]
    fn dedupe_key_keeps_meaningful_query_params() {
        assert_ne!(
            dedupe_key(&u("https://ex.com/p?id=1")),
            dedupe_key(&u("https://ex.com/p?id=2"))
        );
        // ...and is order-insensitive about them.
        assert_eq!(
            dedupe_key(&u("https://ex.com/p?b=2&a=1")),
            dedupe_key(&u("https://ex.com/p?a=1&b=2"))
        );
    }

    #[test]
    fn canonicalize_drops_junk_but_keeps_the_document() {
        assert_eq!(
            canonicalize(&u(
                "https://filfre.net/2026/07/maxis/?utm_source=rss#comments"
            ))
            .as_str(),
            "https://filfre.net/2026/07/maxis/"
        );
        assert_eq!(
            canonicalize(&u("https://ex.com/p?id=7&utm_medium=x")).as_str(),
            "https://ex.com/p?id=7"
        );
    }

    #[test]
    fn accepts_real_posts_from_the_seed_blogs() {
        let blogger = u("https://crpgaddict.blogspot.com/");
        assert!(looks_like_article(
            &u("https://crpgaddict.blogspot.com/2026/07/al-qadim-genies-betrayal.html"),
            &blogger
        ));
        let wp = u("https://www.filfre.net/");
        assert!(looks_like_article(
            &u("https://www.filfre.net/2026/07/the-life-and-times-of-maxis-part-1-simeverything/"),
            &wp
        ));
        // WordPress installs that include the day.
        assert!(looks_like_article(
            &u("https://www.filfre.net/2020/06/12/doom/"),
            &wp
        ));
        // Undated blogs with wordy slugs.
        assert!(looks_like_article(
            &u("https://www.filfre.net/the-shareware-scene-part-4"),
            &wp
        ));
    }

    #[test]
    fn rejects_the_junk_that_shares_a_front_page_with_them() {
        let blogger = u("https://crpgaddict.blogspot.com/");
        let wp = u("https://www.filfre.net/");

        // Attachment pages sit one segment deeper than the post.
        assert!(!looks_like_article(
            &u("https://www.filfre.net/2016/06/simcity-part-1/118655-simcity-front-cover/"),
            &wp
        ));
        // Label, archive, and search indexes.
        assert!(!looks_like_article(
            &u("https://crpgaddict.blogspot.com/search/label/RPG"),
            &blogger
        ));
        assert!(!looks_like_article(
            &u("https://www.filfre.net/category/games/"),
            &wp
        ));
        assert!(!looks_like_article(
            &u("https://www.filfre.net/page/3/"),
            &wp
        ));
        // Feeds.
        assert!(!looks_like_article(&u("https://www.filfre.net/feed/"), &wp));
        assert!(!looks_like_article(
            &u("https://crpgaddict.blogspot.com/feeds/posts/default"),
            &blogger
        ));
        // The homepage itself.
        assert!(!looks_like_article(&u("https://www.filfre.net/"), &wp));
        // Assets.
        assert!(!looks_like_article(
            &u("https://www.filfre.net/2026/07/cover.jpg"),
            &wp
        ));
        // Short slugs are nav, not posts.
        assert!(!looks_like_article(
            &u("https://www.filfre.net/about-me"),
            &wp
        ));
    }

    /// Every WordPress post links its own date to a day archive that serves the
    /// post's full text. Crawling it produced a perfect duplicate article.
    #[test]
    fn rejects_date_archive_pages() {
        let wp = u("https://www.filfre.net/");
        assert!(!looks_like_article(
            &u("https://www.filfre.net/2026/07/03/"),
            &wp
        ));
        assert!(!looks_like_article(
            &u("https://www.filfre.net/2026/07/03"),
            &wp
        ));
        assert!(!looks_like_article(
            &u("https://www.filfre.net/2026/07/"),
            &wp
        ));
        assert!(!looks_like_article(
            &u("https://www.filfre.net/2026/07"),
            &wp
        ));
        // ...while a post published on that day is still a post.
        assert!(looks_like_article(
            &u("https://www.filfre.net/2026/07/03/some-post/"),
            &wp
        ));
        assert!(looks_like_article(
            &u("https://www.filfre.net/2026/07/some-post/"),
            &wp
        ));
        // A slug that merely starts with digits is fine.
        assert!(looks_like_article(
            &u("https://www.filfre.net/2026/07/1993-in-review/"),
            &wp
        ));
    }

    #[test]
    fn never_leaves_the_site() {
        let wp = u("https://www.filfre.net/");
        assert!(!looks_like_article(
            &u("https://twitter.com/2026/07/some-post"),
            &wp
        ));
        assert!(!looks_like_article(
            &u("https://evil.net/2026/07/some-post"),
            &wp
        ));
        // ...but www. is the same site.
        assert!(looks_like_article(
            &u("https://filfre.net/2026/07/maxis-part-1/"),
            &wp
        ));
        // ...and a subdomain is not.
        assert!(!looks_like_article(
            &u("https://cdn.filfre.net/2026/07/maxis-part-1/"),
            &wp
        ));
    }

    #[test]
    fn is_plausible_target_gates_custom_patterns_too() {
        let wp = u("https://www.filfre.net/");
        // A permissive user pattern still must not escape the site or fetch assets.
        assert!(!is_plausible_target(
            &u("https://elsewhere.com/anything"),
            &wp
        ));
        assert!(!is_plausible_target(
            &u("https://filfre.net/img/a.png"),
            &wp
        ));
        assert!(!is_plausible_target(&u("ftp://filfre.net/x"), &wp));
        // But an oddly-shaped post path is fine — that's what patterns are for.
        assert!(is_plausible_target(
            &u("https://filfre.net/archives?p=1234"),
            &wp
        ));
    }

    #[test]
    fn recovers_dates_from_permalinks() {
        use chrono::Datelike;
        let d = date_from_path(&u("https://filfre.net/2026/07/maxis/")).unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2026, 7, 1));
        let d = date_from_path(&u("https://filfre.net/2020/06/12/doom/")).unwrap();
        assert_eq!((d.year(), d.month(), d.day()), (2020, 6, 12));
        assert!(date_from_path(&u("https://filfre.net/about")).is_none());
    }
}
