//! MOBI export.
//!
//! The reading device is a Kindle, whose browser takes a `.mobi` download but
//! can't run feedbot's SPA. Amazon dropped MOBI from Send-to-Kindle, but a
//! sideloaded `.mobi` is the format its browser actually opens, so this is how
//! an article leaves the screen and lands on the device.
//!
//! It **embeds images**: the stored HTML keeps remote `<img src>` URLs, which
//! render as nothing on an offline reader, so we fetch each one through the
//! sidecar (the single network chokepoint) and staple the bytes in as MOBI
//! image records.

use crate::db::{self, Article, Pool};
use crate::fetcher::Fetcher;
use crate::images;
use anyhow::{Context, Result};
use iepub::prelude::{MobiBuilder, MobiHtml, MobiNav};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

// ---------------------------------------------------------------------------
// XHTML helpers
// ---------------------------------------------------------------------------

/// MOBI's chapter markup is XML-ish, but Ammonia (which sanitizes the stored
/// HTML) emits HTML5 with unclosed void tags — `<br>`, not `<br/>`.
static VOID_TAG: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)<(area|base|br|col|embed|hr|img|input|link|meta|param|source|track|wbr)\b([^>]*?)\s*/?>",
    )
    .unwrap()
});

/// Close the void elements Ammonia leaves open, and spell U+00A0 numerically:
/// XHTML defines only `amp/lt/gt/quot/apos`, and a reader's XML parser rejects a
/// whole chapter over one `&nbsp;`.
fn to_xhtml(html: &str) -> String {
    VOID_TAG
        .replace_all(html, "<$1$2/>")
        .replace("&nbsp;", "&#160;")
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

/// A whole-queue export shouldn't embed a half-gigabyte of images; past this we
/// stop embedding and leave the rest as remote links. Images are already
/// transcoded to [`images::MAX_IMG_BYTES`] at capture time, so this only bounds
/// the total, not any single picture.
const IMAGE_BUDGET_BYTES: usize = 48 * 1024 * 1024;
/// How many chapters assemble at once. Enough to overlap the occasional
/// on-demand image capture (for a post crawled before capture existed) without
/// hammering a single blog's CDN.
const BUILD_CONCURRENCY: usize = 4;

/// How far a [`build`] has gotten, for a caller that wants to show progress on a
/// long export. Reported once per article as its images are fetched and
/// embedded, then once when the (unsplittable) MOBI assembly begins.
#[derive(Clone, Copy, Debug)]
pub enum Progress {
    Article { done: usize, total: usize },
    Assembling,
}

/// A progress sink that discards everything, for callers with nothing to show.
pub fn no_progress(_: Progress) {}

/// One article turned into a MOBI chapter: an XHTML fragment (no `<html>`
/// wrapper — iepub wants only the body) plus the image assets it references.
struct Chapter {
    title: String,
    html: String,
    assets: Vec<(String, Vec<u8>)>,
}

/// Byline · date · site, shown under each post's heading.
fn meta_line(a: &Article) -> String {
    let mut parts = Vec::new();
    if let Some(b) = &a.byline {
        parts.push(escape(b));
    }
    if let Some(ts) = a.published_at
        && let Some(dt) = chrono::DateTime::from_timestamp(ts, 0)
    {
        parts.push(dt.format("%-d %B %Y").to_string());
    }
    parts.push(escape(&a.site_name));
    parts.join(" · ")
}

/// Reserve `len` bytes from the shared budget, returning false (and reserving
/// nothing) if they don't fit. A compare-and-swap loop, so chapters racing to
/// embed images can't oversubscribe it.
fn try_reserve(budget: &AtomicUsize, len: usize) -> bool {
    let mut cur = budget.load(Ordering::Relaxed);
    loop {
        if cur < len {
            return false;
        }
        match budget.compare_exchange_weak(cur, cur - len, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => cur = actual,
        }
    }
}

/// Turn `a` into a chapter, embedding its captured images from the store and
/// spending from a shared byte budget. Most articles already carry
/// `/img/<hash>` refs from crawl time, so this is a pair of DB reads; one
/// crawled before capture existed is captured on demand here (fetched and
/// transcoded through the sidecar), which the background backfill also does.
async fn build_chapter(
    a: &Article,
    prefix: &str,
    pool: &Pool,
    fetch: &Fetcher,
    budget: &AtomicUsize,
) -> Chapter {
    let raw = a.content_html.clone().unwrap_or_default();
    // Pull any still-remote images local (no-op once an article is captured),
    // then load the captured bytes to staple into the book.
    let mut html = images::capture_html(pool, fetch, &raw, &a.url).await;

    let srcs = images::image_srcs(&html);
    let hashes: Vec<String> = srcs
        .iter()
        .filter_map(|s| images::local_hash(s).map(str::to_string))
        .collect();
    let blobs = if hashes.is_empty() {
        HashMap::new()
    } else {
        db::call(pool, move |c| db::image_bytes_many(c, &hashes))
            .await
            .unwrap_or_default()
    };

    let mut assets = Vec::new();
    for (i, src) in srcs.iter().enumerate() {
        let Some(hash) = images::local_hash(src) else {
            continue; // a remote src whose capture failed; leave it, alt text shows
        };
        let Some(bytes) = blobs.get(hash) else { continue };
        if !try_reserve(budget, bytes.len()) {
            tracing::warn!("mobi image budget spent; leaving remaining images remote");
            break;
        }
        let name = format!("{prefix}_{i}.jpg");
        // iepub keys assets to the chapter's img@src by exact string, so the
        // rewritten src must equal the asset file name.
        html = html.replace(&format!("src=\"{src}\""), &format!("src=\"{name}\""));
        assets.push((name, bytes.clone()));
    }

    let fragment = format!(
        "<h1>{title}</h1>\n<p>{meta}</p>\n{body}\n<hr/>\n<p><a href=\"{url}\">{url}</a></p>",
        title = escape(&a.title),
        meta = meta_line(a),
        body = to_xhtml(&html),
        url = escape(&a.url),
    );
    Chapter {
        title: a.title.clone(),
        html: fragment,
        assets,
    }
}

/// A plain cover plate — iepub refuses to write a MOBI without a cover, and a
/// real screenshot from the article is a better thumbnail than a blank. Falls
/// back to a solid tile when the article has no usable image.
fn cover_from(first_image: Option<&[u8]>) -> Vec<u8> {
    if let Some(jpeg) = first_image {
        return jpeg.to_vec();
    }
    let mut plate = image::RgbImage::new(600, 800);
    for (x, y, px) in plate.enumerate_pixels_mut() {
        let border = x < 16 || y < 16 || x >= 584 || y >= 784;
        // feedbot's paper, with an ink border.
        *px = if border { image::Rgb([28, 26, 23]) } else { image::Rgb([250, 247, 240]) };
    }
    let mut out = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    plate
        .write_with_encoder(encoder)
        .expect("encoding cover plate");
    out.into_inner()
}

/// Article indices grouped by site — sites in the order their first article
/// appears (so the recency-sorted input keeps newest sites first), and articles
/// in input order within each site. This is what makes the export read as one
/// section per blog.
fn group_by_site(articles: &[Article]) -> Vec<(i64, Vec<usize>)> {
    let mut order: Vec<i64> = Vec::new();
    let mut groups: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, a) in articles.iter().enumerate() {
        if !groups.contains_key(&a.site_id) {
            order.push(a.site_id);
        }
        groups.entry(a.site_id).or_default().push(i);
    }
    order.into_iter().map(|sid| (sid, groups.remove(&sid).unwrap())).collect()
}

