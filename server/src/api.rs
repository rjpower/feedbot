//! HTTP surface: a JSON API under `/api`, the Vue bundle everywhere else.

use crate::crawl::{CrawlOutcome, Crawler};
use crate::db::{self, ArticleQuery, NewSite, Pool, SitePatch};
use crate::export::Exports;
use crate::{Config, mobi};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool,
    pub crawler: Arc<Crawler>,
    pub fetch: crate::fetcher::Fetcher,
    pub exports: Arc<Exports>,
    pub token: Option<Arc<str>>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

/// Anything unhandled is a 500 with the full context chain — this is a
/// single-user homelab app, and a useful error beats a coy one.
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("request failed: {e:#}");
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

const TOKEN_HEADER: &str = "x-feedbot-token";

/// Length-checked, branch-free comparison. Cheap enough to just do right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn token_from(req: &Request) -> Option<String> {
    if let Some(v) = req
        .headers()
        .get(TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        return Some(v.to_string());
    }
    // Query param, so an e-reader can pull a .mobi with a plain URL.
    let q = req.uri().query()?;
    form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
}

async fn require_token(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let Some(expected) = st.token.as_deref() else {
        return next.run(req).await; // unset => open, for local dev
    };
    match token_from(&req) {
        Some(got) if constant_time_eq(got.as_bytes(), expected.as_bytes()) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "bad or missing token" })),
        )
            .into_response(),
    }
}

/// Lets the SPA decide whether to show its lock screen before it 401s.
async fn auth_status(State(st): State<AppState>, req: Request) -> Json<serde_json::Value> {
    let required = st.token.is_some();
    let ok = match (&st.token, token_from(&req)) {
        (None, _) => true,
        (Some(exp), Some(got)) => constant_time_eq(got.as_bytes(), exp.as_bytes()),
        (Some(_), None) => false,
    };
    Json(json!({ "required": required, "ok": ok }))
}

// ---------------------------------------------------------------------------
// Sites
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CreateSite {
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub feed_url: Option<String>,
    #[serde(default)]
    pub url_pattern: Option<String>,
    #[serde(default)]
    pub interval_secs: Option<i64>,
    #[serde(default)]
    pub max_new_per_crawl: Option<i64>,
}

/// Crawling every 10 seconds would be rude to the blogs we like.
const MIN_INTERVAL_SECS: i64 = 300;

fn validate_url(raw: &str) -> ApiResult<url::Url> {
    let u = url::Url::parse(raw.trim()).map_err(|e| ApiError::bad(format!("bad url: {e}")))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(ApiError::bad("url must be http or https"));
    }
    if u.host_str().is_none() {
        return Err(ApiError::bad("url has no host"));
    }
    Ok(u)
}

fn validate_pattern(p: Option<&str>) -> ApiResult<()> {
    if let Some(p) = p.map(str::trim).filter(|p| !p.is_empty()) {
        regex::Regex::new(p).map_err(|e| ApiError::bad(format!("bad url_pattern: {e}")))?;
    }
    Ok(())
}

fn validate_interval(secs: Option<i64>) -> ApiResult<()> {
    match secs {
        Some(s) if s < MIN_INTERVAL_SECS => Err(ApiError::bad(format!(
            "interval_secs must be at least {MIN_INTERVAL_SECS}"
        ))),
        _ => Ok(()),
    }
}

async fn list_sites(State(st): State<AppState>) -> ApiResult<Json<Vec<db::Site>>> {
    Ok(Json(db::call(&st.pool, db::list_sites).await?))
}

