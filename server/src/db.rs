//! SQLite storage. One file, WAL mode, migrated forward via `user_version`.

use anyhow::{Context, Result};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub type Pool = r2d2::Pool<SqliteConnectionManager>;

/// Every migration ever applied, in order. Never edit one that has shipped;
/// append instead. `user_version` records how many have run.
const MIGRATIONS: &[&str] = &[r#"
    CREATE TABLE sites (
        id                INTEGER PRIMARY KEY,
        name              TEXT    NOT NULL,
        url               TEXT    NOT NULL UNIQUE,
        feed_url          TEXT,
        url_pattern       TEXT,
        interval_secs     INTEGER NOT NULL DEFAULT 86400,
        max_new_per_crawl INTEGER NOT NULL DEFAULT 25,
        enabled           INTEGER NOT NULL DEFAULT 1,
        last_crawled_at   INTEGER,
        created_at        INTEGER NOT NULL
    );

    CREATE TABLE articles (
        id           INTEGER PRIMARY KEY,
        site_id      INTEGER NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
        url          TEXT    NOT NULL,
        url_key      TEXT    NOT NULL UNIQUE,
        title        TEXT    NOT NULL,
        byline       TEXT,
        excerpt      TEXT,
        content_html TEXT    NOT NULL,
        word_count   INTEGER NOT NULL DEFAULT 0,
        published_at INTEGER,
        fetched_at   INTEGER NOT NULL,
        read_at      INTEGER,
        starred      INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX idx_articles_site ON articles(site_id);
    CREATE INDEX idx_articles_read ON articles(read_at);
    CREATE INDEX idx_articles_sort ON articles(published_at DESC, fetched_at DESC);

    CREATE TABLE crawls (
        id          INTEGER PRIMARY KEY,
        site_id     INTEGER NOT NULL REFERENCES sites(id) ON DELETE CASCADE,
        started_at  INTEGER NOT NULL,
        finished_at INTEGER,
        discovered  INTEGER NOT NULL DEFAULT 0,
        added       INTEGER NOT NULL DEFAULT 0,
        ok          INTEGER,
        error       TEXT
    );
    CREATE INDEX idx_crawls_site ON crawls(site_id, started_at DESC);
    "#];

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn open(path: &Path) -> Result<Pool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let manager = SqliteConnectionManager::file(path).with_init(|c| {
        c.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )
    });
    let pool = r2d2::Pool::builder()
        .max_size(8)
        .build(manager)
        .context("opening sqlite pool")?;
    let conn = pool.get()?;
    migrate(&conn)?;
    drop(conn);
    Ok(pool)
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        tracing::info!("applying migration {}", i + 1);
        conn.execute_batch(sql)
            .with_context(|| format!("migration {}", i + 1))?;
        conn.pragma_update(None, "user_version", (i + 1) as i64)?;
    }
    Ok(())
}

/// Run a blocking rusqlite closure off the async runtime.
pub async fn call<F, T>(pool: &Pool, f: F) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let pool = pool.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().context("checking out a db connection")?;
        f(&conn)
    })
    .await
    .context("db task panicked")?
}

// ---------------------------------------------------------------------------
// Sites
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Site {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub feed_url: Option<String>,
    pub url_pattern: Option<String>,
    pub interval_secs: i64,
    pub max_new_per_crawl: i64,
    pub enabled: bool,
    pub last_crawled_at: Option<i64>,
    pub created_at: i64,
    /// Only populated by `list_sites`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub article_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_count: Option<i64>,
}

fn site_from_row(r: &Row, counts: bool) -> rusqlite::Result<Site> {
    Ok(Site {
        id: r.get("id")?,
        name: r.get("name")?,
        url: r.get("url")?,
        feed_url: r.get("feed_url")?,
        url_pattern: r.get("url_pattern")?,
        interval_secs: r.get("interval_secs")?,
        max_new_per_crawl: r.get("max_new_per_crawl")?,
        enabled: r.get::<_, i64>("enabled")? != 0,
        last_crawled_at: r.get("last_crawled_at")?,
        created_at: r.get("created_at")?,
        article_count: if counts {
            Some(r.get("article_count")?)
        } else {
            None
        },
        unread_count: if counts {
            Some(r.get("unread_count")?)
        } else {
            None
        },
    })
}

