//! Bounded artifact downloads with atomic publication and integrity checks.
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use crate::interrupt::StopToken;
use crate::mirror::CheckedPackage;
use anyhow::{Context, Result, ensure};
use reqwest::Client;
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
        // check_package records one CheckedFile per metadata file, in order.
        // Verify that pairing rather than trusting it: a misaligned pair would
        // queue one file's URL and hash under another file's path.
        let files: Vec<_> = package
            .metadata
            .releases
            .iter()
            .flat_map(|release| &release.files)
            .collect();
        ensure!(
            files.len() == package.files.len(),
            "Check results for {} cover {} of {} files",
            package.metadata.package.normalized_name,
            package.files.len(),
            files.len()
        );
        for (file, checked) in files.into_iter().zip(&package.files) {
            ensure!(
                file.local_path == checked.local_path,
                "Check result {} does not match metadata file {}",
                checked.local_path.display(),
                file.local_path.display()
            );
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
    pub min_free: u64,
    /// Maximum time without response data; active transfers have no total deadline.
    pub read_timeout: Duration,
}
impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            concurrency: 8,
            attempts: 3,
            min_free: 1024 * 1024 * 1024,
            read_timeout: crate::http::DOWNLOAD_READ_TIMEOUT,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct WorkerProgress {
    pub package: String,
    pub filename: String,
    pub bytes: u64,
    pub size: u64,
    pub attempt: u32,
    pub state: WorkerState,
    pub retry_delay_seconds: Option<f64>,
}

/// Serialized in snake_case, matching the strings progress files always used.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerState {
    #[default]
    Waiting,
    Downloading,
    Retrying,
    Done,
    Failed,
}

impl WorkerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Downloading => "downloading",
            Self::Retrying => "retrying",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for WorkerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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

#[derive(Debug, Default, Clone, Serialize)]
pub struct DownloadProgress {
    pub discovery: DiscoveryProgress,
    pub total_files: usize,
    pub downloaded: usize,
    pub failed: usize,
    pub not_attempted: usize,
    pub stopped: Option<String>,
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
    pub min_free: u64,
    pub progress: DownloadProgress,
    pub outcomes: Vec<DownloadOutcome>,
}

impl DownloadProgress {
    pub fn completed_files(&self) -> usize {
        self.downloaded + self.failed + self.not_attempted
    }
}

enum Event {
    Stopped(String),
    Abandoned {
        worker: usize,
    },
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

fn format_size(bytes: u64) -> String {
    for (threshold, unit) in [
        (1u64 << 60, "EiB"),
        (1u64 << 50, "PiB"),
        (1u64 << 40, "TiB"),
        (1u64 << 30, "GiB"),
        (1u64 << 20, "MiB"),
        (1u64 << 10, "KiB"),
    ] {
        if bytes >= threshold {
            return format!("{:.1} {unit}", bytes as f64 / threshold as f64);
        }
    }
    format!("{bytes} {}", if bytes == 1 { "byte" } else { "bytes" })
}

/// A run-wide storage failure, with actionable filesystem and reserve details.
#[derive(Debug)]
pub struct StorageExhausted {
    pub path: PathBuf,
    pub needed: u64,
    pub available: u64,
    pub min_free: u64,
    source: Option<anyhow::Error>,
}
impl std::fmt::Display for StorageExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Not enough disk space at {} (need {}, {} free, keeping --min-free {})",
            self.path.display(),
            format_size(self.needed),
            format_size(self.available),
            format_size(self.min_free)
        )?;
        Ok(())
    }
}
impl std::error::Error for StorageExhausted {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|source| source.as_ref())
    }
}

/// The shared queue cannot accept further work; ordinary package errors do not
/// use this type and must not stop discovery.
#[derive(Debug)]
pub struct QueueUnavailable {
    message: String,
    source: Option<anyhow::Error>,
}
impl QueueUnavailable {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }
    fn caused(message: impl Into<String>, source: anyhow::Error) -> Self {
        Self {
            message: message.into(),
            source: Some(source),
        }
    }
}
impl std::fmt::Display for QueueUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)?;
        Ok(())
    }
}
impl std::error::Error for QueueUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|source| source.as_ref())
    }
}

