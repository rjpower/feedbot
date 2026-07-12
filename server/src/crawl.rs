//! The crawl policy: find a site's articles, fetch the new ones, store them.
//!
//! Discovery unions two sources, because neither alone is sufficient:
//!
//!   * The **feed**, when there is one. Clean titles and real publication
//!     dates — but a blog can cap it. crpgaddict's Atom feed serves 3 posts.
//!   * The **index page's links**, filtered by `url_pattern` or by the built-in
//!     permalink heuristic. Deep backlog, no metadata, lots of noise.
//!
//! Candidates are then sorted newest-first (using the date in the permalink
//! when nothing better exists) so that the first crawl of a decade-old blog
//! picks up this month's posts rather than 25 arbitrary ones from 2011.

use crate::db::{self, NewArticle, Pool, Site};
use crate::fetcher::Fetcher;
use crate::urlx;
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use url::Url;

pub struct Crawler {
    pool: Pool,
    fetcher: Fetcher,
    /// Politeness pause between article fetches on the same host.
    delay: Duration,
    /// One crawl at a time: there is one browser behind the sidecar, and we
    /// would rather be slow than be rude.
    permit: Arc<Semaphore>,
    in_flight: InFlight,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct CrawlSummary {
    pub discovered: i64,
    pub added: i64,
    pub failed: i64,
}

#[derive(Debug)]
pub enum CrawlOutcome {
    Ran(CrawlSummary),
    AlreadyRunning,
}

/// The set of sites that are crawling *or waiting to*.
///
/// `last_crawled_at` is only stamped when a crawl finishes, so a site queued
/// behind the semaphore still looks due to the scheduler. Without this, a slow
/// first crawl gets a redundant second one enqueued right behind it.
#[derive(Clone, Default)]
struct InFlight(Arc<std::sync::Mutex<HashSet<i64>>>);

/// Releases the site when the crawl ends, however it ends.
struct Claim {
    set: InFlight,
    id: i64,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.set
            .0
            .lock()
            .expect("in-flight lock poisoned")
            .remove(&self.id);
    }
}

impl InFlight {
    fn claim(&self, id: i64) -> Option<Claim> {
        let mut set = self.0.lock().expect("in-flight lock poisoned");
        set.insert(id).then(|| Claim {
            set: self.clone(),
            id,
        })
    }

    fn contains(&self, id: i64) -> bool {
        self.0
            .lock()
            .expect("in-flight lock poisoned")
            .contains(&id)
    }
}

/// One article we might want, before we know whether we already have it.
#[derive(Debug)]
struct Candidate {
    url: Url,
    key: String,
    /// From the feed, when the feed had one.
    title: Option<String>,
    published_at: Option<i64>,
    /// Sort key: best guess at recency, from the feed or the permalink.
    recency: i64,
}

impl Crawler {
    pub fn new(pool: Pool, fetcher: Fetcher, delay: Duration) -> Self {
        Self {
            pool,
            fetcher,
            delay,
            permit: Arc::new(Semaphore::new(1)),
            in_flight: InFlight::default(),
        }
    }

    /// Is this site crawling, or queued to?
    pub fn is_running(&self, site_id: i64) -> bool {
        self.in_flight.contains(site_id)
    }