const SITE_COLS: &str = "id, name, url, feed_url, url_pattern, interval_secs, \
                         max_new_per_crawl, enabled, last_crawled_at, created_at";

pub fn list_sites(conn: &Connection) -> Result<Vec<Site>> {
    let sql = format!(
        "SELECT s.{SITE_COLS},
                (SELECT COUNT(*) FROM articles a WHERE a.site_id = s.id) AS article_count,
                (SELECT COUNT(*) FROM articles a WHERE a.site_id = s.id AND a.read_at IS NULL) AS unread_count
         FROM sites s ORDER BY s.name COLLATE NOCASE"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| site_from_row(r, true))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get_site(conn: &Connection, id: i64) -> Result<Option<Site>> {
    let sql = format!("SELECT {SITE_COLS} FROM sites WHERE id = ?1");
    Ok(conn
        .query_row(&sql, [id], |r| site_from_row(r, false))
        .optional()?)
}

/// Sites that are enabled and whose interval has elapsed since the last crawl.
pub fn due_sites(conn: &Connection, now: i64) -> Result<Vec<Site>> {
    let sql = format!(
        "SELECT {SITE_COLS} FROM sites
         WHERE enabled = 1
           AND (last_crawled_at IS NULL OR last_crawled_at + interval_secs <= ?1)
         ORDER BY COALESCE(last_crawled_at, 0) ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([now], |r| site_from_row(r, false))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Debug, Deserialize)]
pub struct NewSite {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub feed_url: Option<String>,
    #[serde(default)]
    pub url_pattern: Option<String>,
    #[serde(default)]
    pub interval_secs: Option<i64>,
    #[serde(default)]
    pub max_new_per_crawl: Option<i64>,
}

pub const DEFAULT_INTERVAL_SECS: i64 = 24 * 60 * 60;
pub const DEFAULT_MAX_NEW: i64 = 25;

pub fn insert_site(conn: &Connection, s: &NewSite) -> Result<i64> {
    conn.execute(
        "INSERT INTO sites (name, url, feed_url, url_pattern, interval_secs, max_new_per_crawl, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            s.name,
            s.url,
            s.feed_url,
            s.url_pattern,
            s.interval_secs.unwrap_or(DEFAULT_INTERVAL_SECS),
            s.max_new_per_crawl.unwrap_or(DEFAULT_MAX_NEW),
            now(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Fields a PATCH may change. `None` means "leave alone"; note that
/// `Option<Option<T>>` lets the client explicitly null out `feed_url`.
#[derive(Debug, Default, Deserialize)]
pub struct SitePatch {
    pub name: Option<String>,
    pub url: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    pub feed_url: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub url_pattern: Option<Option<String>>,
    pub interval_secs: Option<i64>,
    pub max_new_per_crawl: Option<i64>,
    pub enabled: Option<bool>,
}

fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(d).map(Some)
}

pub fn update_site(conn: &Connection, id: i64, p: &SitePatch) -> Result<bool> {
    let mut sets: Vec<&str> = Vec::new();
    let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    macro_rules! set {
        ($field:expr, $col:literal) => {
            if let Some(v) = &$field {
                sets.push(concat!($col, " = ?"));
                vals.push(Box::new(v.clone()));
            }
        };
    }
    set!(p.name, "name");
    set!(p.url, "url");
    set!(p.feed_url, "feed_url");
    set!(p.url_pattern, "url_pattern");
    set!(p.interval_secs, "interval_secs");
    set!(p.max_new_per_crawl, "max_new_per_crawl");
    if let Some(v) = p.enabled {
        sets.push("enabled = ?");
        vals.push(Box::new(v as i64));
    }
    if sets.is_empty() {
        return Ok(true);
    }
    // Rebuild `?` into `?1..?n` positionally: rusqlite binds by index.
    let mut sql = format!("UPDATE sites SET {} WHERE id = ?", sets.join(", "));
    let mut n = 0;
    sql = sql
        .chars()
        .map(|c| {
            if c == '?' {
                n += 1;
                format!("?{n}")
            } else {
                c.to_string()
            }
        })
        .collect();
    vals.push(Box::new(id));
    let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|b| b.as_ref()).collect();
    Ok(conn.execute(&sql, refs.as_slice())? > 0)
}

pub fn delete_site(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM sites WHERE id = ?1", [id])? > 0)
}

pub fn set_feed_url(conn: &Connection, id: i64, feed: &str) -> Result<()> {
    conn.execute(
        "UPDATE sites SET feed_url = ?1 WHERE id = ?2",
        rusqlite::params![feed, id],
    )?;
    Ok(())
}

pub fn rename_site(conn: &Connection, id: i64, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE sites SET name = ?1 WHERE id = ?2",
        rusqlite::params![name, id],
    )?;
    Ok(())
}

pub fn mark_crawled(conn: &Connection, id: i64, at: i64) -> Result<()> {
    conn.execute(
        "UPDATE sites SET last_crawled_at = ?1 WHERE id = ?2",
        rusqlite::params![at, id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Articles
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct Article {
    pub id: i64,
    pub site_id: i64,
    pub site_name: String,
    pub url: String,
    pub title: String,
    pub byline: Option<String>,
    pub excerpt: Option<String>,
    pub word_count: i64,
    pub published_at: Option<i64>,
    pub fetched_at: i64,
    pub read_at: Option<i64>,
    pub starred: bool,
    /// Only on the single-article endpoint; the list omits it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_html: Option<String>,
}

fn article_from_row(r: &Row, with_content: bool) -> rusqlite::Result<Article> {
    Ok(Article {
        id: r.get("id")?,
        site_id: r.get("site_id")?,
        site_name: r.get("site_name")?,
        url: r.get("url")?,
        title: r.get("title")?,
        byline: r.get("byline")?,
        excerpt: r.get("excerpt")?,
        word_count: r.get("word_count")?,
        published_at: r.get("published_at")?,
        fetched_at: r.get("fetched_at")?,
        read_at: r.get("read_at")?,
        starred: r.get::<_, i64>("starred")? != 0,
        content_html: if with_content {
            Some(r.get("content_html")?)
        } else {
            None
        },
    })
}

pub struct NewArticle {
    pub site_id: i64,
    pub url: String,
    pub url_key: String,
    pub title: String,
    pub byline: Option<String>,
    pub excerpt: Option<String>,
    pub content_html: String,
    pub word_count: i64,
    pub published_at: Option<i64>,
}

/// Insert, ignoring articles we already have. Returns true if a row was added.
pub fn insert_article(conn: &Connection, a: &NewArticle) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO articles
         (site_id, url, url_key, title, byline, excerpt, content_html, word_count, published_at, fetched_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            a.site_id, a.url, a.url_key, a.title, a.byline, a.excerpt,
            a.content_html, a.word_count, a.published_at, now(),
        ],
    )?;
    Ok(n > 0)
}

/// The dedupe keys we already hold for a site, so a crawl can skip them without
/// a round trip per candidate.
pub fn known_keys(conn: &Connection, site_id: i64) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT url_key FROM articles WHERE site_id = ?1")?;
    let rows = stmt.query_map([site_id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Debug, Deserialize)]
pub struct ArticleQuery {
    /// `unread` (default), `all`, or `starred`.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub site_id: Option<i64>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

const ARTICLE_COLS: &str = "a.id, a.site_id, s.name AS site_name, a.url, a.title, a.byline, \
                            a.excerpt, a.word_count, a.published_at, a.fetched_at, a.read_at, a.starred";

/// Newest first, treating a missing publication date as the fetch date rather
/// than sorting those articles to the bottom forever.
const ARTICLE_ORDER: &str = "ORDER BY COALESCE(a.published_at, a.fetched_at) DESC, a.id DESC";

pub fn list_articles(conn: &Connection, q: &ArticleQuery) -> Result<Vec<Article>> {
    let mut where_sql = String::from("WHERE 1=1");
    let mut vals: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    match q.state.as_deref().unwrap_or("unread") {
        "unread" => where_sql.push_str(" AND a.read_at IS NULL"),
        "starred" => where_sql.push_str(" AND a.starred = 1"),
        _ => {}
    }
    if let Some(site_id) = q.site_id {
        vals.push(Box::new(site_id));
        where_sql.push_str(&format!(" AND a.site_id = ?{}", vals.len()));
    }
    if let Some(text) = q.q.as_deref().filter(|t| !t.trim().is_empty()) {
        // Escape LIKE wildcards so a search for "100%" doesn't match everything.
        let escaped = text
            .replace('\\', r"\\")
            .replace('%', r"\%")
            .replace('_', r"\_");
        vals.push(Box::new(format!("%{escaped}%")));
        let i = vals.len();
        where_sql.push_str(&format!(
            " AND (a.title LIKE ?{i} ESCAPE '\\' OR a.excerpt LIKE ?{i} ESCAPE '\\')"
        ));
    }

    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);
    vals.push(Box::new(limit));
    let li = vals.len();
    vals.push(Box::new(offset));
    let oi = vals.len();

    let sql = format!(
        "SELECT {ARTICLE_COLS} FROM articles a JOIN sites s ON s.id = a.site_id
         {where_sql} {ARTICLE_ORDER} LIMIT ?{li} OFFSET ?{oi}"
    );
    let refs: Vec<&dyn rusqlite::ToSql> = vals.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(refs.as_slice(), |r| article_from_row(r, false))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get_article(conn: &Connection, id: i64) -> Result<Option<Article>> {
    let sql = format!(
        "SELECT {ARTICLE_COLS}, a.content_html FROM articles a
         JOIN sites s ON s.id = a.site_id WHERE a.id = ?1"
    );
    Ok(conn
        .query_row(&sql, [id], |r| article_from_row(r, true))
        .optional()?)
}

/// Neighbours in the current sort order, so the reader can page through.
pub fn adjacent_ids(conn: &Connection, id: i64, state: &str) -> Result<(Option<i64>, Option<i64>)> {
    let filter = match state {
        "unread" => "AND (a.read_at IS NULL OR a.id = ?1)",
        "starred" => "AND a.starred = 1",
        _ => "",
    };
    // Rank by the same key the list uses, then step one either side.
    let sql = format!(
        "WITH ordered AS (
            SELECT a.id, ROW_NUMBER() OVER ({ARTICLE_ORDER}) AS rn
            FROM articles a WHERE 1=1 {filter}
         )
         SELECT
            (SELECT id FROM ordered WHERE rn = (SELECT rn FROM ordered WHERE id = ?1) - 1),
            (SELECT id FROM ordered WHERE rn = (SELECT rn FROM ordered WHERE id = ?1) + 1)"
    );
    Ok(conn.query_row(&sql, [id], |r| Ok((r.get(0)?, r.get(1)?)))?)
}

pub fn set_read(conn: &Connection, id: i64, read: bool) -> Result<bool> {
    let at = read.then(now);
    Ok(conn.execute(
        "UPDATE articles SET read_at = ?1 WHERE id = ?2",
        rusqlite::params![at, id],
    )? > 0)
}

pub fn set_starred(conn: &Connection, id: i64, starred: bool) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE articles SET starred = ?1 WHERE id = ?2",
        rusqlite::params![starred as i64, id],
    )? > 0)
}