/// Parse bytes or binary K/M/G/T suffixes, optionally followed by B or iB.
pub fn parse_size(value: &str) -> Result<u64> {
    let value = value.trim();
    let end = value.bytes().take_while(u8::is_ascii_digit).count();
    ensure!(
        end > 0,
        "Invalid size {value:?}: use bytes or K, M, G, T (optionally B or iB)"
    );
    let amount: u64 = value[..end].parse().context("Size overflows u64")?;
    let power = match value[end..].to_ascii_uppercase().as_str() {
        "" | "B" => 0,
        "K" | "KB" | "KIB" => 1,
        "M" | "MB" | "MIB" => 2,
        "G" | "GB" | "GIB" => 3,
        "T" | "TB" | "TIB" => 4,
        _ => anyhow::bail!("Invalid size {value:?}: use bytes or K, M, G, T (optionally B or iB)"),
    };
    amount
        .checked_mul(1024u64.pow(power))
        .context("Size overflows u64")
}

fn storage_error(
    error: anyhow::Error,
    path: &Path,
    needed: u64,
    available: u64,
    min_free: u64,
) -> anyhow::Error {
    if error.downcast_ref::<StorageExhausted>().is_some() {
        return error;
    }
    if error.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded
            )
        })
    }) {
        StorageExhausted {
            path: path.into(),
            needed,
            available,
            min_free,
            source: Some(error),
        }
        .into()
    } else {
        error
    }
}

#[derive(Default)]
struct BudgetState {
    reserved: u64,
    stopped: Option<String>,
}
struct Budget {
    state: Mutex<BudgetState>,
    changed: Condvar,
    min_free: u64,
    available: fn(&Path) -> std::io::Result<u64>,
}
impl Budget {
    fn new(min_free: u64) -> Self {
        Self {
            state: Mutex::new(BudgetState::default()),
            changed: Condvar::new(),
            min_free,
            available: |root| fs4::available_space(root),
        }
    }
    fn stop(&self, reason: String) {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped
            .get_or_insert(reason);
        self.changed.notify_all();
    }
    fn retry_wait(&self, delay: Duration) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (state, _) = self
            .changed
            .wait_timeout_while(state, delay, |state| state.stopped.is_none())
            .unwrap_or_else(|p| p.into_inner());
        state.stopped.is_none()
    }
}
struct Reservation {
    bytes: u64,
    budget: Arc<Budget>,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reserved -= self.bytes;
        self.budget.changed.notify_all();
    }
}
fn reserve_space(
    root: &Path,
    budget: &Arc<Budget>,
    path: &Path,
    bytes: u64,
) -> Result<Reservation> {
    let mut state = budget.state.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        if let Some(reason) = &state.stopped {
            return Err(QueueUnavailable::new(format!("queue closed: {reason}")).into());
        }
        let available = (budget.available)(root)?;
        if available >= budget.min_free
            && bytes
                <= available
                    .saturating_sub(budget.min_free)
                    .saturating_sub(state.reserved)
        {
            state.reserved += bytes;
            return Ok(Reservation {
                bytes,
                budget: budget.clone(),
            });
        }
        if state.reserved == 0 {
            return Err(StorageExhausted {
                path: root.join(path),
                needed: bytes,
                available,
                min_free: budget.min_free,
                source: None,
            }
            .into());
        }
        state = budget
            .changed
            .wait(state)
            .unwrap_or_else(|p| p.into_inner());
    }
}

struct DiskQueueState {
    writer: fs::File,
    reader: BufReader<fs::File>,
    pending: usize,
    closed: bool,
    stopped: Option<String>,
}

// Pending jobs are JSON lines on disk rather than a fixed-size channel. The
// mutex covers only local queue I/O; it is never held during a download.
struct DiskQueue {
    state: Mutex<DiskQueueState>,
    ready: Condvar,
    _spool: tempfile::NamedTempFile,
    budget: Arc<Budget>,
}

