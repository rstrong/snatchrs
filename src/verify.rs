use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::Args;
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};
use serde::Serialize;
use sha2::{Digest, Sha256};
use snatchrs::{
    mirror::{
        DistributionFile, FileState, MirrorLayout, PackageMetadata, inspect_file, resolve_path,
        write_json,
    },
    normalize_name,
    selection::normalize_names,
    simple,
};

#[derive(Args)]
pub struct VerifyArgs {
    /// Mirror directory.
    #[arg(value_name = "DIR")]
    mirror: PathBuf,
    /// Verify only this project; repeat for more names.
    #[arg(long = "package", value_name = "NAME")]
    packages: Vec<String>,
    /// Hashing workers (1–64).
    #[arg(short = 'j', long, value_name = "N", default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=64))]
    concurrency: u16,
    /// Remove same-size files with an incorrect SHA256.
    #[arg(long)]
    remove_bad: bool,
    /// Print the verification report as JSON.
    #[arg(long, help_heading = "Advanced")]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Ok,
    NoHash,
    Missing,
    SizeMismatch,
    HashMismatch,
    Blocked,
    Error,
    ProjectError,
}

#[derive(Default, Serialize)]
struct VerifyReport {
    stale_temp_files_removed: usize,
    stopped: Option<String>,
    projects: usize,
    files: usize,
    bytes_hashed: u64,
    ok: usize,
    no_hash: usize,
    missing: usize,
    size_mismatch: usize,
    hash_mismatch: usize,
    blocked: usize,
    error: usize,
    project_errors: usize,
    removed: usize,
    problems_path: PathBuf,
}
impl VerifyReport {
    fn failures(&self) -> usize {
        self.missing
            + self.size_mismatch
            + self.hash_mismatch
            + self.blocked
            + self.error
            + self.project_errors
    }
    fn record(&mut self, state: State) {
        match state {
            State::Ok => self.ok += 1,
            State::NoHash => self.no_hash += 1,
            State::Missing => self.missing += 1,
            State::SizeMismatch => self.size_mismatch += 1,
            State::HashMismatch => self.hash_mismatch += 1,
            State::Blocked => self.blocked += 1,
            State::Error => self.error += 1,
            State::ProjectError => self.project_errors += 1,
        }
    }
}

#[derive(Serialize)]
struct Problem {
    project: String,
    filename: String,
    local_path: PathBuf,
    state: State,
    detail: String,
}
struct Job {
    project: String,
    file: DistributionFile,
}
struct Outcome {
    state: State,
    detail: String,
    removed: bool,
}
impl Outcome {
    fn new(state: State, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
            removed: false,
        }
    }
}
enum Event {
    Project,
    Swept(usize),
    ProjectError(Problem),
    Bytes(u64),
    File { job: Box<Job>, outcome: Outcome },
}

fn project_problem(project: &str, path: PathBuf, error: impl std::fmt::Display) -> Problem {
    Problem {
        project: project.into(),
        filename: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        local_path: path,
        state: State::ProjectError,
        detail: error.to_string(),
    }
}

fn load_metadata(root: &Path, name: &str) -> Result<PackageMetadata> {
    ensure!(
        normalize_name(name)? == name,
        "Metadata filename is not a normalized project name"
    );
    let path = root.join("metadata").join(format!("{name}.json"));
    ensure!(
        fs::symlink_metadata(&path)?.file_type().is_file(),
        "Expected a regular metadata file: {}",
        path.display()
    );
    let metadata: PackageMetadata = serde_json::from_reader(BufReader::new(File::open(&path)?))
        .with_context(|| format!("Invalid project metadata in {}", path.display()))?;
    ensure!(
        metadata.package.normalized_name == name && normalize_name(&metadata.package.name)? == name,
        "Metadata project name does not match filename"
    );
    Ok(metadata)
}

