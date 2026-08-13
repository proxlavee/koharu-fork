use std::sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use futures::TryStreamExt;
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncSeekExt, AsyncWriteExt},
    sync::{Semaphore, broadcast},
    task::JoinSet,
};

use crate::network::{DownloadClient, download_client};

const EVENT_CAPACITY: usize = 256;
const MAX_CONCURRENT_PARTS: usize = 8;
const MAX_PARTS_PER_TRANSFER: usize = 4;
const MIN_PART_SIZE: u64 = 8 * 1024 * 1024;
const MAX_PART_SIZE: u64 = 64 * 1024 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static EVENTS: LazyLock<broadcast::Sender<Event>> =
    LazyLock::new(|| broadcast::channel(EVENT_CAPACITY).0);
static PART_PERMITS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_PARTS);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Event {
    Started {
        id: u64,
        name: String,
    },
    Progress {
        id: u64,
        name: String,
        completed: u64,
        total: u64,
    },
    Finished {
        id: u64,
    },
    Failed {
        id: u64,
        name: String,
        error: String,
    },
}

#[must_use]
pub fn subscribe() -> broadcast::Receiver<Event> {
    EVENTS.subscribe()
}

pub(crate) struct Transfer {
    client: DownloadClient,
}

struct ProgressReporter {
    id: u64,
    name: String,
    total: u64,
    completed: AtomicU64,
    state: Mutex<ProgressState>,
}

#[derive(Default)]
struct ProgressState {
    last_completed: u64,
    last_published: Option<Instant>,
}

impl ProgressReporter {
    fn new(id: u64, name: &str, total: u64) -> Self {
        Self {
            id,
            name: name.to_owned(),
            total,
            completed: AtomicU64::new(0),
            state: Mutex::new(ProgressState::default()),
        }
    }

    fn advance(&self, amount: u64) {
        let completed = self.completed.fetch_add(amount, Ordering::Relaxed) + amount;
        self.report(completed, false);
    }

    fn completed(&self) -> u64 {
        self.completed.load(Ordering::Relaxed)
    }

    fn finish(&self) {
        self.report(self.completed(), true);
    }

    fn report(&self, completed: u64, force: bool) {
        let publish_now = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .should_publish(completed, self.total, Instant::now(), force);
        if publish_now {
            publish(Event::Progress {
                id: self.id,
                name: self.name.clone(),
                completed,
                total: self.total,
            });
        }
    }
}

impl ProgressState {
    fn should_publish(&mut self, completed: u64, total: u64, now: Instant, force: bool) -> bool {
        if completed <= self.last_completed {
            return false;
        }
        let complete = total > 0 && completed >= total;
        let due = self
            .last_published
            .is_none_or(|last| now.saturating_duration_since(last) >= PROGRESS_INTERVAL);
        if !force && !complete && !due {
            return false;
        }
        self.last_completed = completed;
        self.last_published = Some(now);
        true
    }
}