impl DiskQueue {
    fn new(root: &Path, budget: Arc<Budget>) -> Result<Self> {
        let spool = tempfile::Builder::new()
            .prefix(".snatchrs-queue-")
            .tempfile_in(root)
            .map_err(|error| {
                QueueUnavailable::caused(
                    "Cannot create download queue",
                    storage_error(
                        error.into(),
                        &root.join(".snatchrs-queue"),
                        0,
                        (budget.available)(root).unwrap_or(0),
                        budget.min_free,
                    ),
                )
            })?;
        let state = DiskQueueState {
            // Reopen gives independent read/write offsets; try_clone would share
            // a file offset and corrupt the queue when used for append + read.
            writer: spool.reopen()?,
            reader: BufReader::new(spool.reopen()?),
            pending: 0,
            closed: false,
            stopped: None,
        };
        Ok(Self {
            state: Mutex::new(state),
            ready: Condvar::new(),
            _spool: spool,
            budget,
        })
    }

    fn push(&self, task: &DownloadTask, events: &mpsc::SyncSender<Event>) -> Result<()> {
        let mut record = serde_json::to_vec(task)?;
        record.push(b'\n');
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            return Err(QueueUnavailable::new(format!(
                "queue closed: {}",
                state.stopped.as_deref().unwrap_or("discovery finished")
            ))
            .into());
        }
        state.writer.write_all(&record).map_err(|error| {
            QueueUnavailable::caused(
                "Cannot append download queue",
                storage_error(
                    error.into(),
                    self._spool.path(),
                    record.len() as u64,
                    (self.budget.available)(self._spool.path()).unwrap_or(0),
                    self.budget.min_free,
                ),
            )
        })?;
        // Never block on the event channel while holding the queue lock: the
        // progress consumer may need that lock to stop a full spool safely.
        // Announce before making this record available to a worker.
        drop(state);
        events
            .send(Event::Queued { size: task.size })
            .map_err(|_| QueueUnavailable::new("Progress receiver closed"))?;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.pending += 1;
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    fn next(&self) -> Result<Option<DownloadTask>> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if state.stopped.is_some() {
                return Ok(None);
            }
            if state.pending > 0 {
                let mut record = String::new();
                ensure!(
                    state.reader.read_line(&mut record).map_err(|error| {
                        QueueUnavailable::caused("Cannot read download queue", error.into())
                    })? > 0,
                    QueueUnavailable::new("Download queue was truncated")
                );
                let task = serde_json::from_str(&record).map_err(|error| {
                    QueueUnavailable::caused("Invalid download queue record", error.into())
                })?;
                state.pending -= 1;
                return Ok(Some(task));
            }
            if state.closed {
                return Ok(None);
            }
            state = self.ready.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn stop(&self, reason: String) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.stopped.is_some() {
            return false;
        }
        state.stopped = Some(reason.clone());
        state.closed = true;
        drop(state);
        self.budget.stop(reason);
        self.ready.notify_all();
        true
    }
    fn pending_count(&self) -> usize {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).pending
    }
    fn stop_reason(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stopped
            .clone()
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
    seen_package: Option<String>,
    total_bytes: u64,
}
impl DownloadQueue {
    pub fn discovery(&self, progress: DiscoveryProgress) -> Result<()> {
        self.events
            .send(Event::Discovery(progress))
            .map_err(|_| QueueUnavailable::new("Progress receiver closed").into())
    }

    pub fn stop_reason(&self) -> Option<String> {
        self.jobs.stop_reason()
    }
    pub fn stop(&self, reason: String) {
        if self.jobs.stop(reason.clone()) {
            let _ = self.events.send(Event::Stopped(reason));
        }
    }