    /// Crawl one site end to end, recording a `crawls` row either way.
    pub async fn crawl_site(&self, site_id: i64) -> Result<CrawlOutcome> {
        // Claim before waiting on the semaphore, so a site queued behind
        // another site's crawl is already visible as in-flight.
        let Some(_claim) = self.in_flight.claim(site_id) else {
            tracing::info!(site_id, "crawl skipped: already running");
            return Ok(CrawlOutcome::AlreadyRunning);
        };
        let _guard = self.permit.acquire().await.expect("semaphore never closed");

        let site = db::call(&self.pool, move |c| db::get_site(c, site_id))
            .await?
            .with_context(|| format!("no site {site_id}"))?;

        let crawl_id = db::call(&self.pool, move |c| db::start_crawl(c, site_id)).await?;
        tracing::info!(site = %site.name, "crawl starting");

        let result = self.run(&site).await;

        let (summary, error) = match result {
            Ok(s) => (s, None),
            Err(e) => {
                tracing::warn!(site = %site.name, "crawl failed: {e:#}");
                (CrawlSummary::default(), Some(format!("{e:#}")))
            }
        };

        let (d, a, err) = (summary.discovered, summary.added, error.clone());
        db::call(&self.pool, move |c| {
            db::finish_crawl(c, crawl_id, d, a, err)?;
            // Stamp the site even on failure: a site that always errors should
            // wait out its interval rather than retry every scheduler tick.
            db::mark_crawled(c, site_id, db::now())
        })
        .await?;

        match error {
            Some(e) => Err(anyhow::anyhow!(e)),
            None => {
                tracing::info!(site = %site.name, discovered = summary.discovered,
                               added = summary.added, failed = summary.failed, "crawl done");
                Ok(CrawlOutcome::Ran(summary))
            }
        }
    }

