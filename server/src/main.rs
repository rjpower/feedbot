//! feedbot — scrape blogs, extract readable articles, serve them like a feed.

mod api;
mod crawl;
mod db;
mod epub;
mod fetcher;
mod urlx;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct Config {
    pub db_path: PathBuf,
    pub static_dir: PathBuf,
    pub port: u16,
    pub token: Option<String>,
    pub fetcher_url: String,
    pub fetcher_port: u16,
    /// `None` means something else is running the sidecar (local dev).
    pub fetcher_script: Option<PathBuf>,
    pub crawl_delay: Duration,
    pub scheduler_tick: Duration,
}

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Config {
    fn from_env() -> Self {
        let fetcher_port = env_or("FEEDBOT_FETCHER_PORT", 4000u16);
        Self {
            db_path: env_or("FEEDBOT_DB", PathBuf::from("/data/feedbot.db")),
            static_dir: env_or("FEEDBOT_STATIC", PathBuf::from("/app/static")),
            port: env_or("FEEDBOT_PORT", 8000u16),
            token: std::env::var("FEEDBOT_TOKEN")
                .ok()
                .filter(|t| !t.trim().is_empty()),
            fetcher_url: env_or(
                "FEEDBOT_FETCHER_URL",
                format!("http://127.0.0.1:{fetcher_port}"),
            ),
            fetcher_port,
            fetcher_script: std::env::var("FEEDBOT_FETCHER_SCRIPT")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .or_else(|| Some(PathBuf::from("/app/fetcher/server.mjs")))
                .filter(|p| p.exists()),
            crawl_delay: Duration::from_millis(env_or("FEEDBOT_CRAWL_DELAY_MS", 1500u64)),
            scheduler_tick: Duration::from_secs(env_or("FEEDBOT_SCHEDULER_TICK_SECS", 300u64)),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "feedbot=info,tower_http=warn".into()),
        )
        .init();

    let cfg = Config::from_env();
    tracing::info!("db {:?}, static {:?}", cfg.db_path, cfg.static_dir);
    if cfg.token.is_none() {
        tracing::warn!("FEEDBOT_TOKEN is unset — the API is open to anyone who can reach it");
    }

    let pool = db::open(&cfg.db_path).context("opening database")?;

    match &cfg.fetcher_script {
        Some(script) => fetcher::supervise(script.display().to_string(), cfg.fetcher_port),
        None => tracing::info!(
            "not spawning a fetcher; expecting one at {}",
            cfg.fetcher_url
        ),
    }

    let fetch = fetcher::Fetcher::new(cfg.fetcher_url.clone());
    fetch
        .wait_until_healthy(Duration::from_secs(90))
        .await
        .context("the fetch sidecar never came up")?;
    tracing::info!("fetcher healthy at {}", cfg.fetcher_url);

    let crawler = Arc::new(crawl::Crawler::new(pool.clone(), fetch, cfg.crawl_delay));
    crawl::schedule(crawler.clone(), cfg.scheduler_tick);

    let state = api::AppState {
        pool,
        crawler,
        token: cfg.token.clone().map(Arc::from),
    };
    let app = api::router(state, &cfg);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], cfg.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("feedbot listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await
        .context("server error")?;
    Ok(())
}

async fn shutdown() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler")
    };
    let term = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutting down");
}