    pub fn enqueue(&mut self, mut task: DownloadTask) -> Result<()> {
        if let Some(reason) = self.stop_reason() {
            return Err(QueueUnavailable::new(format!("queue closed: {reason}")).into());
        }
        // Accept existing batch callers' absolute paths under this mirror, but
        // persist and report only paths relative to its root.
        if task.local_path.is_absolute() {
            task.local_path = task
                .local_path
                .strip_prefix(&self.root)
                .context("Download path escapes mirror root")?
                .to_path_buf();
        }
        // The selection is unique and the producer queues one package at a
        // time. Keep conflict detection bounded by the current package.
        if self.seen_package.as_deref() != Some(&task.package) {
            self.seen.clear();
            self.seen_package = Some(task.package.clone());
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
        if let Err(error) = self.jobs.push(&task, &self.events) {
            self.stop(format!("{error:#}"));
            return Err(error);
        }
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
    let (_, report) = stream_with_workers(
        root,
        options,
        options.concurrency.min(tasks.len()),
        &StopToken::default(),
        |queue| {
            for task in tasks {
                if let Err(error) = queue.enqueue(task.clone()) {
                    if error.downcast_ref::<QueueUnavailable>().is_some() {
                        break;
                    }
                    return Err(error);
                }
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
    stream_with_workers(
        root,
        options,
        options.concurrency,
        &StopToken::default(),
        producer,
        progress,
        None,
    )
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
        &StopToken::default(),
        producer,
        progress,
        Some(&mut outcome),
    )
}

/// Interruptible variant for executables with their own signal handler.
pub fn download_stream_to_interruptible<T: Send>(
    root: &Path,
    options: DownloadOptions,
    interrupt: &StopToken,
    producer: impl FnOnce(&mut DownloadQueue) -> Result<T> + Send,
    progress: impl FnMut(&DownloadProgress),
    mut outcome: impl FnMut(&DownloadOutcome) -> Result<()>,
) -> Result<(T, DownloadReport)> {
    stream_with_workers(
        root,
        options,
        options.concurrency,
        interrupt,
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
    interrupt: &StopToken,
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
    let read_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let client = crate::http::transport_builder()
        .gzip(false)
        .read_timeout(options.read_timeout)
        .build()?;
    let mut state = DownloadProgress {
        discovery: DiscoveryProgress::default(),
        total_files: 0,
        downloaded: 0,
        failed: 0,
        not_attempted: 0,
        stopped: None,
        total_bytes: 0,
        verified_bytes: 0,
        transferred_bytes: 0,
        elapsed_seconds: 0.0,
        workers: (0..worker_count)
            .map(|_| WorkerProgress::default())
            .collect(),
    };
    progress(&state);
    let (events, receiver) = mpsc::sync_channel(worker_count.max(1) * 2);
    let budget = Arc::new(Budget::new(options.min_free));
    let jobs = Arc::new(DiskQueue::new(&root, budget.clone())?);
    let queue = DownloadQueue {
        root: root.clone(),
        jobs: jobs.clone(),
        events: events.clone(),
        seen: BTreeMap::new(),
        seen_package: None,
        total_bytes: 0,
    };
    let controller = jobs.clone();
    let start = Instant::now();
    let mut last_update = Instant::now();
    let mut outcomes = Vec::new();
    let mut sink_error = None;
    let mut abandoned_jobs = 0;
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
                loop {
                    if interrupt.is_interrupted() {
                        jobs.stop("Interrupted".into());
                        break;
                    }
                    let task = match jobs.next() {
                        Ok(Some(task)) => task,
                        Ok(None) => break,
                        Err(error) => {
                            let reason = format!("{error:#}");
                            if jobs.stop(reason.clone()) {
                                let _ = events.send(Event::Stopped(reason));
                            }
                            break;
                        }
                    };
                    let mut abandoned = false;
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
                        let result = reserve_space(root, budget, &task.local_path, task.size)
                            .and_then(|_reservation| {
                                read_runtime.block_on(interrupt.run(download_one(
                                    client,
                                    root,
                                    &task,
                                    interrupt,
                                    |bytes| {
                                        let _ = events.send(Event::Bytes { worker, bytes });
                                    },
                                )))
                            });
                        match result {
                            Ok(()) => {
                                outcome.downloaded = true;
                                outcome.sha256_verified = task.sha256.is_some();
                                outcome.error = None;
                                break;
                            }
                            Err(error) => {
                                if interrupt.is_interrupted() {
                                    jobs.stop("Interrupted".into());
                                    abandoned = true;
                                    break;
                                }
                                let error = storage_error(
                                    error,
                                    &root.join(&task.local_path),
                                    task.size,
                                    (budget.available)(root).unwrap_or(0),
                                    options.min_free,
                                );
                                if error.downcast_ref::<QueueUnavailable>().is_some() {
                                    abandoned = true;
                                    break;
                                }
                                outcome.error = Some(format!("{error:#}"));
                                if error.downcast_ref::<StorageExhausted>().is_some() {
                                    let reason = format!("{error:#}");
                                    if jobs.stop(reason.clone()) {
                                        let _ = events.send(Event::Stopped(reason));
                                    }
                                    break;
                                }
                                if jobs.stop_reason().is_some() {
                                    break;
                                }
                                let Some(delay) =
                                    crate::http::retry_delay(attempt, options.attempts, &error)
                                else {
                                    break;
                                };
                                if events.send(Event::Retrying { worker, delay }).is_err() {
                                    return Ok(());
                                }
                                if !budget.retry_wait(delay) {
                                    abandoned = interrupt.is_interrupted();
                                    break;
                                }
                            }
                        }
                    }
                    if abandoned {
                        let _ = events.send(Event::Abandoned { worker });
                        continue;
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
            if interrupt.is_interrupted() {
                controller.stop("Interrupted".into());
                state.stopped = Some("Interrupted".into());
            }
            let event = match receiver.recv_timeout(Duration::from_millis(80)) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if let Some(event) = event {
                match event {
                    Event::Stopped(reason) => {
                        state.stopped.get_or_insert(reason);
                    }
                    Event::Abandoned { worker } => {
                        abandoned_jobs += 1;
                        state.workers[worker].state = WorkerState::Waiting;
                        state.workers[worker].retry_delay_seconds = None;
                    }
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
                            state: WorkerState::Downloading,
                            retry_delay_seconds: None,
                        };
                    }
                    Event::Bytes { worker, bytes } => {
                        state.workers[worker].bytes += bytes;
                        state.transferred_bytes += bytes;
                    }
                    Event::Retrying { worker, delay } => {
                        state.workers[worker].state = WorkerState::Retrying;
                        state.workers[worker].retry_delay_seconds = Some(delay.as_secs_f64());
                        state.workers[worker].bytes = 0;
                    }
                    Event::Finished { worker, outcome } => {
                        if outcome.downloaded {
                            state.downloaded += 1;
                            state.verified_bytes += outcome.task.size;
                            state.workers[worker].state = WorkerState::Done;
                        } else {
                            state.failed += 1;
                            state.workers[worker].state = WorkerState::Failed;
                        }
                        if let Some(sink) = outcome_sink.as_mut() {
                            if sink_error.is_none()
                                && let Err(error) = sink(&outcome)
                            {
                                let error = storage_error(
                                    error,
                                    &root.join("download-outcomes.jsonl"),
                                    0,
                                    (budget.available)(&root).unwrap_or(0),
                                    options.min_free,
                                );
                                if error.downcast_ref::<StorageExhausted>().is_some() {
                                    let reason = format!("{error:#}");
                                    controller.stop(reason.clone());
                                    state.stopped.get_or_insert(reason);
                                }
                                sink_error = Some(error);
                            }
                        } else {
                            outcomes.push(*outcome);
                        }
                    }
                }
            }
            if state.stopped.is_some() {
                state.not_attempted = (controller.pending_count() + abandoned_jobs).min(
                    state
                        .total_files
                        .saturating_sub(state.downloaded + state.failed),
                );
            }
            if last_update.elapsed() >= Duration::from_millis(80)
                || (state.discovery.finished && state.completed_files() == state.total_files)
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
    state.not_attempted = state
        .total_files
        .saturating_sub(state.downloaded + state.failed);
    progress(&state);
    if let Some(error) = sink_error {
        return Err(error.context("Cannot record download outcome"));
    }
    outcomes.sort_by(|a, b| a.task.local_path.cmp(&b.task.local_path));
    Ok((
        produced,
        DownloadReport {
            concurrency: options.concurrency,
            min_free: options.min_free,
            progress: state,
            outcomes,
        },
    ))
}

async fn download_one(
    client: &Client,
    root: &Path,
    task: &DownloadTask,
    interrupt: &StopToken,
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
        .await
        .map_err(crate::http::request_error)?;
    let mut response = crate::http::check_async_response(response)?;
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
    let result: Result<()> = async {
        loop {
            interrupt.check()?;
            let Some(buffer) = response.chunk().await.map_err(crate::http::body_error)? else {
                break;
            };
            let size = buffer.len();
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
        interrupt.check()?;
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
    }
    .await;
    if unreported > 0 {
        progress(unreported);
    }
    result
}

#[cfg(test)]
mod dedupe_tests {
    use super::*;

    #[test]
    fn dedupe_memory_is_scoped_to_one_package_and_conflicts_still_fail() {
        let root = tempfile::tempdir().unwrap();
        let (events, _receiver) = mpsc::sync_channel(4);
        let mut queue = DownloadQueue {
            root: root.path().canonicalize().unwrap(),
            jobs: Arc::new(DiskQueue::new(root.path(), Arc::new(Budget::new(0))).unwrap()),
            events,
            seen: BTreeMap::new(),
            seen_package: None,
            total_bytes: 0,
        };
        for name in ["first", "second", "third"] {
            fs::create_dir_all(root.path().join("packages").join(name)).unwrap();
            let item = DownloadTask {
                package: name.into(),
                filename: "file.whl".into(),
                download_url: "http://127.0.0.1:1/file".parse().unwrap(),
                local_path: Path::new("packages").join(name).join("file.whl"),
                size: 1,
                sha256: None,
            };
            queue.enqueue(item.clone()).unwrap();
            queue.enqueue(item.clone()).unwrap();
            let mut conflicting = item;
            conflicting.size = 2;
            assert!(queue.enqueue(conflicting).is_err());
            assert_eq!(queue.seen.len(), 1);
            assert_eq!(queue.seen_package.as_deref(), Some(name));
        }
        assert_eq!(queue.total_bytes, 3);
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use std::io::{self, ErrorKind};

    fn budget() -> Arc<Budget> {
        Arc::new(Budget {
            available: |_| Ok(10),
            ..Budget::new(2)
        })
    }

    #[test]
    fn formats_storage_sizes_with_one_decimal_binary_units() {
        for (bytes, expected) in [
            (0, "0 bytes"),
            (1, "1 byte"),
            (1023, "1023 bytes"),
            (1024, "1.0 KiB"),
            (18236, "17.8 KiB"),
            (1024 * 1024, "1.0 MiB"),
            (2673950720, "2.5 GiB"),
            (1099511627776000, "1000.0 TiB"),
        ] {
            assert_eq!(format_size(bytes), expected);
        }
        let error = StorageExhausted {
            path: "packages/pkg/file.whl".into(),
            needed: 18236,
            available: 2673950720,
            min_free: 1099511627776000,
            source: None,
        };
        assert!(
            error
                .to_string()
                .contains("need 17.8 KiB, 2.5 GiB free, keeping --min-free 1000.0 TiB")
        );
    }

    #[test]
    fn parses_binary_min_free_sizes_and_rejects_overflows() {
        for (value, expected) in [
            ("0", 0),
            ("1024", 1024),
            ("10K", 10 * 1024),
            ("1G", 1024u64.pow(3)),
            ("1GiB", 1024u64.pow(3)),
            ("2t", 2 * 1024u64.pow(4)),
            ("3mb", 3 * 1024 * 1024),
            ("4kIb", 4096),
        ] {
            assert_eq!(parse_size(value).unwrap(), expected);
        }
        for value in [
            "",
            "garbage",
            "-1",
            "1.5G",
            "1GBB",
            "1Ki",
            "1 P",
            "18446744073709551616",
            "18446744073709551615T",
        ] {
            assert!(parse_size(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn nested_full_and_quota_errors_are_typed_and_never_retried() {
        for kind in [
            ErrorKind::StorageFull,
            ErrorKind::QuotaExceeded,
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
        ] {
            let error = anyhow::Error::new(io::Error::from(kind))
                .context("persist")
                .context("download");
            let error = storage_error(error, Path::new("packages/pkg/file.whl"), 9, 10, 2);
            let full = matches!(kind, ErrorKind::StorageFull | ErrorKind::QuotaExceeded);
            assert_eq!(error.downcast_ref::<StorageExhausted>().is_some(), full);
            if full {
                assert!(crate::http::retry_delay(1, 3, &error).is_none());
                let message = error.to_string();
                for text in [
                    "packages/pkg/file.whl",
                    "9 bytes",
                    "10 bytes",
                    "--min-free 2",
                ] {
                    assert!(message.contains(text), "{message}");
                }
            }
        }
    }

    #[test]
    fn reserves_and_releases_or_reports_that_a_file_cannot_fit_alone() {
        let budget = budget();
        let reservation = reserve_space(Path::new("."), &budget, Path::new("file"), 8).unwrap();
        assert_eq!(budget.state.lock().unwrap().reserved, 8);
        drop(reservation);
        assert_eq!(budget.state.lock().unwrap().reserved, 0);
        let error = reserve_space(Path::new("."), &budget, Path::new("file"), 9)
            .err()
            .unwrap();
        let error = error.downcast_ref::<StorageExhausted>().unwrap();
        assert_eq!((error.needed, error.available, error.min_free), (9, 10, 2));
    }

    #[test]
    fn reservation_waits_for_other_transfers_and_wakes_on_stop() {
        for stopping in [false, true] {
            let budget = budget();
            let held = reserve_space(Path::new("."), &budget, Path::new("first"), 6).unwrap();
            let other = budget.clone();
            let (sent, received) = mpsc::channel();
            let handle = thread::spawn(move || {
                let result = reserve_space(Path::new("."), &other, Path::new("second"), 5);
                sent.send(result.map(|reservation| {
                    drop(reservation);
                }))
                .unwrap();
            });
            assert!(received.recv_timeout(Duration::from_millis(60)).is_err());
            if stopping {
                budget.stop("test shutdown".into());
            } else {
                drop(held);
            }
            let result = received
                .recv_timeout(Duration::from_secs(2))
                .expect("reservation must wake");
            if stopping {
                assert!(
                    result
                        .unwrap_err()
                        .downcast_ref::<QueueUnavailable>()
                        .is_some()
                );
            } else {
                result.unwrap();
            }
            handle.join().unwrap();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn full_spool_errors_close_the_queue_and_preserve_the_storage_cause() {
        // /dev/full deterministically returns ENOSPC without filling a disk.
        let root = tempfile::tempdir().unwrap();
        let budget = Arc::new(Budget::new(0));
        let jobs = Arc::new(DiskQueue::new(root.path(), budget).unwrap());
        jobs.state.lock().unwrap().writer = fs::OpenOptions::new()
            .write(true)
            .open("/dev/full")
            .unwrap();
        let (events, _receiver) = mpsc::sync_channel(4);
        let mut queue = DownloadQueue {
            root: root.path().into(),
            jobs,
            events,
            seen: BTreeMap::new(),
            seen_package: None,
            total_bytes: 0,
        };
        let error = queue
            .enqueue(DownloadTask {
                package: "pkg".into(),
                filename: "file.whl".into(),
                local_path: "file.whl".into(),
                download_url: "http://127.0.0.1:1/file".parse().unwrap(),
                size: 1,
                sha256: None,
            })
            .unwrap_err();
        assert!(error.downcast_ref::<QueueUnavailable>().is_some());
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<StorageExhausted>().is_some())
        );
        assert!(
            queue
                .stop_reason()
                .unwrap()
                .contains("Not enough disk space")
        );
        assert!(queue.jobs.next().unwrap().is_none());
    }
}
