//! Bounded artifact downloads with atomic publication and integrity checks.
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use crate::mirror::{CheckedPackage, FileState};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize)]
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
    pub timeout: Duration,
}
impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            concurrency: 8,
            attempts: 3,
            timeout: Duration::from_secs(900),
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
    Finished {
        worker: usize,
        outcome: Box<DownloadOutcome>,
    },
}

// Reservations conservatively cover the full size of queued and active files.
// They are released even on failed sends, failed downloads, or worker unwinding.
struct Reservation {
    bytes: u64,
    budget: Arc<Mutex<u64>>,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        *self.budget.lock().unwrap_or_else(|p| p.into_inner()) -= self.bytes;
    }
}
struct QueuedTask {
    task: DownloadTask,
    _reservation: Reservation,
}

/// The checker feeds this bounded queue as soon as a package has been inspected.
/// Sending applies backpressure while workers continue downloading.
pub struct DownloadQueue {
    root: PathBuf,
    sender: mpsc::SyncSender<QueuedTask>,
    events: mpsc::SyncSender<Event>,
    budget: Arc<Mutex<u64>>,
    seen: BTreeMap<PathBuf, DownloadTask>,
    total_bytes: u64,
}
impl DownloadQueue {
    pub fn discovery(&self, progress: DiscoveryProgress) -> Result<()> {
        self.events
            .send(Event::Discovery(progress))
            .context("Progress receiver closed")
    }

    pub fn enqueue(&mut self, task: DownloadTask) -> Result<()> {
        if let Some(existing) = self.seen.get(&task.local_path) {
            ensure!(
                same_file(existing, &task),
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
        let mut reserved = self.budget.lock().unwrap_or_else(|p| p.into_inner());
        let needed = reserved
            .checked_add(task.size)
            .context("Reserved sizes overflow u64")?;
        ensure!(
            fs2::available_space(&self.root)? >= needed,
            "Not enough disk space for {needed} bytes of queued and active downloads"
        );
        *reserved = needed;
        drop(reserved);
        let reservation = Reservation {
            bytes: task.size,
            budget: self.budget.clone(),
        };
        self.events
            .send(Event::Queued { size: task.size })
            .context("Progress receiver closed")?;
        self.seen.insert(task.local_path.clone(), task.clone());
        self.total_bytes = total_bytes;
        self.sender
            .send(QueuedTask {
                task,
                _reservation: reservation,
            })
            .context("Download workers stopped")?;
        Ok(())
    }
}

fn validate_path(root: &Path, task: &DownloadTask) -> Result<()> {
    let parent = task
        .local_path
        .parent()
        .context("Download path has no parent")?;
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

/// Retained batch API; the CLI uses download_stream to overlap checking and I/O.
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
    )?;
    Ok(report)
}

/// Run a producer, bounded download workers, and a responsive progress consumer
/// concurrently. Closing the producer drains all accepted jobs before returning.
pub fn download_stream<T: Send>(
    root: &Path,
    options: DownloadOptions,
    producer: impl FnOnce(&mut DownloadQueue) -> Result<T> + Send,
    progress: impl FnMut(&DownloadProgress),
) -> Result<(T, DownloadReport)> {
    stream_with_workers(root, options, options.concurrency, producer, progress)
}

fn stream_with_workers<T: Send>(
    root: &Path,
    options: DownloadOptions,
    worker_count: usize,
    producer: impl FnOnce(&mut DownloadQueue) -> Result<T> + Send,
    mut progress: impl FnMut(&DownloadProgress),
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
    let client = Client::builder()
        .gzip(false)
        .user_agent(concat!("snatchrs/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(options.timeout)
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
    let (sender, jobs) = mpsc::sync_channel(worker_count.max(1) * 4);
    let jobs = Arc::new(Mutex::new(jobs));
    let queue = DownloadQueue {
        root,
        sender,
        events: events.clone(),
        budget: Arc::new(Mutex::new(0)),
        seen: BTreeMap::new(),
        total_bytes: 0,
    };
    let start = Instant::now();
    let mut last_update = Instant::now();
    let mut outcomes = Vec::new();
    let produced = thread::scope(|scope| -> Result<T> {
        let mut handles = Vec::new();
        for worker in 0..worker_count {
            let events = events.clone();
            let jobs = jobs.clone();
            let client = &client;
            handles.push(scope.spawn(move || {
                loop {
                    let job = jobs.lock().unwrap_or_else(|p| p.into_inner()).recv();
                    let Ok(QueuedTask { task, _reservation }) = job else {
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
                            return;
                        }
                        match download_one(client, &task, |bytes| {
                            let _ = events.send(Event::Bytes { worker, bytes });
                        }) {
                            Ok(()) => {
                                outcome.downloaded = true;
                                outcome.sha256_verified = task.sha256.is_some();
                                outcome.error = None;
                                break;
                            }
                            Err(error) => outcome.error = Some(format!("{error:#}")),
                        }
                        if attempt < options.attempts {
                            thread::sleep(Duration::from_millis(250 * u64::from(attempt)));
                        }
                    }
                    if events
                        .send(Event::Finished {
                            worker,
                            outcome: Box::new(outcome),
                        })
                        .is_err()
                    {
                        return;
                    }
                    drop(_reservation);
                }
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
                        };
                    }
                    Event::Bytes { worker, bytes } => {
                        state.workers[worker].bytes += bytes;
                        state.transferred_bytes += bytes;
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
                        outcomes.push(*outcome);
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
                .map_err(|_| anyhow::anyhow!("Download worker panicked"))?;
        }
        produced
            .join()
            .map_err(|_| anyhow::anyhow!("Metadata producer panicked"))?
    })?;
    state.elapsed_seconds = start.elapsed().as_secs_f64();
    progress(&state);
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

fn download_one(client: &Client, task: &DownloadTask, mut progress: impl FnMut(u64)) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(&task.local_path) {
        ensure!(
            metadata.file_type().is_file(),
            "Refusing to replace a non-file path"
        );
    }
    let parent = task
        .local_path
        .parent()
        .context("Missing download parent")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".snatchrs-")
        .tempfile_in(parent)?;
    let mut response = client
        .get(task.download_url.clone())
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()?
        .error_for_status()?;
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
            let size = response.read(&mut buffer)?;
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
        temporary
            .persist(&task.local_path)
            .map_err(|error| error.error)?;
        Ok(())
    })();
    if unreported > 0 {
        progress(unreported);
    }
    result
}

/// Refresh the final file checks after publication, preserving the selection.
pub fn final_summary(packages: &[CheckedPackage]) -> crate::mirror::CheckSummary {
    let mut summary = crate::mirror::CheckSummary::default();
    for package in packages {
        summary.packages += 1;
        summary.releases += package.metadata.releases.len();
        for file in package
            .metadata
            .releases
            .iter()
            .flat_map(|release| &release.files)
        {
            summary.files += 1;
            summary.expected_bytes += file.size;
            match crate::mirror::inspect_file(file) {
                FileState::Missing => summary.missing += 1,
                FileState::SizeMismatch { .. } => summary.size_mismatch += 1,
                FileState::MatchingSize => summary.matching_size += 1,
                FileState::NotRegularFile | FileState::Error { .. } => summary.blocked += 1,
            }
        }
    }
    summary
}