async fn create_site(
    State(st): State<AppState>,
    Json(body): Json<CreateSite>,
) -> ApiResult<(StatusCode, Json<db::Site>)> {
    let url = validate_url(&body.url)?;
    validate_pattern(body.url_pattern.as_deref())?;
    validate_interval(body.interval_secs)?;
    if let Some(f) = body
        .feed_url
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        validate_url(f)?;
    }

    let name = body
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            url.host_str()
                .unwrap_or("site")
                .trim_start_matches("www.")
                .to_string()
        });

    let new = NewSite {
        name,
        url: url.to_string(),
        feed_url: body.feed_url.filter(|f| !f.trim().is_empty()),
        url_pattern: body.url_pattern.filter(|p| !p.trim().is_empty()),
        interval_secs: body.interval_secs,
        max_new_per_crawl: body.max_new_per_crawl,
    };

    let id = db::call(&st.pool, move |c| db::insert_site(c, &new))
        .await
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                ApiError::bad("that url is already a site")
            } else {
                e.into()
            }
        })?;

    let site = db::call(&st.pool, move |c| db::get_site(c, id))
        .await?
        .ok_or_else(|| ApiError::not_found("site vanished"))?;

    // Give the new site its first crawl right away; the caller shouldn't wait.
    let crawler = st.crawler.clone();
    tokio::spawn(async move {
        if let Err(e) = crawler.crawl_site(id).await {
            tracing::warn!("first crawl of site {id} failed: {e:#}");
        }
    });

    Ok((StatusCode::CREATED, Json(site)))
}

async fn update_site(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(patch): Json<SitePatch>,
) -> ApiResult<Json<db::Site>> {
    if let Some(u) = &patch.url {
        validate_url(u)?;
    }
    if let Some(p) = &patch.url_pattern {
        validate_pattern(p.as_deref())?;
    }
    if let Some(Some(f)) = patch.feed_url.as_ref().map(|f| f.as_deref().map(str::trim))
        && !f.is_empty()
    {
        validate_url(f)?;
    }
    validate_interval(patch.interval_secs)?;

    let found = db::call(&st.pool, move |c| db::update_site(c, id, &patch)).await?;
    if !found {
        return Err(ApiError::not_found("no such site"));
    }
    db::call(&st.pool, move |c| db::get_site(c, id))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("no such site"))
}

async fn remove_site(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    if db::call(&st.pool, move |c| db::delete_site(c, id)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no such site"))
    }
}

#[derive(Deserialize)]
pub struct CrawlOpts {
    /// Run the crawl inline and return its summary instead of queueing it.
    #[serde(default, deserialize_with = "truthy")]
    wait: bool,
}

/// Query strings are typed by humans, and a human writes `?wait=1`.
fn truthy<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    let s = String::deserialize(d)?;
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" | "" => Ok(false),
        other => Err(serde::de::Error::custom(format!(
            "expected a boolean, got {other:?}"
        ))),
    }
}

async fn crawl_now(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Query(opts): Query<CrawlOpts>,
) -> ApiResult<Response> {
    if db::call(&st.pool, move |c| db::get_site(c, id))
        .await?
        .is_none()
    {
        return Err(ApiError::not_found("no such site"));
    }
    let busy = || {
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": "a crawl for this site is already running" })),
        )
            .into_response()
    };

    if opts.wait {
        return Ok(match st.crawler.crawl_site(id).await? {
            CrawlOutcome::Ran(summary) => Json(summary).into_response(),
            CrawlOutcome::AlreadyRunning => busy(),
        });
    }
    if st.crawler.is_running(id) {
        return Ok(busy());
    }
    let crawler = st.crawler.clone();
    tokio::spawn(async move {
        if let Err(e) = crawler.crawl_site(id).await {
            tracing::warn!("manual crawl of site {id} failed: {e:#}");
        }
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "queued": true }))).into_response())
}

// ---------------------------------------------------------------------------
// Articles
// ---------------------------------------------------------------------------

async fn list_articles(
    State(st): State<AppState>,
    Query(q): Query<ArticleQuery>,
) -> ApiResult<Json<Vec<db::Article>>> {
    Ok(Json(
        db::call(&st.pool, move |c| db::list_articles(c, &q)).await?,
    ))
}

#[derive(Deserialize)]
pub struct ReadContext {
    /// Which list the reader is walking, for prev/next.
    #[serde(default)]
    state: Option<String>,
}

#[derive(Serialize)]
pub struct ArticleView {
    #[serde(flatten)]
    article: db::Article,
    prev_id: Option<i64>,
    next_id: Option<i64>,
}