pub fn mark_all_read(conn: &Connection, site_id: Option<i64>) -> Result<usize> {
    Ok(match site_id {
        Some(s) => conn.execute(
            "UPDATE articles SET read_at = ?1 WHERE read_at IS NULL AND site_id = ?2",
            rusqlite::params![now(), s],
        )?,
        None => conn.execute(
            "UPDATE articles SET read_at = ?1 WHERE read_at IS NULL",
            [now()],
        )?,
    })
}

pub fn delete_article(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM articles WHERE id = ?1", [id])? > 0)
}

// ---------------------------------------------------------------------------
// Crawls
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Crawl {
    pub id: i64,
    pub site_id: i64,
    pub site_name: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub discovered: i64,
    pub added: i64,
    pub ok: Option<bool>,
    pub error: Option<String>,
}

pub fn start_crawl(conn: &Connection, site_id: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO crawls (site_id, started_at) VALUES (?1, ?2)",
        rusqlite::params![site_id, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finish_crawl(
    conn: &Connection,
    id: i64,
    discovered: i64,
    added: i64,
    error: Option<String>,
) -> Result<()> {
    conn.execute(
        "UPDATE crawls SET finished_at = ?1, discovered = ?2, added = ?3, ok = ?4, error = ?5
         WHERE id = ?6",
        rusqlite::params![now(), discovered, added, error.is_none() as i64, error, id],
    )?;
    Ok(())
}

pub fn list_crawls(conn: &Connection, limit: i64) -> Result<Vec<Crawl>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.site_id, s.name AS site_name, c.started_at, c.finished_at,
                c.discovered, c.added, c.ok, c.error
         FROM crawls c JOIN sites s ON s.id = c.site_id
         -- id breaks the tie when two crawls start within the same second.
         ORDER BY c.started_at DESC, c.id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit.clamp(1, 200)], |r| {
        Ok(Crawl {
            id: r.get("id")?,
            site_id: r.get("site_id")?,
            site_name: r.get("site_name")?,
            started_at: r.get("started_at")?,
            finished_at: r.get("finished_at")?,
            discovered: r.get("discovered")?,
            added: r.get("added")?,
            ok: r.get::<_, Option<i64>>("ok")?.map(|v| v != 0),
            error: r.get("error")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub sites: i64,
    pub articles: i64,
    pub unread: i64,
    pub starred: i64,
}

pub fn stats(conn: &Connection) -> Result<Stats> {
    Ok(conn.query_row(
        "SELECT (SELECT COUNT(*) FROM sites),
                (SELECT COUNT(*) FROM articles),
                (SELECT COUNT(*) FROM articles WHERE read_at IS NULL),
                (SELECT COUNT(*) FROM articles WHERE starred = 1)",
        [],
        |r| {
            Ok(Stats {
                sites: r.get(0)?,
                articles: r.get(1)?,
                unread: r.get(2)?,
                starred: r.get(3)?,
            })
        },
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        migrate(&c).unwrap();
        c
    }

    fn seed_site(c: &Connection, name: &str) -> i64 {
        insert_site(
            c,
            &NewSite {
                name: name.into(),
                url: format!("https://{name}.example.com/"),
                feed_url: None,
                url_pattern: None,
                interval_secs: None,
                max_new_per_crawl: None,
            },
        )
        .unwrap()
    }

    fn seed_article(c: &Connection, site_id: i64, key: &str, published: Option<i64>) -> bool {
        insert_article(
            c,
            &NewArticle {
                site_id,
                url: format!("https://x.test/{key}"),
                url_key: key.into(),
                title: format!("Post {key}"),
                byline: None,
                excerpt: Some("excerpt".into()),
                content_html: "<p>hi</p>".into(),
                word_count: 1,
                published_at: published,
            },
        )
        .unwrap()
    }

    #[test]
    fn migrations_are_idempotent() {
        let c = mem();
        migrate(&c).unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }

    #[test]
    fn duplicate_articles_are_ignored_not_errors() {
        let c = mem();
        let s = seed_site(&c, "a");
        assert!(seed_article(&c, s, "k1", None));
        assert!(
            !seed_article(&c, s, "k1", None),
            "second insert should be a no-op"
        );
        assert_eq!(stats(&c).unwrap().articles, 1);
    }

    #[test]
    fn deleting_a_site_takes_its_articles_with_it() {
        let c = mem();
        let s = seed_site(&c, "a");
        seed_article(&c, s, "k1", None);
        assert!(delete_site(&c, s).unwrap());
        assert_eq!(stats(&c).unwrap().articles, 0);
    }

    #[test]
    fn unread_is_the_default_filter_and_starred_survives_reading() {
        let c = mem();
        let s = seed_site(&c, "a");
        seed_article(&c, s, "k1", Some(100));
        seed_article(&c, s, "k2", Some(200));
        let id = list_articles(
            &c,
            &ArticleQuery {
                state: None,
                site_id: None,
                q: None,
                limit: None,
                offset: None,
            },
        )
        .unwrap()[0]
            .id;

        set_read(&c, id, true).unwrap();
        set_starred(&c, id, true).unwrap();

        let unread = list_articles(
            &c,
            &ArticleQuery {
                state: Some("unread".into()),
                site_id: None,
                q: None,
                limit: None,
                offset: None,
            },
        )
        .unwrap();
        assert_eq!(unread.len(), 1);
        let starred = list_articles(
            &c,
            &ArticleQuery {
                state: Some("starred".into()),
                site_id: None,
                q: None,
                limit: None,
                offset: None,
            },
        )
        .unwrap();
        assert_eq!(starred.len(), 1);
        let all = list_articles(
            &c,
            &ArticleQuery {
                state: Some("all".into()),
                site_id: None,
                q: None,
                limit: None,
                offset: None,
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn newest_first_using_fetched_at_when_undated() {
        let c = mem();
        let s = seed_site(&c, "a");
        seed_article(&c, s, "old", Some(1_000));
        seed_article(&c, s, "new", Some(9_000));
        seed_article(&c, s, "undated", None); // fetched now => sorts first
        let got = list_articles(
            &c,
            &ArticleQuery {
                state: Some("all".into()),
                site_id: None,
                q: None,
                limit: None,
                offset: None,
            },
        )
        .unwrap();
        let titles: Vec<_> = got.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(titles, vec!["Post undated", "Post new", "Post old"]);
    }

    #[test]
    fn search_escapes_like_wildcards() {
        let c = mem();
        let s = seed_site(&c, "a");
        seed_article(&c, s, "k1", None); // title "Post k1"
        let hit = |t: &str| {
            list_articles(
                &c,
                &ArticleQuery {
                    state: Some("all".into()),
                    site_id: None,
                    q: Some(t.into()),
                    limit: None,
                    offset: None,
                },
            )
            .unwrap()
            .len()
        };
        assert_eq!(hit("Post"), 1);
        assert_eq!(hit("%"), 0, "a literal % must not match everything");
        assert_eq!(hit("_"), 0, "a literal _ must not match any single char");
    }

    #[test]
    fn adjacent_ids_walks_the_list_order() {
        let c = mem();
        let s = seed_site(&c, "a");
        seed_article(&c, s, "a", Some(300));
        seed_article(&c, s, "b", Some(200));
        seed_article(&c, s, "c", Some(100));
        let all: Vec<i64> = list_articles(
            &c,
            &ArticleQuery {
                state: Some("all".into()),
                site_id: None,
                q: None,
                limit: None,
                offset: None,
            },
        )
        .unwrap()
        .iter()
        .map(|a| a.id)
        .collect();
        let (prev, next) = adjacent_ids(&c, all[1], "all").unwrap();
        assert_eq!(prev, Some(all[0]));
        assert_eq!(next, Some(all[2]));
        let (prev, next) = adjacent_ids(&c, all[0], "all").unwrap();
        assert_eq!(prev, None);
        assert_eq!(next, Some(all[1]));
    }

    #[test]
    fn due_sites_respects_interval_and_enabled() {
        let c = mem();
        let never = seed_site(&c, "never");
        let fresh = seed_site(&c, "fresh");
        let stale = seed_site(&c, "stale");
        let off = seed_site(&c, "off");
        let t = 1_000_000;
        mark_crawled(&c, fresh, t - 10).unwrap();
        mark_crawled(&c, stale, t - DEFAULT_INTERVAL_SECS - 10).unwrap();
        mark_crawled(&c, off, 0).unwrap();
        update_site(
            &c,
            off,
            &SitePatch {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .unwrap();

        let due: Vec<i64> = due_sites(&c, t).unwrap().iter().map(|s| s.id).collect();
        assert!(due.contains(&never) && due.contains(&stale));
        assert!(!due.contains(&fresh) && !due.contains(&off));
    }

    #[test]
    fn patch_can_null_out_a_feed_url_but_leaves_omitted_fields() {
        let c = mem();
        let s = seed_site(&c, "a");
        set_feed_url(&c, s, "https://a.example.com/feed").unwrap();

        // Omitted field => untouched.
        update_site(
            &c,
            s,
            &SitePatch {
                name: Some("renamed".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let site = get_site(&c, s).unwrap().unwrap();
        assert_eq!(site.name, "renamed");
        assert_eq!(site.feed_url.as_deref(), Some("https://a.example.com/feed"));

        // Explicit null => cleared.
        update_site(
            &c,
            s,
            &SitePatch {
                feed_url: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(get_site(&c, s).unwrap().unwrap().feed_url, None);
    }

    #[test]
    fn crawl_rows_record_success_and_failure() {
        let c = mem();
        let s = seed_site(&c, "a");
        let ok = start_crawl(&c, s).unwrap();
        finish_crawl(&c, ok, 5, 2, None).unwrap();
        let bad = start_crawl(&c, s).unwrap();
        finish_crawl(&c, bad, 0, 0, Some("boom".into())).unwrap();

        let rows = list_crawls(&c, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ok, Some(false));
        assert_eq!(rows[0].error.as_deref(), Some("boom"));
        assert_eq!(rows[1].ok, Some(true));
        assert_eq!((rows[1].discovered, rows[1].added), (5, 2));
    }
}