fn safe_path(root: &Path, job: &Job) -> Result<PathBuf> {
    let filename = Path::new(&job.file.filename);
    ensure!(
        filename.components().count() == 1
            && matches!(filename.components().next(), Some(Component::Normal(_)))
            && !job.file.filename.contains(['/', '\\', ':'])
            && !job.file.filename.chars().any(char::is_control),
        "Invalid distribution filename"
    );
    let expected = Path::new("packages").join(&job.project).join(filename);
    ensure!(
        job.file.local_path == expected,
        "Distribution path is outside its project package directory"
    );
    let path = resolve_path(root, &job.file.local_path)?;
    for parent in [
        root.join("packages"),
        root.join("packages").join(&job.project),
    ] {
        match fs::symlink_metadata(&parent) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_dir(),
                "Expected a real package directory: {}",
                parent.display()
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

fn unchanged_file(original: &fs::Metadata, current: &fs::Metadata) -> bool {
    if !current.file_type().is_file()
        || original.len() != current.len()
        || original.modified().ok() != current.modified().ok()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if original.dev() != current.dev() || original.ino() != current.ino() {
            return false;
        }
    }
    true
}

fn verify_file(
    root: &Path,
    job: &Job,
    remove_bad: bool,
    buffer: &mut [u8],
    mut bytes: impl FnMut(u64),
    interrupt: &snatchrs::interrupt::StopToken,
) -> Outcome {
    let path = match safe_path(root, job) {
        Ok(path) => path,
        Err(error) => return Outcome::new(State::Blocked, format!("{error:#}")),
    };
    match inspect_file(root, &job.file) {
        FileState::Missing => return Outcome::new(State::Missing, "File is missing"),
        FileState::SizeMismatch { actual_size } => {
            return Outcome::new(
                State::SizeMismatch,
                format!("Expected {} bytes, found {actual_size}", job.file.size),
            );
        }
        FileState::NotRegularFile => {
            return Outcome::new(
                State::Blocked,
                "Not a regular file (symlinks are not followed)",
            );
        }
        FileState::Error { message } => return Outcome::new(State::Blocked, message),
        FileState::MatchingSize => {}
    }
    let Some(expected) = &job.file.sha256 else {
        return Outcome::new(State::NoHash, "");
    };
    let hashed = (|| -> Result<(String, fs::Metadata)> {
        let before_open = fs::symlink_metadata(&path)?;
        let mut file = File::open(&path)?;
        let original = file.metadata()?;
        ensure!(
            unchanged_file(&before_open, &original),
            "File changed before hashing"
        );
        ensure!(
            original.file_type().is_file(),
            "File changed to a nonregular file before hashing"
        );
        let mut digest = Sha256::new();
        let mut read = 0u64;
        loop {
            interrupt.check()?;
            let size = file.read(buffer)?;
            if size == 0 {
                break;
            }
            digest.update(&buffer[..size]);
            bytes(size as u64);
            read += size as u64;
            ensure!(read <= job.file.size, "File grew while hashing");
        }
        ensure!(read == job.file.size, "File size changed while hashing");
        Ok((format!("{:x}", digest.finalize()), original))
    })();
    let (actual, original) = match hashed {
        Ok(value) => value,
        Err(error) => return Outcome::new(State::Error, format!("{error:#}")),
    };
    if actual.eq_ignore_ascii_case(expected) {
        return Outcome::new(State::Ok, "");
    }
    let mut outcome = Outcome::new(
        State::HashMismatch,
        format!("Expected SHA256 {expected}, found {actual}"),
    );
    if remove_bad && !interrupt.is_interrupted() {
        // Recheck the path and identity after hashing, so replacing a file or
        // swapping in a symlink cannot make us delete a different object.
        let removed = (|| -> Result<()> {
            safe_path(root, job)?;
            ensure!(
                unchanged_file(&original, &fs::symlink_metadata(&path)?),
                "File changed while hashing; not removed"
            );
            fs::remove_file(&path)?;
            Ok(())
        })();
        match removed {
            Ok(()) => outcome.removed = true,
            Err(error) => outcome
                .detail
                .push_str(&format!("; not removed: {error:#}")),
        }
    }
    outcome
}

fn produce(
    root: &Path,
    names: Vec<String>,
    jobs: mpsc::SyncSender<Job>,
    events: mpsc::SyncSender<Event>,
    cancelled: &AtomicBool,
    interrupt: &snatchrs::interrupt::StopToken,
) {
    let send_project = |name: String| -> Result<()> {
        if cancelled.load(Ordering::Acquire) || interrupt.is_interrupted() {
            return Ok(());
        }
        events.send(Event::Swept(snatchrs::cleanup::sweep_project(root, &name)))?;
        events.send(Event::Project)?;
        match load_metadata(root, &name) {
            Ok(metadata) => {
                for file in metadata
                    .releases
                    .into_iter()
                    .flat_map(|release| release.files)
                {
                    if cancelled.load(Ordering::Acquire) || interrupt.is_interrupted() {
                        break;
                    }
                    jobs.send(Job {
                        project: name.clone(),
                        file,
                    })?;
                }
            }
            Err(error) => events.send(Event::ProjectError(project_problem(
                &name,
                Path::new("metadata").join(format!("{name}.json")),
                format!("{error:#}"),
            )))?,
        }
        Ok(())
    };
    let produced = (|| -> Result<()> {
        ensure!(
            fs::symlink_metadata(root.join("metadata"))?
                .file_type()
                .is_dir(),
            "Expected a real metadata directory"
        );
        if !names.is_empty() {
            for name in names {
                if cancelled.load(Ordering::Acquire) || interrupt.is_interrupted() {
                    break;
                }
                send_project(name)?;
            }
        } else {
            for entry in fs::read_dir(root.join("metadata"))? {
                if cancelled.load(Ordering::Acquire) || interrupt.is_interrupted() {
                    break;
                }
                let entry = entry?;
                if entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "json")
                {
                    continue;
                }
                send_project(
                    entry
                        .path()
                        .file_stem()
                        .context("Metadata has no project name")?
                        .to_string_lossy()
                        .into_owned(),
                )?;
            }
        }
        Ok(())
    })();
    if let Err(error) = produced
        && !interrupt.is_interrupted()
    {
        let _ = events.send(Event::ProjectError(project_problem(
            "<metadata>",
            "metadata".into(),
            format!("{error:#}"),
        )));
    }
}