async fn get_article(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Query(ctx): Query<ReadContext>,
) -> ApiResult<Json<ArticleView>> {
    let state = ctx.state.unwrap_or_else(|| "unread".into());
    let view = db::call(&st.pool, move |c| {
        let Some(article) = db::get_article(c, id)? else {
            return Ok(None);
        };
        let (prev_id, next_id) = db::adjacent_ids(c, id, &state)?;
        Ok(Some(ArticleView {
            article,
            prev_id,
            next_id,
        }))
    })
    .await?;
    view.map(Json)
        .ok_or_else(|| ApiError::not_found("no such article"))
}

#[derive(Deserialize)]
pub struct ReadBody {
    read: bool,
}

async fn set_read(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<ReadBody>,
) -> ApiResult<StatusCode> {
    if db::call(&st.pool, move |c| db::set_read(c, id, b.read)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no such article"))
    }
}

#[derive(Deserialize)]
pub struct StarBody {
    starred: bool,
}

async fn set_starred(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<StarBody>,
) -> ApiResult<StatusCode> {
    if db::call(&st.pool, move |c| db::set_starred(c, id, b.starred)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no such article"))
    }
}

async fn remove_article(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    if db::call(&st.pool, move |c| db::delete_article(c, id)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("no such article"))
    }
}

#[derive(Deserialize)]
pub struct ReadAllBody {
    #[serde(default)]
    site_id: Option<i64>,
}

async fn mark_all_read(
    State(st): State<AppState>,
    Json(b): Json<ReadAllBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let n = db::call(&st.pool, move |c| db::mark_all_read(c, b.site_id)).await?;
    Ok(Json(json!({ "marked": n })))
}

// ---------------------------------------------------------------------------
// MOBI
// ---------------------------------------------------------------------------

