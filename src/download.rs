//! Bounded artifact downloads with atomic publication and integrity checks.
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use crate::mirror::CheckedPackage;
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadTask {
    pub package: String,
    pub filename: String,
    pub download_url: url::Url,
    pub local_path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
}

pub fn build_tasks(packages: &[CheckedPackage]) -> Result<Vec<DownloadTask>> {
    let mut tasks: BTreeMap<PathBuf, DownloadTask> = BTreeMap::new();
    for package in packages {
        let files = package
            .metadata
            .releases
            .iter()
            .flat_map(|release| &release.files);
        for (file, checked) in files.zip(&package.files) {
            if !checked.state.needs_download() {
                continue;
            }
            let task = DownloadTask {
                package: package.metadata.package.normalized_name.clone(),
                filename: file.filename.clone(),
                download_url: file.download_url.clone(),
                local_path: file.local_path.clone(),
                size: file.size,
                sha256: file.sha256.clone(),
            };
            if let Some(existing) = tasks.get(&task.local_path) {
                ensure!(
                    same_file(existing, &task),
                    "Conflicting metadata for {}",
                    task.local_path.display()
                );
            } else {
                tasks.insert(task.local_path.clone(), task);
            }
        }
    }
    Ok(tasks.into_values().collect())
}

fn same_file(a: &DownloadTask, b: &DownloadTask) -> bool {
    a.size == b.size && a.download_url == b.download_url && a.sha256 == b.sha256
}