impl Transfer {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            client: download_client()?,
        })
    }

    pub(crate) fn get(&self, url: &str) -> reqwest_middleware::RequestBuilder {
        self.client.get(url)
    }

    pub(crate) async fn fetch(&self, url: &str, destination: &std::path::Path) -> Result<()> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let name = display_name(url);
        publish(Event::Started {
            id,
            name: name.clone(),
        });

        let result = self.fetch_inner(id, &name, url, destination).await;
        match result {
            Ok(()) => {
                publish(Event::Finished { id });
                Ok(())
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(destination).await;
                publish(Event::Failed {
                    id,
                    name,
                    error: error.to_string(),
                });
                Err(error)
            }
        }
    }

    async fn fetch_inner(
        &self,
        id: u64,
        name: &str,
        url: &str,
        destination: &std::path::Path,
    ) -> Result<()> {
        let probe = self
            .client
            .head(url)
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .with_context(|| format!("failed to inspect {url}"))?
            .error_for_status()
            .with_context(|| format!("failed to inspect {url}"))?;
        let total = probe.content_length();
        let ranged = probe
            .headers()
            .get(header::ACCEPT_RANGES)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("bytes"));

        if let Some(total) = total
            && total > 0
            && ranged
        {
            self.fetch_parts(id, name, url, destination, total).await
        } else {
            self.fetch_stream(id, name, url, destination, total).await
        }
    }

    async fn fetch_stream(
        &self,
        id: u64,
        name: &str,
        url: &str,
        destination: &std::path::Path,
        total: Option<u64>,
    ) -> Result<()> {
        let response = self
            .client
            .get(url)
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await?
            .error_for_status()?;
        let total = total.or(response.content_length()).unwrap_or(0);
        let mut file = tokio::fs::File::create(destination).await?;
        let progress = ProgressReporter::new(id, name, total);
        let mut body = response.bytes_stream();
        while let Some(bytes) = body.try_next().await? {
            file.write_all(&bytes).await?;
            progress.advance(bytes.len() as u64);
        }
        file.flush().await?;
        let completed = progress.completed();
        if total > 0 {
            ensure!(
                completed == total,
                "{url} ended after {completed} of {total} bytes"
            );
        }
        progress.finish();
        Ok(())
    }

    async fn fetch_parts(
        &self,
        id: u64,
        name: &str,
        url: &str,
        destination: &std::path::Path,
        total: u64,
    ) -> Result<()> {
        tokio::fs::File::create(destination)
            .await?
            .set_len(total)
            .await?;

        let concurrency = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(MAX_PARTS_PER_TRANSFER)
            .clamp(2, MAX_PARTS_PER_TRANSFER);
        let part_size = total
            .div_ceil((concurrency * 4) as u64)
            .clamp(MIN_PART_SIZE, MAX_PART_SIZE);
        let progress = Arc::new(ProgressReporter::new(id, name, total));
        let mut tasks = JoinSet::new();

        for start in (0..total).step_by(part_size as usize) {
            while tasks.len() >= concurrency {
                let task = tasks
                    .join_next()
                    .await
                    .context("download task disappeared")?;
                task.context("download task failed")??;
            }
            let end = (start + part_size).min(total) - 1;
            tasks.spawn(fetch_part(
                self.client.clone(),
                url.to_owned(),
                destination.to_owned(),
                start,
                end,
                progress.clone(),
            ));
        }
        while let Some(result) = tasks.join_next().await {
            result.context("download task failed")??;
        }
        ensure!(
            progress.completed() == total,
            "{url} download was incomplete"
        );
        progress.finish();
        Ok(())
    }
}

async fn fetch_part(
    client: DownloadClient,
    url: String,
    destination: std::path::PathBuf,
    start: u64,
    end: u64,
    progress: Arc<ProgressReporter>,
) -> Result<()> {
    let _permit = PART_PERMITS
        .acquire()
        .await
        .context("download part limiter closed")?;
    let response = client
        .get(&url)
        .header(header::RANGE, format!("bytes={start}-{end}"))
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await?;
    ensure!(
        response.status() == StatusCode::PARTIAL_CONTENT,
        "{url} did not honor byte range {start}-{end}"
    );
    let actual_range = response
        .headers()
        .get(header::CONTENT_RANGE)
        .context("partial response omitted Content-Range")?
        .to_str()?;
    let expected_range = format!("bytes {start}-{end}/{}", progress.total);
    ensure!(
        actual_range == expected_range,
        "{url} returned Content-Range {actual_range}, expected {expected_range}"
    );
    let expected = end - start + 1;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(destination)
        .await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut received = 0;
    let mut body = response.bytes_stream();
    while let Some(bytes) = body.try_next().await? {
        let amount = bytes.len() as u64;
        ensure!(
            received + amount <= expected,
            "{url} returned more than {expected} bytes for range {start}-{end}"
        );
        file.write_all(&bytes).await?;
        received += amount;
        progress.advance(amount);
    }
    file.flush().await?;
    ensure!(
        received == expected,
        "{url} returned {received} bytes for a {expected}-byte range"
    );
    Ok(())
}

fn publish(event: Event) {
    let _ = EVENTS.send(event);
}

fn display_name(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "download".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_downloads_from_the_url_path() {
        assert_eq!(
            display_name("https://example.test/a/model.bin?q=1"),
            "model.bin"
        );
        assert_eq!(display_name("https://example.test/"), "download");
    }

    #[test]
    fn subscribers_observe_events() {
        let mut receiver = subscribe();
        let event = Event::Finished { id: 42 };
        publish(event.clone());
        assert_eq!(receiver.try_recv().unwrap(), event);
    }

    #[test]
    fn progress_updates_are_throttled_without_hiding_completion() {
        let started = Instant::now();
        let mut state = ProgressState::default();

        assert!(state.should_publish(1, 100, started, false));
        assert!(!state.should_publish(2, 100, started + PROGRESS_INTERVAL / 2, false));
        assert!(state.should_publish(3, 100, started + PROGRESS_INTERVAL, false));
        assert!(!state.should_publish(2, 100, started + PROGRESS_INTERVAL * 2, false));
        assert!(state.should_publish(100, 100, started + PROGRESS_INTERVAL, false));
        assert!(!state.should_publish(100, 100, started + PROGRESS_INTERVAL * 2, true));
    }
}