    async fn run(&self, site: &Site) -> Result<CrawlSummary> {
        let base = Url::parse(&site.url).with_context(|| format!("bad site url {}", site.url))?;
        let pattern = site
            .url_pattern
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .map(|p| Regex::new(p).with_context(|| format!("bad url_pattern {p:?}")))
            .transpose()?;

        let candidates = self.gather(site, &base, pattern.as_ref()).await?;
        let discovered = candidates.len() as i64;

        let site_id = site.id;
        let known = db::call(&self.pool, move |c| db::known_keys(c, site_id)).await?;

        let mut fresh: Vec<Candidate> = candidates
            .into_iter()
            .filter(|c| !known.contains(&c.key))
            .collect();
        fresh.sort_by_key(|c| std::cmp::Reverse(c.recency));
        fresh.truncate(site.max_new_per_crawl.max(0) as usize);

        if fresh.is_empty() {
            return Ok(CrawlSummary {
                discovered,
                added: 0,
                failed: 0,
            });
        }
        tracing::info!(site = %site.name, "fetching {} new articles", fresh.len());

        let mut summary = CrawlSummary {
            discovered,
            added: 0,
            failed: 0,
        };
        for (i, cand) in fresh.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(self.delay).await;
            }
            match self.ingest(site, cand).await {
                Ok(true) => summary.added += 1,
                Ok(false) => {} // raced with another crawl; already stored
                Err(e) => {
                    summary.failed += 1;
                    tracing::warn!(url = %cand.url, "article failed: {e:#}");
                }
            }
        }
        Ok(summary)
    }

    /// Union of feed entries and index-page links, deduped by `url_key`.
    async fn gather(
        &self,
        site: &Site,
        base: &Url,
        pattern: Option<&Regex>,
    ) -> Result<Vec<Candidate>> {
        let mut out: HashMap<String, Candidate> = HashMap::new();

        // --- index page: links + feed autodiscovery ---
        let disc = self.fetcher.discover(&site.url).await;
        let mut feed_url = site.feed_url.clone().filter(|f| !f.trim().is_empty());

        match &disc {
            Ok(d) => {
                if feed_url.is_none()
                    && let Some(found) = d.feeds.first()
                {
                    tracing::info!(site = %site.name, "discovered feed {found}");
                    let (id, f) = (site.id, found.clone());
                    db::call(&self.pool, move |c| db::set_feed_url(c, id, &f)).await?;
                    feed_url = Some(found.clone());
                }
                // A site added by URL alone is named after its host. The
                // homepage's own <title> is a better name, so adopt it once.
                if let Some(better) = better_name(&site.name, base, &d.title) {
                    tracing::info!(site = %site.name, "renaming to {better:?}");
                    let id = site.id;
                    db::call(&self.pool, move |c| db::rename_site(c, id, &better)).await?;
                }
                for link in &d.links {
                    let Ok(u) = Url::parse(&link.href) else {
                        continue;
                    };
                    if !accepts(&u, base, pattern) {
                        continue;
                    }
                    insert_candidate(&mut out, u, None, None);
                }
            }
            Err(e) => tracing::warn!(site = %site.name, "index page failed: {e:#}"),
        }

        // --- feed entries ---
        if let Some(feed) = &feed_url {
            match self.fetcher.feed(feed).await {
                Ok(f) => {
                    for entry in f.entries {
                        let Ok(u) = Url::parse(&entry.url) else {
                            continue;
                        };
                        // A feed is an assertion that these are the site's
                        // posts, so `url_pattern` and the permalink heuristic
                        // don't get a vote — but the safety gate still does.
                        if !urlx::is_plausible_target(&u, base) {
                            continue;
                        }
                        let ts = entry.published.as_deref().and_then(parse_time);
                        insert_candidate(&mut out, u, entry.title, ts);
                    }
                }
                Err(e) => tracing::warn!(site = %site.name, feed = %feed, "feed failed: {e:#}"),
            }
        }

        // Both sources dead: surface it rather than reporting a cheerful "0 new".
        if out.is_empty()
            && let Err(e) = disc
        {
            return Err(e).context("no candidates: index page and feed both failed");
        }
        Ok(out.into_values().collect())
    }

    /// Fetch one article and store it. `false` means it was already there.
    async fn ingest(&self, site: &Site, cand: &Candidate) -> Result<bool> {
        let doc = self.fetcher.article(cand.url.as_str()).await?;

        // Follow redirects to wherever the article actually lives, but re-key
        // off the final URL so a redirect doesn't create a duplicate later.
        let final_url = Url::parse(&doc.final_url).unwrap_or_else(|_| cand.url.clone());
        let canonical = urlx::canonicalize(&final_url);
        let key = urlx::dedupe_key(&canonical);

        // Feed titles are clean; Readability's are scraped off the page.
        let title = cand
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .unwrap_or(doc.title.trim())
            .to_string();

        let published_at = cand
            .published_at
            .or_else(|| doc.published_time.as_deref().and_then(parse_time))
            .or_else(|| {
                urlx::date_from_path(&canonical)
                    .and_then(|d| d.and_hms_opt(12, 0, 0))
                    .map(|dt| dt.and_utc().timestamp())
            });

        // Untrusted HTML from the open internet, rendered into the reader with
        // v-html. Ammonia strips scripts, event handlers, and javascript: URLs.
        let content_html = ammonia::clean(&doc.html);

        // Pull every image local and content-addressed while we're already here:
        // the reader and the offline export both want them stored, not hotlinked.
        // Referer is the article itself, which hotlink-protected hosts want.
        let content_html =
            crate::images::capture_html(&self.pool, &self.fetcher, &content_html, canonical.as_str())
                .await;

        let row = NewArticle {
            site_id: site.id,
            url: canonical.to_string(),
            url_key: key,
            title: if title.is_empty() {
                canonical.to_string()
            } else {
                title
            },
            byline: doc.byline,
            excerpt: doc.excerpt,
            content_html,
            word_count: doc.word_count,
            published_at,
        };
        db::call(&self.pool, move |c| db::insert_article(c, &row)).await
    }
}

/// `Some(title)` when the site is still carrying its auto-derived hostname and
/// the homepage offers a real name. Never overrides a name a human chose.
fn better_name(current: &str, base: &Url, page_title: &str) -> Option<String> {
    if current != urlx::registrable_host(base) {
        return None;
    }
    let title = page_title.trim();
    if title.is_empty() || title.len() > 80 {
        return None;
    }
    Some(title.to_string())
}

/// Structural gate, then the site's pattern if it has one, else our heuristic.
fn accepts(candidate: &Url, base: &Url, pattern: Option<&Regex>) -> bool {
    match pattern {
        Some(re) => urlx::is_plausible_target(candidate, base) && re.is_match(candidate.as_str()),
        None => urlx::looks_like_article(candidate, base),
    }
}

