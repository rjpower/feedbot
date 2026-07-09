//! Client for the Node sidecar, plus the supervisor that keeps it alive.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::time::Duration;

#[derive(Clone)]
pub struct Fetcher {
    base: String,
    http: reqwest::Client,
}

#[derive(Debug, Deserialize)]
pub struct Link {
    pub href: String,
    #[allow(dead_code)]
    pub text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    /// The homepage's `<title>` — almost always the name of the blog.
    pub title: String,
    pub feeds: Vec<String>,
    pub links: Vec<Link>,
}

#[derive(Debug, Deserialize)]
pub struct FeedEntry {
    pub url: String,
    pub title: Option<String>,
    pub published: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Feed {
    pub entries: Vec<FeedEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArticleDoc {
    pub final_url: String,
    pub title: String,
    pub byline: Option<String>,
    pub excerpt: Option<String>,
    pub published_time: Option<String>,
    pub html: String,
    pub word_count: i64,
}

/// The sidecar answers `{ok: false, error}` for anything it refuses or fails.
#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

impl Fetcher {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            http: reqwest::Client::builder()
                // Rendering a long article can take a while; the sidecar caps
                // navigation itself, this is just a backstop.
                .timeout(Duration::from_secs(120))
                .build()
                .expect("building reqwest client"),
        }
    }

    async fn post<T: for<'de> Deserialize<'de>>(&self, path: &str, url: &str) -> Result<T> {
        let body = self
            .http
            .post(format!("{}{path}", self.base))
            .json(&serde_json::json!({ "url": url }))
            .send()
            .await
            .with_context(|| format!("POST {path} for {url}"))?
            .text()
            .await?;

        let env: Envelope = serde_json::from_str(&body)
            .with_context(|| format!("{path} returned non-JSON: {}", truncate(&body, 200)))?;
        if !env.ok {
            bail!("{}", env.error.unwrap_or_else(|| "fetcher error".into()));
        }
        serde_json::from_str(&body).with_context(|| {
            format!(
                "{path} payload did not match schema: {}",
                truncate(&body, 200)
            )
        })
    }

    pub async fn discover(&self, url: &str) -> Result<Discovery> {
        self.post("/discover", url).await
    }

    pub async fn feed(&self, url: &str) -> Result<Feed> {
        self.post("/feed", url).await
    }

    pub async fn article(&self, url: &str) -> Result<ArticleDoc> {
        self.post("/article", url).await
    }

    pub async fn healthy(&self) -> bool {
        matches!(
            self.http.get(format!("{}/healthz", self.base)).send().await,
            Ok(r) if r.status().is_success()
        )
    }

    /// Block until the sidecar answers, so we never serve a crawl that can't run.
    pub async fn wait_until_healthy(&self, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            if self.healthy().await {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Err(anyhow!("fetcher at {} never became healthy", self.base))
    }
}

fn truncate(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Run the sidecar as a child process, restarting it if it dies. Chromium is
/// happy to segfault occasionally; a dead sidecar must not mean a dead feedbot.
pub fn supervise(script: String, port: u16) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            tracing::info!("starting fetcher: node {script}");
            let child = tokio::process::Command::new("node")
                .arg(&script)
                .env("FETCHER_PORT", port.to_string())
                .env("FETCHER_HOST", "127.0.0.1")
                .kill_on_drop(true)
                .spawn();

            match child {
                Ok(mut c) => {
                    backoff = Duration::from_secs(1);
                    match c.wait().await {
                        Ok(status) => tracing::error!("fetcher exited: {status}"),
                        Err(e) => tracing::error!("waiting on fetcher: {e}"),
                    }
                }
                Err(e) => {
                    tracing::error!("spawning fetcher: {e}");
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
            tokio::time::sleep(backoff).await;
        }
    });
}