fn mobi_response(bytes: Vec<u8>, filename: &str) -> Response {
    (
        [
            // The type the Kindle browser recognizes as "a book to download".
            (header::CONTENT_TYPE, "application/x-mobipocket-ebook".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}.mobi\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

async fn article_mobi(State(st): State<AppState>, Path(id): Path<i64>) -> ApiResult<Response> {
    let article = db::call(&st.pool, move |c| db::get_article(c, id))
        .await?
        .ok_or_else(|| ApiError::not_found("no such article"))?;
    let name = mobi::safe_filename(&article.title);
    // One article is quick, so it stays a plain synchronous download — no job,
    // nothing to report progress about.
    let bytes = mobi::build(
        std::slice::from_ref(&article),
        &article.title,
        &st.fetch,
        &mobi::no_progress,
    )
    .await?;
    Ok(mobi_response(bytes, &name))
}

/// How many of each site's most-recent articles a whole-list export includes.
/// Per-site rather than a global cut so a prolific blog can't crowd the quiet
/// ones out of the book — every site with matching articles is represented.
const DEFAULT_PER_SITE: i64 = 10;

#[derive(Deserialize)]
pub struct ExportQuery {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    site_id: Option<i64>,
    #[serde(default)]
    per_site: Option<i64>,
}

/// Start a reading-list export — one section per site, each site's most-recent
/// posts nested beneath it, every image embedded rather than left a dead remote
/// link. The book takes minutes to build, so this returns a job to poll rather
/// than the bytes; [`download_export`] serves the `.mobi` once it's ready.
async fn start_export(
    State(st): State<AppState>,
    Query(q): Query<ExportQuery>,
) -> (StatusCode, Json<crate::export::JobView>) {
    let label = q.state.unwrap_or_else(|| "unread".into());
    let per_site = q.per_site.unwrap_or(DEFAULT_PER_SITE);
    let job = st.exports.start(
        st.pool.clone(),
        st.fetch.clone(),
        label,
        q.site_id,
        per_site,
    );
    (StatusCode::ACCEPTED, Json(job.view()))
}

/// Where the poller lives: phase and articles-done while running, byte size when
/// the book is ready, the error if it failed.
async fn export_status(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<crate::export::JobView>> {
    st.exports
        .get(&id)
        .map(|j| Json(j.view()))
        .ok_or_else(|| ApiError::not_found("no such export"))
}

/// The finished `.mobi`. A plain GET with a query-param token, so the e-reader
/// can pull it straight from the download the SPA hands it. 409 until it's ready.
async fn download_export(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let job = st
        .exports
        .get(&id)
        .ok_or_else(|| ApiError::not_found("no such export"))?;
    match job.bytes() {
        Some(bytes) => Ok(mobi_response(bytes.to_vec(), &job.filename)),
        None => Err(ApiError(
            StatusCode::CONFLICT,
            "export is not ready yet".into(),
        )),
    }
}

/// Recent export jobs, newest first — lets the UI re-attach after a reload.
async fn list_exports(State(st): State<AppState>) -> Json<Vec<crate::export::JobView>> {
    Json(st.exports.list())
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

async fn stats(State(st): State<AppState>) -> ApiResult<Json<db::Stats>> {
    Ok(Json(db::call(&st.pool, db::stats).await?))
}

#[derive(Deserialize)]
pub struct CrawlsQuery {
    #[serde(default)]
    limit: Option<i64>,
}

async fn list_crawls(
    State(st): State<AppState>,
    Query(q): Query<CrawlsQuery>,
) -> ApiResult<Json<Vec<db::Crawl>>> {
    let limit = q.limit.unwrap_or(25);
    Ok(Json(
        db::call(&st.pool, move |c| db::list_crawls(c, limit)).await?,
    ))
}

pub fn router(state: AppState, cfg: &Config) -> Router {
    let api = Router::new()
        .route("/stats", get(stats))
        .route("/crawls", get(list_crawls))
        .route("/sites", get(list_sites).post(create_site))
        .route("/sites/{id}", patch(update_site).delete(remove_site))
        .route("/sites/{id}/crawl", post(crawl_now))
        .route("/articles", get(list_articles))
        .route("/articles/read-all", post(mark_all_read))
        .route("/articles/{id}", get(get_article))
        .route("/articles/{id}", delete(remove_article))
        .route("/articles/{id}/read", post(set_read))
        .route("/articles/{id}/star", post(set_starred))
        .route("/articles/{id}/mobi", get(article_mobi))
        .route("/export/mobi", post(start_export))
        .route("/export/mobi/{id}", get(export_status))
        .route("/export/mobi/{id}/download", get(download_export))
        .route("/exports", get(list_exports))
        // Without this, an unknown /api path falls through to the SPA and a
        // typo'd endpoint answers with a page of HTML.
        .fallback(|| async { ApiError::not_found("no such endpoint") })
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));

    // `.fallback` and not `.not_found_service`: the latter wraps the response in
    // SetStatus(404), which is right for a custom error page and wrong for an
    // SPA, where /read/12 is a real page that happens to live in index.html.
    let index = cfg.static_dir.join("index.html");
    let spa = ServeDir::new(&cfg.static_dir).fallback(ServeFile::new(&index));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        // Outside the token layer on purpose: it is how the SPA learns it needs one.
        .route("/api/auth", get(auth_status))
        .nest("/api", api)
        .fallback_service(spa)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn urls_must_be_http() {
        assert!(validate_url("https://filfre.net/").is_ok());
        assert!(validate_url("http://filfre.net/").is_ok());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("not a url").is_err());
    }

    #[test]
    fn patterns_must_compile() {
        assert!(validate_pattern(Some(r"/\d{4}/")).is_ok());
        assert!(validate_pattern(Some("")).is_ok());
        assert!(validate_pattern(None).is_ok());
        assert!(validate_pattern(Some("[unclosed")).is_err());
    }

    #[test]
    fn intervals_have_a_floor() {
        assert!(validate_interval(Some(86_400)).is_ok());
        assert!(validate_interval(Some(MIN_INTERVAL_SECS)).is_ok());
        assert!(validate_interval(Some(10)).is_err());
        assert!(validate_interval(None).is_ok());
    }
}
