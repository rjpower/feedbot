//! Whole-list MOBI exports, run as background jobs.
//!
//! A 100-post export fetches and transcodes hundreds of images through the
//! sidecar and takes minutes — too long to hold a request open, and nicer to
//! watch than a dead spinner. So the request kicks a job off and returns its id,
//! the SPA polls it for progress, and the finished `.mobi` is pulled from a
//! second endpoint. Jobs live in memory: this is a single-user homelab app, and
//! an export is a transient thing you build, download, and forget.

use crate::db::{self, Pool};
use crate::fetcher::Fetcher;
use crate::mobi::{self, Progress};
use anyhow::Result;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How many finished exports to keep for re-download before evicting the oldest.
/// Each retained job holds its whole `.mobi` (tens of MB) in memory.
const KEEP: usize = 3;

/// The registry of export jobs — a handful at most, so a `Vec` scanned by id is
/// plenty. Running jobs are never evicted; finished ones age out past [`KEEP`].
pub struct Exports {
    jobs: Mutex<Vec<Arc<Job>>>,
    seq: AtomicU64,
}

pub struct Job {
    pub id: String,
    pub filename: String,
    label: String,
    seq: u64,
    state: Mutex<JobState>,
}

struct JobState {
    phase: Phase,
    done: usize,
    total: usize,
    /// `None` while running; `Some(Ok)` with the bytes, or `Some(Err)` with the
    /// failure message, once the worker finishes.
    result: Option<Result<Arc<[u8]>, String>>,
}

#[derive(Clone, Copy)]
enum Phase {
    Selecting,
    Fetching,
    Assembling,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Selecting => "selecting",
            Phase::Fetching => "fetching",
            Phase::Assembling => "assembling",
        }
    }
}

/// The JSON the poller sees. Never carries the `.mobi` bytes themselves.
#[derive(Serialize)]
pub struct JobView {
    pub id: String,
    pub label: String,
    /// `running` | `done` | `failed`.
    pub status: &'static str,
    pub phase: &'static str,
    pub done: usize,
    pub total: usize,
    /// Byte size of the finished book, once it exists.
    pub size: Option<usize>,
    pub error: Option<String>,
}

impl Job {
    fn report(&self, p: Progress) {
        let mut st = self.state.lock().expect("export state poisoned");
        match p {
            Progress::Article { done, total } => {
                st.phase = Phase::Fetching;
                st.done = done;
                st.total = total;
            }
            Progress::Assembling => st.phase = Phase::Assembling,
        }
    }

    fn finish(&self, bytes: Vec<u8>) {
        let mut st = self.state.lock().expect("export state poisoned");
        st.result = Some(Ok(bytes.into()));
    }

    fn fail(&self, err: String) {
        let mut st = self.state.lock().expect("export state poisoned");
        st.result = Some(Err(err));
    }

    fn is_finished(&self) -> bool {
        self.state
            .lock()
            .expect("export state poisoned")
            .result
            .is_some()
    }

    /// The finished book, if the job succeeded. `None` while running or failed.
    pub fn bytes(&self) -> Option<Arc<[u8]>> {
        match &self.state.lock().expect("export state poisoned").result {
            Some(Ok(b)) => Some(b.clone()),
            _ => None,
        }
    }

    pub fn view(&self) -> JobView {
        let st = self.state.lock().expect("export state poisoned");
        let (status, size, error) = match &st.result {
            None => ("running", None, None),
            Some(Ok(b)) => ("done", Some(b.len()), None),
            Some(Err(e)) => ("failed", None, Some(e.clone())),
        };
        JobView {
            id: self.id.clone(),
            label: self.label.clone(),
            status,
            phase: st.phase.as_str(),
            done: st.done,
            total: st.total,
            size,
            error,
        }
    }
}

impl Exports {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            seq: AtomicU64::new(0),
        }
    }

    /// Spawn an export in the background and return the job to poll. `pool` and
    /// `fetch` are cheap handles (a connection pool and an HTTP client), cloned
    /// into the worker so the job outlives the request.
    pub fn start(
        &self,
        pool: Pool,
        fetch: Fetcher,
        label: String,
        site_id: Option<i64>,
        per_site: i64,
    ) -> Arc<Job> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let job = Arc::new(Job {
            id: format!("e{seq}"),
            filename: format!("feedbot-{}", mobi::safe_filename(&label)),
            label: label.clone(),
            seq,
            state: Mutex::new(JobState {
                phase: Phase::Selecting,
                done: 0,
                total: 0,
                result: None,
            }),
        });

        {
            let mut jobs = self.jobs.lock().expect("exports lock poisoned");
            jobs.push(job.clone());
            evict(&mut jobs);
        }

        let worker = job.clone();
        tokio::spawn(async move {
            let title = format!("feedbot — {label}");
            match run(&pool, &fetch, &worker, &label, site_id, per_site, &title).await {
                Ok(bytes) => worker.finish(bytes),
                Err(e) => {
                    tracing::warn!(job = %worker.id, "export failed: {e:#}");
                    worker.fail(format!("{e:#}"));
                }
            }
        });
        job
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs
            .lock()
            .expect("exports lock poisoned")
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    /// Recent jobs, newest first — lets the UI re-attach to an export in flight.
    pub fn list(&self) -> Vec<JobView> {
        self.jobs
            .lock()
            .expect("exports lock poisoned")
            .iter()
            .rev()
            .map(|j| j.view())
            .collect()
    }
}

impl Default for Exports {
    fn default() -> Self {
        Self::new()
    }
}

/// Drop finished jobs past the newest [`KEEP`], so retained `.mobi` bytes don't
/// pile up. A running job is never dropped, however old — its worker still holds
/// the `Arc` regardless, but keeping it listed lets the poller keep finding it.
fn evict(jobs: &mut Vec<Arc<Job>>) {
    let stale: Vec<u64> = jobs
        .iter()
        .rev()
        .filter(|j| j.is_finished())
        .skip(KEEP)
        .map(|j| j.seq)
        .collect();
    if !stale.is_empty() {
        jobs.retain(|j| !stale.contains(&j.seq));
    }
}

/// Select the articles, then build the book — the same work the old synchronous
/// handler did, with progress reported to the job as it goes.
async fn run(
    pool: &Pool,
    fetch: &Fetcher,
    job: &Arc<Job>,
    label: &str,
    site_id: Option<i64>,
    per_site: i64,
    title: &str,
) -> Result<Vec<u8>> {
    let state = label.to_string();
    let articles = db::call(pool, move |c| {
        let summaries = db::list_articles_per_site(c, &state, site_id, per_site)?;
        summaries
            .iter()
            .map(|s| {
                db::get_article(c, s.id)?
                    .ok_or_else(|| anyhow::anyhow!("article {} vanished mid-export", s.id))
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })
    .await?;

    anyhow::ensure!(!articles.is_empty(), "no articles match that filter");
    job.report(Progress::Article {
        done: 0,
        total: articles.len(),
    });

    let sink = {
        let job = job.clone();
        move |p| job.report(p)
    };
    mobi::build(&articles, title, pool, fetch, &sink).await
}