#[derive(Debug, Clone, Copy)]
pub struct DownloadOptions {
    pub concurrency: usize,
    pub attempts: u32,
    /// Maximum time without response data; active transfers have no total deadline.
    pub read_timeout: Duration,
}
impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            concurrency: 8,
            attempts: 3,
            read_timeout: Duration::from_secs(900),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct WorkerProgress {
    pub package: String,
    pub filename: String,
    pub bytes: u64,
    pub size: u64,
    pub attempt: u32,
    pub state: String,
    pub retry_delay_seconds: Option<f64>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct DiscoveryProgress {
    pub checked_packages: usize,
    pub total_packages: usize,
    pub current_package: String,
    pub releases: usize,
    pub files: usize,
    pub missing: usize,
    pub size_mismatch: usize,
    pub errors: usize,
    pub unavailable: usize,
    pub finished: bool,
}

#[derive(Debug, Serialize)]
pub struct DownloadProgress {
    pub discovery: DiscoveryProgress,
    pub total_files: usize,
    pub downloaded: usize,
    pub failed: usize,
    pub total_bytes: u64,
    pub verified_bytes: u64,
    pub transferred_bytes: u64,
    pub elapsed_seconds: f64,
    pub workers: Vec<WorkerProgress>,
}
#[derive(Debug, Serialize)]
pub struct DownloadOutcome {
    pub task: DownloadTask,
    pub attempts: u32,
    pub downloaded: bool,
    pub sha256_verified: bool,
    pub error: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct DownloadReport {
    pub concurrency: usize,
    pub progress: DownloadProgress,
    pub outcomes: Vec<DownloadOutcome>,
}

enum Event {
    Queued {
        size: u64,
    },
    Discovery(DiscoveryProgress),
    DiscoveryFinished,
    Started {
        worker: usize,
        package: String,
        filename: String,
        size: u64,
        attempt: u32,
    },
    Bytes {
        worker: usize,
        bytes: u64,
    },
    Retrying {
        worker: usize,
        delay: Duration,
    },
    Finished {
        worker: usize,
        outcome: Box<DownloadOutcome>,
    },
}

// Reserve space only for active transfers. Queued metadata must not prevent the
// checker from advancing, even when a large backlog has accumulated.
struct Reservation {
    bytes: u64,
    budget: Arc<Mutex<u64>>,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        *self.budget.lock().unwrap_or_else(|p| p.into_inner()) -= self.bytes;
    }
}
fn reserve_space(root: &Path, budget: &Arc<Mutex<u64>>, bytes: u64) -> Result<Reservation> {
    let mut reserved = budget.lock().unwrap_or_else(|p| p.into_inner());
    let needed = reserved
        .checked_add(bytes)
        .context("Reserved sizes overflow u64")?;
    ensure!(
        fs2::available_space(root)? >= needed,
        "Not enough disk space for {needed} bytes of active downloads"
    );
    *reserved = needed;
    Ok(Reservation {
        bytes,
        budget: budget.clone(),
    })
}

struct DiskQueueState {
    writer: fs::File,
    reader: BufReader<fs::File>,
    pending: usize,
    closed: bool,
}

// Pending jobs are JSON lines on disk rather than a fixed-size channel. The
// mutex covers only local queue I/O; it is never held during a download.
struct DiskQueue {
    state: Mutex<DiskQueueState>,
    ready: Condvar,
    _spool: tempfile::NamedTempFile,
}

impl DiskQueue {
    fn new(root: &Path) -> Result<Self> {
        let spool = tempfile::Builder::new()
            .prefix(".snatchrs-queue-")
            .tempfile_in(root)?;
        let state = DiskQueueState {
            // Reopen gives independent read/write offsets; try_clone would share
            // a file offset and corrupt the queue when used for append + read.
            writer: spool.reopen()?,
            reader: BufReader::new(spool.reopen()?),
            pending: 0,
            closed: false,
        };
        Ok(Self {
            state: Mutex::new(state),
            ready: Condvar::new(),
            _spool: spool,
        })
    }

    fn push(&self, task: &DownloadTask, events: &mpsc::SyncSender<Event>) -> Result<()> {
        let mut record = serde_json::to_vec(task)?;
        record.push(b'\n');
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(!state.closed, "Download queue is closed");
        state
            .writer
            .write_all(&record)
            .context("Cannot append download queue")?;
        // Announce before allowing a worker to take the job, so progress totals
        // always include a file before its Started/Finished events arrive.
        events
            .send(Event::Queued { size: task.size })
            .context("Progress receiver closed")?;
        state.pending += 1;
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    fn next(&self) -> Result<Option<DownloadTask>> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if state.pending > 0 {
                let mut record = String::new();
                ensure!(
                    state.reader.read_line(&mut record)? > 0,
                    "Download queue was truncated"
                );
                let task =
                    serde_json::from_str(&record).context("Invalid download queue record")?;
                state.pending -= 1;
                return Ok(Some(task));
            }
            if state.closed {
                return Ok(None);
            }
            state = self.ready.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn close(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
        self.ready.notify_all();
    }
}

/// The checker appends pending work to disk and can advance independently of
/// download speed. Only the configured workers hold active jobs in memory.
pub struct DownloadQueue {
    root: PathBuf,
    jobs: Arc<DiskQueue>,
    events: mpsc::SyncSender<Event>,
    seen: BTreeMap<PathBuf, [u8; 32]>,
    total_bytes: u64,
}
impl DownloadQueue {
    pub fn discovery(&self, progress: DiscoveryProgress) -> Result<()> {
        self.events
            .send(Event::Discovery(progress))
            .context("Progress receiver closed")
    }

    pub fn enqueue(&mut self, mut task: DownloadTask) -> Result<()> {
        // Accept existing batch callers' absolute paths under this mirror, but
        // persist and report only paths relative to its root.
        if task.local_path.is_absolute() {
            task.local_path = task
                .local_path
                .strip_prefix(&self.root)
                .context("Download path escapes mirror root")?
                .to_path_buf();
        }
        // Retain only path + fingerprint for deduplication, not another full
        // in-memory copy of every pending task already written to the spool.
        let fingerprint: [u8; 32] = Sha256::digest(serde_json::to_vec(&(
            task.size,
            &task.download_url,
            &task.sha256,
        ))?)
        .into();
        if let Some(existing) = self.seen.get(&task.local_path) {
            ensure!(
                existing == &fingerprint,
                "Conflicting metadata for {}",
                task.local_path.display()
            );
            return Ok(());
        }
        validate_path(&self.root, &task)?;
        let total_bytes = self
            .total_bytes
            .checked_add(task.size)
            .context("Download sizes overflow u64")?;
        self.jobs.push(&task, &self.events)?;
        self.seen.insert(task.local_path, fingerprint);
        self.total_bytes = total_bytes;
        Ok(())
    }
}

impl Drop for DownloadQueue {
    fn drop(&mut self) {
        self.jobs.close();
    }
}

fn validate_path(root: &Path, task: &DownloadTask) -> Result<()> {
    let path = crate::mirror::resolve_path(root, &task.local_path)?;
    let parent = path.parent().context("Download path has no parent")?;
    ensure!(
        parent.canonicalize()?.starts_with(root),
        "Download path escapes mirror root"
    );
    ensure!(
        fs::symlink_metadata(parent)?.file_type().is_dir(),
        "Download parent must be a real directory"
    );
    Ok(())
}

/// Retained batch API; the CLI streams outcomes with download_stream_to.
pub fn download_all(
    root: &Path,
    tasks: &[DownloadTask],
    options: DownloadOptions,
    progress: impl FnMut(&DownloadProgress),
) -> Result<DownloadReport> {
    let total = tasks.iter().try_fold(0u64, |sum, task| {
        sum.checked_add(task.size)
            .context("Download sizes overflow u64")
    })?;
    ensure!(
        fs2::available_space(root)? >= total,
        "Not enough disk space for {total} bytes of downloads"
    );
    let (_, report) = stream_with_workers(
        root,
        options,
        options.concurrency.min(tasks.len()),
        |queue| {
            for task in tasks {
                queue.enqueue(task.clone())?;
            }
            Ok(())
        },
        progress,
        None,
    )?;
    Ok(report)
}

/// Run a producer, fixed download workers, and a responsive progress consumer
/// concurrently. Closing the producer drains all accepted jobs before returning.
pub fn download_stream<T: Send>(
    root: &Path,
    options: DownloadOptions,
    producer: impl FnOnce(&mut DownloadQueue) -> Result<T> + Send,
    progress: impl FnMut(&DownloadProgress),
) -> Result<(T, DownloadReport)> {
    stream_with_workers(root, options, options.concurrency, producer, progress, None)
}

/// Stream completed outcomes to a fallible sink instead of retaining them in RAM.
/// The returned report has an empty `outcomes` vector; counters remain complete.
/// A sink failure drains workers before returning the error.
pub fn download_stream_to<T: Send>(
    root: &Path,
    options: DownloadOptions,
    producer: impl FnOnce(&mut DownloadQueue) -> Result<T> + Send,
    progress: impl FnMut(&DownloadProgress),
    mut outcome: impl FnMut(&DownloadOutcome) -> Result<()>,
) -> Result<(T, DownloadReport)> {
    stream_with_workers(
        root,
        options,
        options.concurrency,
        producer,
        progress,
        Some(&mut outcome),
    )
}

type OutcomeSink<'a> = dyn FnMut(&DownloadOutcome) -> Result<()> + 'a;

fn stream_with_workers<T: Send>(
    root: &Path,
    options: DownloadOptions,
    worker_count: usize,
    producer: impl FnOnce(&mut DownloadQueue) -> Result<T> + Send,
    mut progress: impl FnMut(&DownloadProgress),
    mut outcome_sink: Option<&mut OutcomeSink<'_>>,
) -> Result<(T, DownloadReport)> {
    ensure!(
        (1..=64).contains(&options.concurrency),
        "Concurrency must be between 1 and 64"
    );
    ensure!(
        (1..=10).contains(&options.attempts),
        "Download attempts must be between 1 and 10"
    );
    let root = root.canonicalize()?;
    // Configure per-read stalls on the async transport, then disable the
    // blocking wrapper's separate timeout so active large files can finish.
    let client = reqwest::blocking::ClientBuilder::from(
        crate::http::transport_builder()
            .gzip(false)
            .read_timeout(options.read_timeout),
    )
    .timeout(None)
    .build()?;
    // The blocking response polls the async read-timeout timer on each worker.
    let read_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()?;
    let mut state = DownloadProgress {
        discovery: DiscoveryProgress::default(),
        total_files: 0,
        downloaded: 0,
        failed: 0,
        total_bytes: 0,
        verified_bytes: 0,
        transferred_bytes: 0,
        elapsed_seconds: 0.0,
        workers: (0..worker_count)
            .map(|_| WorkerProgress {
                state: "waiting".into(),
                ..Default::default()
            })
            .collect(),
    };
    progress(&state);
    let (events, receiver) = mpsc::sync_channel(worker_count.max(1) * 2);
    let jobs = Arc::new(DiskQueue::new(&root)?);
    let budget = Arc::new(Mutex::new(0));
    let queue = DownloadQueue {
        root: root.clone(),
        jobs: jobs.clone(),
        events: events.clone(),
        seen: BTreeMap::new(),
        total_bytes: 0,
    };
    let start = Instant::now();
    let mut last_update = Instant::now();
    let mut outcomes = Vec::new();
    let mut sink_error = None;
    let produced = thread::scope(|scope| -> Result<T> {
        let mut handles = Vec::new();
        for worker in 0..worker_count {
            let events = events.clone();
            let jobs = jobs.clone();
            let client = &client;
            let root = &root;
            let budget = &budget;
            let read_runtime = &read_runtime;
            handles.push(scope.spawn(move || -> Result<()> {
                let _runtime = read_runtime.enter();
                loop {
                    let Some(task) = jobs.next()? else {
                        break;
                    };
                    let mut outcome = DownloadOutcome {
                        task: task.clone(),
                        attempts: 0,
                        downloaded: false,
                        sha256_verified: false,
                        error: None,
                    };
                    for attempt in 1..=options.attempts {
                        outcome.attempts = attempt;
                        if events
                            .send(Event::Started {
                                worker,
                                package: task.package.clone(),
                                filename: task.filename.clone(),
                                size: task.size,
                                attempt,
                            })
                            .is_err()
                        {
                            return Ok(());
                        }
                        let result =
                            reserve_space(root, budget, task.size).and_then(|_reservation| {
                                download_one(client, root, &task, |bytes| {
                                    let _ = events.send(Event::Bytes { worker, bytes });
                                })
                            });
                        match result {
                            Ok(()) => {
                                outcome.downloaded = true;
                                outcome.sha256_verified = task.sha256.is_some();
                                outcome.error = None;
                                break;
                            }
                            Err(error) => {
                                outcome.error = Some(format!("{error:#}"));
                                let Some(delay) =
                                    crate::http::retry_delay(attempt, options.attempts, &error)
                                else {
                                    break;
                                };
                                if events.send(Event::Retrying { worker, delay }).is_err() {
                                    return Ok(());
                                }
                                thread::sleep(delay);
                            }
                        }
                    }
                    if events
                        .send(Event::Finished {
                            worker,
                            outcome: Box::new(outcome),
                        })
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Ok(())
            }));
        }
        drop(jobs);
        let produced = scope.spawn(move || {
            let mut queue = queue;
            let result = producer(&mut queue);
            let _ = queue.events.send(Event::DiscoveryFinished);
            result
        });
        drop(events);
        loop {
            let event = match receiver.recv_timeout(Duration::from_millis(80)) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if let Some(event) = event {
                match event {
                    Event::Queued { size } => {
                        state.total_files += 1;
                        state.total_bytes += size;
                    }
                    Event::Discovery(discovery) => state.discovery = discovery,
                    Event::DiscoveryFinished => state.discovery.finished = true,
                    Event::Started {
                        worker,
                        package,
                        filename,
                        size,
                        attempt,
                    } => {
                        state.workers[worker] = WorkerProgress {
                            package,
                            filename,
                            bytes: 0,
                            size,
                            attempt,
                            state: "downloading".into(),
                            retry_delay_seconds: None,
                        };
                    }
                    Event::Bytes { worker, bytes } => {
                        state.workers[worker].bytes += bytes;
                        state.transferred_bytes += bytes;
                    }
                    Event::Retrying { worker, delay } => {
                        state.workers[worker].state = "retrying".into();
                        state.workers[worker].retry_delay_seconds = Some(delay.as_secs_f64());
                        state.workers[worker].bytes = 0;
                    }
                    Event::Finished { worker, outcome } => {
                        if outcome.downloaded {
                            state.downloaded += 1;
                            state.verified_bytes += outcome.task.size;
                            state.workers[worker].state = "done".into();
                        } else {
                            state.failed += 1;
                            state.workers[worker].state = "failed".into();
                        }
                        if let Some(sink) = outcome_sink.as_mut() {
                            if sink_error.is_none()
                                && let Err(error) = sink(&outcome)
                            {
                                sink_error = Some(error);
                            }
                        } else {
                            outcomes.push(*outcome);
                        }
                    }
                }
            }
            if last_update.elapsed() >= Duration::from_millis(80)
                || (state.discovery.finished
                    && state.downloaded + state.failed == state.total_files)
            {
                state.elapsed_seconds = start.elapsed().as_secs_f64();
                progress(&state);
                last_update = Instant::now();
            }
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("Download worker panicked"))??;
        }
        produced
            .join()
            .map_err(|_| anyhow::anyhow!("Metadata producer panicked"))?
    })?;
    state.elapsed_seconds = start.elapsed().as_secs_f64();
    progress(&state);
    if let Some(error) = sink_error {
        return Err(error.context("Cannot record download outcome"));
    }
    outcomes.sort_by(|a, b| a.task.local_path.cmp(&b.task.local_path));
    Ok((
        produced,
        DownloadReport {
            concurrency: options.concurrency,
            progress: state,
            outcomes,
        },
    ))
}

fn download_one(
    client: &Client,
    root: &Path,
    task: &DownloadTask,
    mut progress: impl FnMut(u64),
) -> Result<()> {
    validate_path(root, task)?;
    let path = crate::mirror::resolve_path(root, &task.local_path)?;
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        ensure!(
            metadata.file_type().is_file(),
            "Refusing to replace a non-file path"
        );
    }
    let parent = path.parent().context("Missing download parent")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".snatchrs-")
        .tempfile_in(parent)?;
    let response = client
        .get(task.download_url.clone())
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .map_err(crate::http::request_error)?;
    let mut response = crate::http::check_response(response)?;
    if let Some(length) = response.content_length() {
        ensure!(
            length == task.size,
            "Content-Length {length} differs from expected {}",
            task.size
        );
    }
    let mut hasher = Sha256::new();
    let mut received = 0u64;
    let mut unreported = 0u64;
    let mut buffer = [0u8; 65536];
    let result = (|| -> Result<()> {
        loop {
            let size = response
                .read(&mut buffer)
                .map_err(crate::http::read_error)?;
            if size == 0 {
                break;
            }
            received += size as u64;
            unreported += size as u64;
            ensure!(received <= task.size, "Artifact exceeds expected size");
            temporary.write_all(&buffer[..size])?;
            hasher.update(&buffer[..size]);
            if unreported >= 262144 {
                progress(unreported);
                unreported = 0;
            }
        }
        ensure!(
            received == task.size,
            "Downloaded {received} bytes; expected {}",
            task.size
        );
        if let Some(expected) = &task.sha256 {
            let actual = format!("{:x}", hasher.finalize());
            ensure!(
                actual.eq_ignore_ascii_case(expected),
                "SHA256 mismatch for {}",
                task.filename
            );
        }
        temporary.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        temporary.persist(&path).map_err(|error| error.error)?;
        Ok(())
    })();
    if unreported > 0 {
        progress(unreported);
    }
    result
}