/// Keep the richer of two sightings of the same article: the feed knows the
/// title and date, the index page only knows the link.
fn insert_candidate(
    out: &mut HashMap<String, Candidate>,
    url: Url,
    title: Option<String>,
    published_at: Option<i64>,
) {
    let canonical = urlx::canonicalize(&url);
    let key = urlx::dedupe_key(&canonical);
    let recency = published_at
        .or_else(|| {
            urlx::date_from_path(&canonical)
                .and_then(|d| d.and_hms_opt(12, 0, 0))
                .map(|dt| dt.and_utc().timestamp())
        })
        .unwrap_or(0);

    let entry = out.entry(key.clone()).or_insert(Candidate {
        url: canonical,
        key,
        title: None,
        published_at: None,
        recency,
    });
    if entry.title.is_none() {
        entry.title = title;
    }
    if entry.published_at.is_none() {
        entry.published_at = published_at;
    }
    entry.recency = entry.recency.max(recency);
}

/// Feeds and meta tags disagree about date formats. Accept the common ones.
fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return Some(dt.timestamp());
    }
    // Blogger sometimes emits a naive local timestamp.
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().timestamp());
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d.and_hms_opt(12, 0, 0)?.and_utc().timestamp());
    }
    None
}

/// Poll for due sites forever. One crawl at a time, courtesy of the semaphore.
pub fn schedule(crawler: Arc<Crawler>, tick: Duration) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            let due = match db::call(&crawler.pool, |c| db::due_sites(c, db::now())).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("scheduler could not read due sites: {e:#}");
                    continue;
                }
            };
            for site in due {
                // A site whose first crawl is still queued is due but busy.
                if crawler.is_running(site.id) {
                    continue;
                }
                if let Err(e) = crawler.crawl_site(site.id).await {
                    tracing::warn!(site = %site.name, "scheduled crawl failed: {e:#}");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parses_the_date_formats_the_seeds_emit() {
        // Blogger Atom, with an offset that must actually be applied.
        assert_eq!(
            parse_time("2026-07-08T00:00:00-04:00"),
            parse_time("2026-07-08T04:00:00Z"),
        );
        // WordPress RSS pubDate (RFC 2822).
        assert_eq!(
            parse_time("Fri, 26 Jun 2026 14:03:11 +0000"),
            parse_time("2026-06-26T14:03:11Z"),
        );
        // ISO with millis, from Date.toISOString().
        assert_eq!(
            parse_time("2026-07-03T00:00:00.000Z"),
            parse_time("2026-07-03T00:00:00Z"),
        );
        // A bare date lands at midday, not midnight, so a timezone slip can't
        // move it to the previous day.
        let noon = parse_time("2026-07-03").unwrap();
        assert_eq!(
            chrono::DateTime::from_timestamp(noon, 0)
                .unwrap()
                .format("%Y-%m-%d %H:%M")
                .to_string(),
            "2026-07-03 12:00",
        );
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("last Tuesday"), None);
    }

    #[test]
    fn a_url_pattern_replaces_the_heuristic_but_not_the_safety_gate() {
        let base = u("https://filfre.net/");
        let re = Regex::new(r"/archives\?p=\d+$").unwrap();

        // The heuristic would reject this shape; the pattern accepts it.
        assert!(accepts(
            &u("https://filfre.net/archives?p=99"),
            &base,
            Some(&re)
        ));
        // ...but the pattern cannot authorize leaving the site.
        assert!(!accepts(
            &u("https://evil.com/archives?p=99"),
            &base,
            Some(&re)
        ));
        // ...nor fetching an asset.
        let img = Regex::new(r".*").unwrap();
        assert!(!accepts(
            &u("https://filfre.net/a/b.png"),
            &base,
            Some(&img)
        ));
        // Non-matching URLs are dropped even if the heuristic would like them.
        assert!(!accepts(
            &u("https://filfre.net/2026/07/a-real-post/"),
            &base,
            Some(&re)
        ));
        // With no pattern, the heuristic decides.
        assert!(accepts(
            &u("https://filfre.net/2026/07/a-real-post/"),
            &base,
            None
        ));
        assert!(!accepts(
            &u("https://filfre.net/category/games/"),
            &base,
            None
        ));
    }

    #[test]
    fn a_site_can_only_be_claimed_once_at_a_time() {
        let f = InFlight::default();
        let first = f.claim(7).expect("first claim succeeds");
        assert!(f.contains(7));
        assert!(f.claim(7).is_none(), "a second claim must be refused");
        // ...but other sites are unaffected.
        let other = f.claim(8).expect("a different site is free");
        assert!(f.contains(8));

        drop(first);
        assert!(!f.contains(7), "dropping the claim releases the site");
        assert!(f.claim(7).is_some(), "and it can be claimed again");
        drop(other);
        assert!(!f.contains(8));
    }

    /// A crawl that fails must not leave its site permanently unclaimable.
    #[test]
    fn a_claim_is_released_even_on_an_early_return() {
        let f = InFlight::default();
        fn fallible(f: &InFlight) -> Result<()> {
            let _claim = f.claim(1).unwrap();
            anyhow::bail!("boom");
        }
        assert!(fallible(&f).is_err());
        assert!(!f.contains(1));
    }

    #[test]
    fn auto_named_sites_adopt_their_homepage_title() {
        let base = u("https://www.filfre.net/");
        // Name still equals the derived host => adopt the page title.
        assert_eq!(
            better_name("filfre.net", &base, "The Digital Antiquarian"),
            Some("The Digital Antiquarian".into())
        );
        // A name the user chose is never touched.
        assert_eq!(
            better_name("Jimmy's Blog", &base, "The Digital Antiquarian"),
            None
        );
        // Junk titles are not an improvement.
        assert_eq!(better_name("filfre.net", &base, "   "), None);
        assert_eq!(better_name("filfre.net", &base, &"x".repeat(200)), None);
    }

    #[test]
    fn candidates_merge_feed_metadata_onto_scraped_links() {
        let mut out = HashMap::new();
        // Seen first as a bare link on the index page...
        insert_candidate(
            &mut out,
            u("https://filfre.net/2026/07/maxis/#comments"),
            None,
            None,
        );
        // ...then in the feed, with a title and a date.
        insert_candidate(
            &mut out,
            u("http://www.filfre.net/2026/07/maxis"),
            Some("SimEverything".into()),
            Some(1_783_000_000),
        );

        assert_eq!(out.len(), 1, "both sightings are the same article");
        let c = out.values().next().unwrap();
        assert_eq!(c.title.as_deref(), Some("SimEverything"));
        assert_eq!(c.published_at, Some(1_783_000_000));
        assert_eq!(c.recency, 1_783_000_000);
    }

    #[test]
    fn undated_candidates_fall_back_to_the_date_in_the_permalink() {
        let mut out = HashMap::new();
        insert_candidate(
            &mut out,
            u("https://filfre.net/2011/03/old-post/"),
            None,
            None,
        );
        insert_candidate(
            &mut out,
            u("https://filfre.net/2026/07/new-post/"),
            None,
            None,
        );
        let mut v: Vec<_> = out.into_values().collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.recency));
        assert!(v[0].url.as_str().contains("2026"), "newest first");
        assert!(v[0].recency > v[1].recency);
    }

    #[test]
    fn candidates_with_no_date_at_all_sort_last() {
        let mut out = HashMap::new();
        insert_candidate(
            &mut out,
            u("https://ex.com/some-undated-slug-here/"),
            None,
            None,
        );
        insert_candidate(&mut out, u("https://ex.com/2020/01/dated/"), None, None);
        let mut v: Vec<_> = out.into_values().collect();
        v.sort_by_key(|c| std::cmp::Reverse(c.recency));
        assert!(v[0].url.as_str().contains("2020"));
        assert_eq!(v[1].recency, 0);
    }
}