/// A built chapter, carrying the site it belongs to so the nav can group it.
struct Built {
    chap_id: usize,
    site_name: String,
    title: String,
    html: String,
    assets: Vec<(String, Vec<u8>)>,
}

/// Build one `.mobi` from the given articles, images embedded. A multi-article
/// export is organized one section per site, each site's posts nested beneath
/// it in the table of contents; a single article is just itself.
pub async fn build(
    articles: &[Article],
    title: &str,
    pool: &Pool,
    fetch: &Fetcher,
    on_progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<Vec<u8>> {
    anyhow::ensure!(!articles.is_empty(), "nothing to export");

    let groups = group_by_site(articles);

    // Assign chapter ids in grouped order — the text stream and the nav must
    // share this order — but build them concurrently and slot each back into
    // place as it finishes, so a slow image host stalls only its own chapter.
    let mut order: Vec<(usize, usize)> = Vec::new();
    let mut chap_id = 0usize;
    for (_sid, idxs) in &groups {
        for &i in idxs {
            chap_id += 1;
            order.push((chap_id, i));
        }
    }
    let total = order.len();

    let arts: Arc<Vec<Article>> = Arc::new(articles.to_vec());
    let budget = Arc::new(AtomicUsize::new(IMAGE_BUDGET_BYTES));
    let sem = Arc::new(Semaphore::new(BUILD_CONCURRENCY));
    let mut set = JoinSet::new();
    for (cid, ai) in order {
        let (arts, pool, fetch, budget, sem) =
            (arts.clone(), pool.clone(), fetch.clone(), budget.clone(), sem.clone());
        set.spawn(async move {
            let _permit = sem.acquire_owned().await.expect("build semaphore closed");
            let a = &arts[ai];
            let ch = build_chapter(a, &format!("img{cid}"), &pool, &fetch, &budget).await;
            (
                cid,
                Built {
                    chap_id: cid,
                    site_name: a.site_name.clone(),
                    title: ch.title,
                    html: ch.html,
                    assets: ch.assets,
                },
            )
        });
    }

    let mut slots: Vec<Option<Built>> = (0..total).map(|_| None).collect();
    let mut done = 0usize;
    while let Some(res) = set.join_next().await {
        let (cid, chapter) = res.map_err(|e| anyhow::anyhow!("chapter task failed: {e}"))?;
        done += 1;
        on_progress(Progress::Article { done, total });
        slots[cid - 1] = Some(chapter);
    }
    let built: Vec<Built> = slots
        .into_iter()
        .map(|b| b.expect("every chapter slot filled"))
        .collect();

    let author = articles
        .iter()
        .find_map(|a| a.byline.clone())
        .unwrap_or_else(|| articles[0].site_name.clone());
    let first_image = built
        .iter()
        .flat_map(|c| c.assets.first())
        .map(|(_, bytes)| bytes.as_slice())
        .next();

    // Site → post nesting is only meaningful past a single article; iepub's
    // auto TOC is right for one. Build the nav from chapter metadata *before*
    // the chapters are moved into the builder below.
    let multi = built.len() > 1;
    let navs = multi.then(|| build_nav(&groups, &built));

    // Everything is fetched; the rest is serializing the book, which for a
    // whole-queue export is a long, unsplittable step worth flagging.
    on_progress(Progress::Assembling);

    let mut b = MobiBuilder::new()
        .with_title(title)
        .with_creator(author)
        .with_identifier("feedbot")
        .append_title(false) // each chapter fragment carries its own <h1>
        .custome_nav(multi) // false lets iepub build the trivial one-post TOC
        .cover(cover_from(first_image));

    for ch in built {
        for (name, bytes) in ch.assets {
            b = b.add_assets(name, bytes);
        }
        b = b.add_chapter(
            MobiHtml::new(ch.chap_id)
                .with_title(ch.title)
                .with_data(ch.html.into_bytes()),
        );
    }
    for nav in navs.into_iter().flatten() {
        b = b.add_nav(nav);
    }

    let bytes = b.mem().map_err(|e| anyhow::anyhow!("{e}")).context("generating mobi")?;

    // iepub writes only an inline HTML TOC; splice in the NCX index a Kindle
    // reads for its native chapter list. Best-effort — a book without the index
    // still opens fine, so a failure here must not sink the export.
    Ok(match crate::ncx::add_toc(&bytes) {
        Ok(with_toc) => with_toc,
        Err(e) => {
            tracing::warn!("mobi built without a Kindle chapter index: {e:#}");
            bytes
        }
    })
}

/// A two-level table of contents: one parent per site (pointing at its first
/// post, so tapping the site name jumps there) with a child per post. iepub
/// resolves each nav's `chap_id` against the matching chapter's id; the nav's
/// own id only has to be unique, so a running counter does.
fn build_nav(groups: &[(i64, Vec<usize>)], built: &[Built]) -> Vec<MobiNav> {
    let mut nav_id = 1000usize;
    let mut chapters = built.iter();
    let mut navs = Vec::with_capacity(groups.len());
    for (_sid, idxs) in groups {
        // `built` is in the same grouped order, so the next `idxs.len()`
        // chapters are exactly this site's posts.
        let posts: Vec<&Built> = chapters.by_ref().take(idxs.len()).collect();
        nav_id += 1;
        let mut parent = MobiNav::new(nav_id, posts[0].chap_id).with_title(posts[0].site_name.clone());
        for p in &posts {
            nav_id += 1;
            parent.add_child(MobiNav::new(nav_id, p.chap_id).with_title(p.title.clone()));
        }
        navs.push(parent);
    }
    navs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_plate_is_a_jpeg_when_no_image() {
        let c = cover_from(None);
        assert_eq!(&c[..3], &[0xFF, 0xD8, 0xFF]);
    }

    fn at_site(id: i64) -> Article {
        Article {
            id: 0,
            site_id: id,
            site_name: format!("site {id}"),
            url: String::new(),
            title: String::new(),
            byline: None,
            excerpt: None,
            word_count: 0,
            published_at: None,
            fetched_at: 0,
            read_at: None,
            starred: false,
            content_html: None,
        }
    }

    #[test]
    fn grouping_keeps_first_seen_site_order_and_within_site_order() {
        // Recency-sorted input interleaves sites; grouping must gather each
        // site's posts while keeping the site whose newest post came first up top.
        let arts = [at_site(2), at_site(1), at_site(2), at_site(3), at_site(1)];
        let groups = group_by_site(&arts);
        assert_eq!(
            groups,
            vec![(2, vec![0, 2]), (1, vec![1, 4]), (3, vec![3])]
        );
    }

    #[test]
    fn grouping_a_single_site_is_one_group() {
        let arts = [at_site(7), at_site(7)];
        assert_eq!(group_by_site(&arts), vec![(7, vec![0, 1])]);
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
    fn nbsp_becomes_numeric_but_xml_entities_survive() {
        assert_eq!(to_xhtml("<p>a&nbsp;b</p>"), "<p>a&#160;b</p>");
        assert_eq!(to_xhtml("<p>a &amp; b &lt;c&gt;</p>"), "<p>a &amp; b &lt;c&gt;</p>");
    }

    #[test]
    fn escapes_the_five_xml_metacharacters() {
        assert_eq!(escape(r#"Tom & "Jerry" <b>"#), "Tom &amp; &quot;Jerry&quot; &lt;b&gt;");
    }

    #[test]
    fn filenames_are_slugified() {
        assert_eq!(safe_filename("Al-Qadim: Master of None!"), "Al-Qadim-Master-of-None");
        assert_eq!(safe_filename("   "), "article");
    }
}