fn append_problem(writer: &mut BufWriter<File>, problem: &Problem) -> Result<()> {
    serde_json::to_writer(&mut *writer, problem)?;
    writeln!(writer)?;
    writer.flush()?;
    Ok(())
}

pub fn run(
    args: VerifyArgs,
    quiet: bool,
    no_progress: bool,
    interrupt: &snatchrs::interrupt::StopToken,
) -> Result<()> {
    let names = normalize_names(args.packages)?;
    let root = args
        .mirror
        .canonicalize()
        .with_context(|| format!("Cannot open mirror {}", args.mirror.display()))?;
    ensure!(root.is_dir(), "Mirror is not a directory");
    let layout = MirrorLayout { root };
    let _lock = layout.lock()?;
    let reports = layout.root.join("reports");
    match fs::create_dir(&reports) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => ensure!(
            fs::symlink_metadata(&reports)?.file_type().is_dir(),
            "Expected a real reports directory"
        ),
        Err(error) => return Err(error.into()),
    }
    let run = snatchrs::mirror::create_report_directory(&reports, "verify-")?;
    let mut problems = snatchrs::mirror::create_jsonl(&run.join("problems.jsonl"))?;
    let mut report = VerifyReport {
        stale_temp_files_removed: _lock.stale_temp_files_removed,
        problems_path: run.strip_prefix(&layout.root)?.join("problems.jsonl"),
        ..Default::default()
    };
    let bar = crate::interactive(quiet, no_progress, args.json).then(|| {
        let bar = ProgressBar::new_spinner();
        bar.set_style(ProgressStyle::with_template("{spinner:.cyan} snatchrs · Verifying mirror  {elapsed_precise}\n  {human_pos} files checked · {msg}").expect("static progress template"));
        bar.enable_steady_tick(Duration::from_millis(100));
        bar
    });
    let mut last_update = Instant::now();
    let mut last_log = Instant::now();
    let cancelled = AtomicBool::new(false);
    let mut removed_projects = BTreeSet::new();
    let mut journal_error = None;
    thread::scope(|scope| {
        let (sent_jobs, jobs) = mpsc::sync_channel::<Job>(usize::from(args.concurrency) * 2);
        let jobs = Arc::new(Mutex::new(jobs));
        let (sent_events, events) = mpsc::sync_channel(usize::from(args.concurrency) * 2);
        let producer_events = sent_events.clone();
        let root = &layout.root;
        let cancel = &cancelled;
        let producer = scope
            .spawn(move || produce(root, names, sent_jobs, producer_events, cancel, interrupt));
        let mut workers = Vec::new();
        for _ in 0..args.concurrency {
            let jobs = jobs.clone();
            let events = sent_events.clone();
            let cancel = &cancelled;
            workers.push(scope.spawn(move || {
                let mut buffer = vec![0; 1024 * 1024];
                while !cancel.load(Ordering::Acquire) && !interrupt.is_interrupted() {
                    let Ok(job) = jobs.lock().unwrap_or_else(|p| p.into_inner()).recv() else {
                        break;
                    };
                    let outcome = verify_file(
                        root,
                        &job,
                        args.remove_bad,
                        &mut buffer,
                        |bytes| {
                            let _ = events.send(Event::Bytes(bytes));
                        },
                        interrupt,
                    );
                    if interrupt.is_interrupted() && outcome.state == State::Error {
                        break;
                    }
                    if events
                        .send(Event::File {
                            job: Box::new(job),
                            outcome,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }));
        }
        drop(jobs);
        drop(sent_events);
        for event in events {
            let problem = match event {
                Event::Swept(count) => {
                    report.stale_temp_files_removed += count;
                    None
                }
                Event::Project => {
                    report.projects += 1;
                    None
                }
                Event::Bytes(bytes) => {
                    report.bytes_hashed += bytes;
                    None
                }
                Event::ProjectError(problem) => {
                    report.project_errors += 1;
                    Some(problem)
                }
                Event::File { job, outcome } => {
                    report.files += 1;
                    report.record(outcome.state);
                    if outcome.removed {
                        report.removed += 1;
                        removed_projects.insert(job.project.clone());
                    }
                    if matches!(outcome.state, State::Ok | State::NoHash) {
                        None
                    } else {
                        Some(Problem {
                            project: job.project,
                            filename: job.file.filename,
                            local_path: job.file.local_path,
                            state: outcome.state,
                            detail: outcome.detail,
                        })
                    }
                }
            };
            if let Some(problem) = problem
                && journal_error.is_none()
                && let Err(error) = append_problem(&mut problems, &problem)
            {
                journal_error = Some(error);
                cancelled.store(true, Ordering::Release);
            }
            if last_update.elapsed() >= Duration::from_millis(80) {
                if let Some(bar) = &bar {
                    bar.set_position(report.files as u64);
                    bar.set_message(format!(
                        "{} hashed · {} problems",
                        HumanBytes(report.bytes_hashed),
                        report.failures()
                    ));
                } else if !quiet && last_log.elapsed() >= Duration::from_secs(5) {
                    eprintln!(
                        "Verified {} projects, {} files · {} hashed · {} problems",
                        report.projects,
                        report.files,
                        HumanBytes(report.bytes_hashed),
                        report.failures()
                    );
                    last_log = Instant::now();
                }
                last_update = Instant::now();
            }
        }
        for worker in workers {
            if worker.join().is_err() {
                report.project_errors += 1;
                journal_error.get_or_insert_with(|| anyhow::anyhow!("Hashing worker panicked"));
            }
        }
        if producer.join().is_err() {
            report.project_errors += 1;
            journal_error.get_or_insert_with(|| anyhow::anyhow!("Metadata reader panicked"));
        }
    });
    for name in &removed_projects {
        let rewritten = load_metadata(&layout.root, name)
            .and_then(|metadata| simple::write_project_page(&layout.root, &metadata));
        if let Err(error) = rewritten {
            report.project_errors += 1;
            if let Err(error) = append_problem(
                &mut problems,
                &project_problem(
                    name,
                    Path::new("metadata").join(format!("{name}.json")),
                    format!("Cannot update Simple page: {error:#}"),
                ),
            ) {
                journal_error.get_or_insert(error);
            }
        }
    }
    if !removed_projects.is_empty()
        && let Err(error) = simple::write_root_index(&layout.root)
    {
        report.project_errors += 1;
        if let Err(error) = append_problem(
            &mut problems,
            &project_problem("<simple>", "simple/index.html".into(), error),
        ) {
            journal_error.get_or_insert(error);
        }
    }
    if let Err(error) = problems
        .flush()
        .and_then(|()| problems.get_ref().sync_all())
    {
        journal_error.get_or_insert(error.into());
    }
    if journal_error.is_some() {
        report.project_errors += 1;
    }
    if let Some(bar) = bar {
        bar.finish_and_clear();
    }
    report.stopped = interrupt.is_interrupted().then(|| "Interrupted".into());
    snatchrs::cleanup::announce(report.stale_temp_files_removed, quiet);
    let path = layout.root.join("verify-report.json");
    write_json(&path, &report)?;
    let repair = if report.removed > 0 {
        "; run snatchrs mirror with the same selection to fetch them again"
    } else {
        ""
    };
    let mut stdout = BufWriter::new(io::stdout().lock());
    if args.json {
        serde_json::to_writer_pretty(&mut stdout, &report)?;
        writeln!(stdout)?;
    } else {
        writeln!(
            stdout,
            "Verified {} projects, {} files · {} ok · {} no hash · {} missing · {} size mismatch · {} hash mismatch · {} blocked · {} read errors · {} project errors · {} hashed · {} removed{repair}",
            report.projects,
            report.files,
            report.ok,
            report.no_hash,
            report.missing,
            report.size_mismatch,
            report.hash_mismatch,
            report.blocked,
            report.error,
            report.project_errors,
            HumanBytes(report.bytes_hashed),
            report.removed
        )?;
    }
    stdout.flush()?;
    if let Some(error) = journal_error {
        return Err(error.context(format!("Verification report saved to {}", path.display())));
    }
    ensure!(
        report.failures() == 0,
        "Verification found {} problems{repair}; see {}",
        report.failures(),
        path.display()
    );
    Ok(())
}
