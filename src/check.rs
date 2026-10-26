use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::{ArgGroup, Args};
use serde::Serialize;
use snatchrs::{
    DEFAULT_INDEX_URL, IndexClient, IndexProgress, Package,
    mirror::{
        CheckSummary, CheckedPackage, MetadataOutcome, MirrorLayout, check_package,
        sample_packages, write_json,
    },
};
use url::Url;

#[derive(Args)]
#[command(group(ArgGroup::new("selection").required(true).multiple(false)
    .args(["all", "packages", "packages_file", "sample"])))]
pub struct CheckArgs {
    /// Mirror directory.
    #[arg(value_name = "DIR")]
    mirror: PathBuf,
    /// Select every project in the index.
    #[arg(long)]
    all: bool,
    /// Select a project; repeat for more names.
    #[arg(long = "package", value_name = "NAME")]
    packages: Vec<String>,
    /// Read project names from a file.
    #[arg(long, value_name = "PATH")]
    packages_file: Option<PathBuf>,
    /// Select N random projects.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    sample: Option<u32>,
    /// Seed for repeatable sampling.
    #[arg(long, requires = "sample", conflicts_with_all = ["all", "packages", "packages_file"])]
    seed: Option<u64>,
    /// Root Simple Index URL.
    #[arg(long, value_name = "URL", default_value = DEFAULT_INDEX_URL, help_heading = "Advanced")]
    index_url: Url,
    /// Override the project metadata base URL.
    #[arg(long, value_name = "URL", help_heading = "Advanced")]
    metadata_url: Option<Url>,
    /// Metadata workers (1–32).
    #[arg(long, value_name = "N", default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=32), help_heading = "Advanced")]
    metadata_concurrency: u16,
    /// Fetch fresh project metadata.
    #[arg(long, help_heading = "Advanced")]
    refresh: bool,
    /// Print the summary report as JSON.
    #[arg(long, help_heading = "Advanced")]
    json: bool,
}

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SelectionMode {
    All,
    Packages,
    PackagesFile,
    Sample,
}

struct MetadataOptions {
    base: Url,
    refresh: bool,
    concurrency: usize,
}

// Fetches finish out of order. Only this bounded channel crosses back to the
// single journal/check/queue consumer, so a slow project cannot block its peers
// or cause an unbounded accumulation of completed metadata in memory.
fn parallel_metadata(
    packages: Vec<Package>,
    client: &IndexClient,
    layout: &MirrorLayout,
    options: &MetadataOptions,
    mut consume: impl FnMut(String, Result<MetadataOutcome>) -> Result<bool>,
) -> Result<()> {
    let workers = options.concurrency.min(packages.len());
    let jobs = Mutex::new(packages.into_iter());
    let cancelled = AtomicBool::new(false);
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::sync_channel(workers.max(1));
        let mut handles = Vec::new();
        for _ in 0..workers {
            let sender = sender.clone();
            let jobs = &jobs;
            let cancelled = &cancelled;
            handles.push(scope.spawn(move || {
                while !cancelled.load(Ordering::Acquire) {
                    let Some(package) = jobs.lock().unwrap_or_else(|p| p.into_inner()).next()
                    else {
                        break;
                    };
                    let name = package.normalized_name.clone();
                    let result = client.load_metadata(
                        package,
                        options.base.clone(),
                        layout,
                        options.refresh,
                    );
                    if sender.send((name, result)).is_err() {
                        break;
                    }
                }
            }));
        }
        drop(sender);
        let consumed = (|| {
            for (name, result) in &receiver {
                if !consume(name, result)? {
                    break;
                }
            }
            Ok(())
        })();
        cancelled.store(true, Ordering::Release);
        // Release blocked sends before joining, including on report/queue errors.
        drop(receiver);
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("Metadata worker panicked"))?;
        }
        consumed
    })
}

#[derive(Serialize)]
struct PackageError {
    name: String,
    error: String,
}

#[derive(Clone, Serialize)]
struct CheckReport {
    mirror_root: PathBuf,
    metadata_concurrency: usize,
    seed: Option<u64>,
    selection: SelectionMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    packages_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_packages: Option<Vec<String>>,
    summary: CheckSummary,
    packages_path: PathBuf,
    errors_path: PathBuf,
    error_count: usize,
    checked: usize,
    not_inspected: usize,
    queue_error: Option<String>,
    unavailable_path: PathBuf,
    unavailable_count: usize,
}

// Each run owns its detail files so atomic summary replacement never points an
// older report at truncated or partially overwritten details from a newer run.
struct InspectionJournal {
    packages: BufWriter<File>,
    errors: BufWriter<File>,
    unavailable: BufWriter<File>,
}

fn append_jsonl(file: &mut BufWriter<File>, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *file, value)?;
    writeln!(file)?;
    file.flush()?;
    Ok(())
}

fn record_error(
    report: &mut CheckReport,
    journal: &mut InspectionJournal,
    error: PackageError,
) -> Result<()> {
    report.error_count += 1;
    append_jsonl(&mut journal.errors, &error)?;
    Ok(())
}

fn record_unavailable(
    report: &mut CheckReport,
    journal: &mut InspectionJournal,
    name: &str,
    metadata_url: Url,
) -> Result<()> {
    report.unavailable_count += 1;
    append_jsonl(
        &mut journal.unavailable,
        &serde_json::json!({
            "name": name, "status": "not_found", "http_status": 404,
            "metadata_url": metadata_url,
        }),
    )?;
    Ok(())
}

fn visit_packages(
    path: &std::path::Path,
    mut visit: impl FnMut(CheckedPackage) -> Result<()>,
) -> Result<()> {
    for line in BufReader::new(File::open(path)?).lines() {
        let package =
            serde_json::from_str(&line?).context("Invalid checked-package journal record")?;
        visit(package)?;
    }
    Ok(())
}

#[derive(Args)]
pub struct MirrorArgs {
    #[command(flatten)]
    selection: CheckArgs,
    /// Download workers (1–64).
    #[arg(short = 'j', long, value_name = "N", default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    concurrency: u16,
    /// Free space to keep (binary units).
    #[arg(long, value_name = "SIZE", default_value = "1G", value_parser = snatchrs::download::parse_size, help_heading = "Advanced")]
    min_free: u64,
}

pub fn run_mirror(args: MirrorArgs, quiet: bool, no_progress: bool, attempts: u32) -> Result<()> {
    run_inner(
        args.selection,
        quiet,
        no_progress,
        Some(snatchrs::download::DownloadOptions {
            concurrency: usize::from(args.concurrency),
            attempts,
            min_free: args.min_free,
            ..Default::default()
        }),
        attempts,
    )
}

pub fn run(args: CheckArgs, quiet: bool, no_progress: bool, attempts: u32) -> Result<()> {
    run_inner(args, quiet, no_progress, None, attempts)
}

fn run_inner(
    args: CheckArgs,
    quiet: bool,
    no_progress: bool,
    download: Option<snatchrs::download::DownloadOptions>,
    attempts: u32,
) -> Result<()> {
    let start = Instant::now();
    let interactive = crate::interactive(quiet, no_progress, args.json);
    let mut dashboard = interactive.then(crate::progress::Dashboard::new);
    let layout = MirrorLayout::create(&args.mirror)?;
    let _lock = layout.lock()?;
    let client = IndexClient::with_attempts(attempts)?;
    let metadata_url = args
        .metadata_url
        .clone()
        .unwrap_or_else(|| args.index_url.clone());
    let metadata_options = MetadataOptions {
        base: metadata_url,
        refresh: args.refresh,
        concurrency: usize::from(args.metadata_concurrency),
    };
    let seed = args.sample.map(|_| args.seed.unwrap_or_else(rand::random));
    let selection = if args.all {
        SelectionMode::All
    } else if args.sample.is_some() {
        SelectionMode::Sample
    } else if args.packages_file.is_some() {
        SelectionMode::PackagesFile
    } else {
        SelectionMode::Packages
    };
    let mut authoritative_names = None;
    let packages = if args.all || args.sample.is_some() {
        let mut index =
            client.fetch_with_progress(args.index_url.clone(), |event: IndexProgress<'_>| {
                if let Some(dashboard) = dashboard.as_mut() {
                    dashboard.update(event);
                }
            })?;
        // Deduplicate in place; --all moves the index's existing package vector
        // into the worker pool rather than cloning it or recording every name.
        let mut names = HashSet::new();
        index
            .packages
            .retain(|package| names.insert(package.normalized_name.clone()));
        if args.all {
            if download.is_some() {
                authoritative_names = Some(names);
            }
            index.packages
        } else {
            sample_packages(
                &index,
                args.sample.context("Missing sample size")? as usize,
                seed.context("Missing sample seed")?,
            )
        }
    } else {
        let names = if let Some(path) = &args.packages_file {
            let names = snatchrs::selection::names_from_reader(BufReader::new(
                File::open(path)
                    .with_context(|| format!("Cannot open packages file {}", path.display()))?,
            ))
            .with_context(|| format!("Cannot read packages file {}", path.display()))?;
            ensure!(
                !names.is_empty(),
                "No project names found in {}",
                path.display()
            );
            names
        } else {
            snatchrs::selection::normalize_names(args.packages)?
        };
        let base = snatchrs::normalize_index_url(args.index_url)?;
        names
            .into_iter()
            .map(|name| {
                Ok(Package {
                    index_url: base.join(&format!("{name}/"))?,
                    normalized_name: name.clone(),
                    name,
                    last_serial: None,
                })
            })
            .collect::<Result<Vec<_>>>()?
    };
    let reports = layout.root.join("reports");
    fs::create_dir_all(&reports)?;
    let run_dir = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(reports)?
        .keep();
    let run_dir = run_dir.strip_prefix(&layout.root)?.to_path_buf();
    let packages_path = run_dir.join("checked-packages.jsonl");
    let errors_path = run_dir.join("package-errors.jsonl");
    let unavailable_path = run_dir.join("unavailable-projects.jsonl");
    let mut journal = InspectionJournal {
        packages: BufWriter::new(File::create(layout.root.join(&packages_path))?),
        errors: BufWriter::new(File::create(layout.root.join(&errors_path))?),
        unavailable: BufWriter::new(File::create(layout.root.join(&unavailable_path))?),
    };
    let mut report = CheckReport {
        mirror_root: PathBuf::from("."),
        metadata_concurrency: metadata_options.concurrency,
        seed,
        selection,
        packages_file: args.packages_file,
        selected_packages: (selection != SelectionMode::All).then(|| {
            packages
                .iter()
                .map(|package| package.normalized_name.clone())
                .collect()
        }),
        summary: CheckSummary::default(),
        packages_path,
        errors_path,
        error_count: 0,
        checked: 0,
        not_inspected: 0,
        queue_error: None,
        unavailable_path,
        unavailable_count: 0,
    };
    if let Some(options) = download {
        if let Some(dashboard) = &dashboard {
            dashboard.clear();
        }
        return run_pipeline(
            &layout,
            &client,
            packages,
            (report, journal, authoritative_names),
            metadata_options,
            options,
            OutputOptions {
                quiet,
                no_progress,
                json: args.json,
            },
        );
    }
    let total = packages.len();
    let mut completed = 0;
    if let Some(dashboard) = &dashboard {
        dashboard.checking(0, total, "fetching metadata", &report.summary, 0);
    }
    parallel_metadata(
        packages,
        &client,
        &layout,
        &metadata_options,
        |name, result| {
            match result {
                Ok(MetadataOutcome::Available(metadata)) => {
                    report.checked += 1;
                    let checked = check_package(&layout.root, *metadata, &mut report.summary);
                    append_jsonl(&mut journal.packages, &checked)?;
                }
                Ok(MetadataOutcome::NotFound { metadata_url }) => {
                    record_unavailable(&mut report, &mut journal, &name, metadata_url)?;
                }
                Err(error) => record_error(
                    &mut report,
                    &mut journal,
                    PackageError {
                        name: name.clone(),
                        error: format!("{error:#}"),
                    },
                )?,
            }
            completed += 1;
            if let Some(dashboard) = &dashboard {
                dashboard.checking(
                    completed,
                    total,
                    &name,
                    &report.summary,
                    report.unavailable_count,
                );
            }
            Ok(true)
        },
    )?;
    journal.packages.get_ref().sync_all()?;
    journal.errors.get_ref().sync_all()?;
    journal.unavailable.get_ref().sync_all()?;
    let report_path = layout.root.join("check-report.json");
    write_json(&report_path, &report)?;
    if let Some(dashboard) = &dashboard {
        dashboard.finish_check(
            &report.summary,
            report.error_count,
            report.unavailable_count,
            start.elapsed(),
        );
    }
    if !quiet {
        eprintln!("Report: {}", report_path.display());
    }
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    if args.json {
        serde_json::to_writer_pretty(&mut stdout, &report)?;
        writeln!(stdout)?;
    } else if !interactive {
        visit_packages(&layout.root.join(&report.packages_path), |package| {
            let missing = package
                .files
                .iter()
                .filter(|file| matches!(file.state, snatchrs::mirror::FileState::Missing))
                .count();
            let mismatch = package
                .files
                .iter()
                .filter(|file| {
                    matches!(file.state, snatchrs::mirror::FileState::SizeMismatch { .. })
                })
                .count();
            writeln!(
                stdout,
                "{}: {} releases, {} files · {} missing, {} size mismatches",
                package.metadata.package.normalized_name,
                package.metadata.releases.len(),
                package.files.len(),
                missing,
                mismatch
            )?;
            Ok(())
        })?;
        writeln!(
            stdout,
            "Checked {} packages, {} releases, {} files · {} missing, {} size mismatches, {} matching sizes, {} blocked · {} package errors · {} unavailable projects",
            report.summary.packages,
            report.summary.releases,
            report.summary.files,
            report.summary.missing,
            report.summary.size_mismatch,
            report.summary.matching_size,
            report.summary.blocked,
            report.error_count,
            report.unavailable_count
        )?;
        writeln!(
            stdout,
            "{} files need downloading; no downloads performed",
            report.summary.missing + report.summary.size_mismatch
        )?;
    }
    stdout.flush()?;
    ensure!(
        report.error_count == 0 && report.summary.blocked == 0,
        "Inspection had {} package errors and {} blocked files; see {}",
        report.error_count,
        report.summary.blocked,
        report_path.display()
    );
    Ok(())
}

#[derive(Serialize)]
struct MirrorReport<'a> {
    inspection: &'a CheckReport,
    downloads: MirrorDownloads<'a>,
    final_check: snatchrs::mirror::CheckSummary,
    simple_index: snatchrs::simple::SimpleSummary,
}

#[derive(Serialize)]
struct MirrorDownloads<'a> {
    concurrency: usize,
    min_free: u64,
    progress: &'a snatchrs::download::DownloadProgress,
    outcomes_path: PathBuf,
}

struct OutputOptions {
    quiet: bool,
    no_progress: bool,
    json: bool,
}

fn run_pipeline(
    layout: &MirrorLayout,
    client: &IndexClient,
    packages: Vec<Package>,
    inspection: (CheckReport, InspectionJournal, Option<HashSet<String>>),
    metadata_options: MetadataOptions,
    options: snatchrs::download::DownloadOptions,
    output: OutputOptions,
) -> Result<()> {
    let OutputOptions {
        quiet,
        no_progress,
        json,
    } = output;
    let (inspection, journal, authoritative_names) = inspection;
    let outcomes_path = inspection
        .packages_path
        .parent()
        .context("Missing run directory")?
        .join("download-outcomes.jsonl");
    let (mut outcomes_file, outcome_open_error) =
        match File::create(layout.root.join(&outcomes_path)) {
            Ok(file) => (Some(BufWriter::new(file)), None),
            Err(error) => (
                None,
                Some(format!("Cannot open download outcomes: {error}")),
            ),
        };
    let mut fallback_inspection = inspection.clone();
    fallback_inspection.not_inspected = packages.len();
    let mut finished_inspection = None;
    let mut latest_progress = snatchrs::download::DownloadProgress::default();
    let mut ui = crate::interactive(quiet, no_progress, json)
        .then(|| crate::download_ui::DownloadDashboard::new(options.concurrency));
    let mut checkpoint = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or_else(Instant::now);
    let mut log_time = Instant::now();
    let mut checkpoint_error = None;
    let mut stop_announced = false;
    let downloaded = snatchrs::download::download_stream_to(
        &layout.root,
        options,
        |queue| {
            let report = if let Some(reason) = &outcome_open_error {
                let mut report = inspection;
                let mut journal = journal;
                report.not_inspected = packages.len();
                stop_inspection(queue, &mut report, &mut journal, reason.clone());
                report
            } else {
                inspect_stream(
                    queue,
                    layout,
                    client,
                    metadata_options,
                    packages,
                    (inspection, journal),
                )?
            };
            finished_inspection = Some(report);
            Ok(())
        },
        |progress| {
            latest_progress = progress.clone();
            if let Some(ui) = ui.as_mut() {
                ui.update(progress);
            }
            if checkpoint.elapsed() >= Duration::from_secs(1)
                || (progress.discovery.finished
                    && progress.completed_files() == progress.total_files)
            {
                if let Err(error) = write_json(&layout.root.join("mirror-progress.json"), progress)
                {
                    checkpoint_error = Some(format!("{error:#}"));
                }
                checkpoint = Instant::now();
            }
            if let Some(reason) = &progress.stopped
                && ui.is_none()
                && !quiet
                && !stop_announced
            {
                eprintln!("Stopped: {reason}");
                stop_announced = true;
            }
            if progress.stopped.is_none()
                && ui.is_none()
                && !quiet
                && log_time.elapsed() >= Duration::from_secs(5)
            {
                eprintln!(
                    "Checked {}/{} packages · downloaded {}/{} discovered files · {} failed · {} unavailable projects · {:.1} MiB transferred",
                    progress.discovery.checked_packages,
                    progress.discovery.total_packages,
                    progress.downloaded,
                    progress.total_files,
                    progress.failed,
                    progress.discovery.unavailable,
                    progress.transferred_bytes as f64 / 1_048_576.0
                );
                log_time = Instant::now();
            }
        },
        |outcome| {
            append_jsonl(
                outcomes_file
                    .as_mut()
                    .context("Download outcomes unavailable")?,
                outcome,
            )
        },
    );
    let mut inspection = finished_inspection.unwrap_or(fallback_inspection);
    let mut downloads = match downloaded {
        Ok(((), downloads)) => downloads,
        Err(error) => {
            let reason = format!("{error:#}");
            inspection.queue_error.get_or_insert(reason.clone());
            latest_progress.stopped.get_or_insert(reason);
            latest_progress.not_attempted = latest_progress
                .total_files
                .saturating_sub(latest_progress.downloaded + latest_progress.failed);
            snatchrs::download::DownloadReport {
                concurrency: options.concurrency,
                min_free: options.min_free,
                progress: latest_progress,
                outcomes: Vec::new(),
            }
        }
    };
    if let Some(file) = &outcomes_file
        && let Err(error) = file.get_ref().sync_all()
    {
        let reason = format!("Cannot sync download outcomes: {error}");
        inspection.queue_error.get_or_insert(reason.clone());
        downloads.progress.stopped.get_or_insert(reason);
    }
    downloads.progress.discovery.total_packages = inspection.checked
        + inspection.error_count
        + inspection.unavailable_count
        + inspection.not_inspected;
    downloads.progress.discovery.checked_packages =
        inspection.checked + inspection.error_count + inspection.unavailable_count;
    downloads.progress.discovery.finished = true;
    if let Err(error) = write_json(
        &layout.root.join("mirror-progress.json"),
        &downloads.progress,
    ) {
        checkpoint_error = Some(format!("{error:#}"));
    }
    if let Some(ui) = &ui {
        ui.finish(&downloads.progress);
    }
    let mut final_check = CheckSummary::default();
    let mut simple_index = snatchrs::simple::SimpleSummary::default();
    if let Err(error) = visit_packages(&layout.root.join(&inspection.packages_path), |package| {
        match snatchrs::simple::write_project_page(&layout.root, &package.metadata) {
            Ok(page) => simple_index.add(page),
            Err(error) => {
                simple_index.record_error(package.metadata.package.normalized_name.clone(), error)
            }
        }
        check_package(&layout.root, package.metadata, &mut final_check);
        Ok(())
    }) {
        simple_index.record_error("<inspection>", error);
    }
    // Reaching the final pass means the root listing was fetched completely.
    // Per-project failures do not weaken its authority about deleted projects.
    if let Some(names) = authoritative_names {
        match snatchrs::simple::project_names(&layout.root) {
            Ok(projects) => {
                for name in projects {
                    if !names.contains(&name) {
                        let removed = layout.invalidate_metadata(&name).and_then(|()| {
                            snatchrs::simple::remove_project_page(&layout.root, &name)
                        });
                        match removed {
                            Ok(()) => simple_index.projects_removed += 1,
                            Err(error) => simple_index.record_error(name, error),
                        }
                    }
                }
            }
            Err(error) => simple_index.record_error("<root>", error),
        }
    }
    if let Err(error) = snatchrs::simple::write_root_index(&layout.root) {
        simple_index.record_error("<root>", error);
    }
    let report = MirrorReport {
        final_check,
        simple_index,
        inspection: &inspection,
        downloads: MirrorDownloads {
            concurrency: downloads.concurrency,
            min_free: downloads.min_free,
            progress: &downloads.progress,
            outcomes_path,
        },
    };
    let path = layout.root.join("mirror-report.json");
    write_json(&path, &report)?;
    if !quiet {
        eprintln!("Report: {}", path.display());
    }
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    if json {
        serde_json::to_writer_pretty(&mut stdout, &report)?;
        writeln!(stdout)?;
    } else if ui.is_none() {
        writeln!(
            stdout,
            "Downloaded {} files · {} failed · {} files with matching sizes · {} inspection errors · {} unavailable projects",
            downloads.progress.downloaded,
            downloads.progress.failed,
            report.final_check.matching_size,
            inspection.error_count,
            inspection.unavailable_count
        )?;
    }
    stdout.flush()?;
    if let Some(reason) = &downloads.progress.stopped {
        let stopped = if stop_announced {
            "Stopped".to_owned()
        } else {
            format!("Stopped: {reason}")
        };
        anyhow::bail!(
            "{stopped}. {} queued files were not attempted and {} projects were not inspected. Free space and rerun; completed files are kept.",
            downloads.progress.not_attempted,
            inspection.not_inspected
        );
    }
    ensure!(
        inspection.error_count == 0
            && inspection.queue_error.is_none()
            && downloads.progress.failed == 0
            && report.simple_index.page_errors == 0
            && report.final_check.blocked == 0
            && report.final_check.missing == 0
            && report.final_check.size_mismatch == 0,
        "Mirror incomplete: {} inspection errors, {} failed downloads, {} missing, {} wrong-sized, {} blocked, {} page errors; see {}",
        inspection.error_count,
        downloads.progress.failed,
        report.final_check.missing,
        report.final_check.size_mismatch,
        report.final_check.blocked,
        report.simple_index.page_errors,
        path.display()
    );
    ensure!(
        checkpoint_error.is_none(),
        "Progress checkpoint failed: {}",
        checkpoint_error.unwrap_or_default()
    );
    Ok(())
}

fn discovery(
    report: &CheckReport,
    total: usize,
    checked: usize,
    name: &str,
) -> snatchrs::download::DiscoveryProgress {
    snatchrs::download::DiscoveryProgress {
        total_packages: total,
        checked_packages: checked,
        current_package: name.to_owned(),
        releases: report.summary.releases,
        files: report.summary.files,
        missing: report.summary.missing,
        size_mismatch: report.summary.size_mismatch,
        errors: report.error_count,
        unavailable: report.unavailable_count,
        finished: false,
    }
}

fn stop_inspection(
    queue: &snatchrs::download::DownloadQueue,
    report: &mut CheckReport,
    journal: &mut InspectionJournal,
    reason: String,
) {
    if report.queue_error.is_none() {
        report.queue_error = Some(reason.clone());
        // An exhausted filesystem may also reject this journal append. Keep
        // the reason in the in-memory summary so saving the report is attempted.
        let _ = append_jsonl(
            &mut journal.errors,
            &PackageError {
                name: "<queue>".into(),
                error: reason.clone(),
            },
        );
    }
    queue.stop(reason);
}

fn inspect_stream(
    queue: &mut snatchrs::download::DownloadQueue,
    layout: &MirrorLayout,
    client: &IndexClient,
    metadata_options: MetadataOptions,
    packages: Vec<Package>,
    inspection: (CheckReport, InspectionJournal),
) -> Result<CheckReport> {
    let (mut report, mut journal) = inspection;
    let total = packages.len();
    let mut completed = 0;
    let inspected = (|| -> Result<()> {
        queue.discovery(discovery(&report, total, 0, ""))?;
        parallel_metadata(
            packages,
            client,
            layout,
            &metadata_options,
            |name, result| {
                if let Some(reason) = queue.stop_reason() {
                    stop_inspection(queue, &mut report, &mut journal, reason);
                    return Ok(false);
                }
                let classified = report.checked + report.error_count + report.unavailable_count;
                let handled = (|| -> Result<()> {
                    match result {
                        Ok(MetadataOutcome::Available(metadata)) => {
                            let checked =
                                check_package(&layout.root, *metadata, &mut report.summary);
                            let tasks =
                                snatchrs::download::build_tasks(std::slice::from_ref(&checked));
                            append_jsonl(&mut journal.packages, &checked)?;
                            let queued = tasks.and_then(|tasks| {
                                for task in tasks {
                                    queue.enqueue(task)?;
                                }
                                Ok(())
                            });
                            match queued {
                                Ok(()) => report.checked += 1,
                                Err(error)
                                    if error
                                        .downcast_ref::<snatchrs::download::QueueUnavailable>()
                                        .is_some() =>
                                {
                                    report.checked += 1;
                                    return Err(error);
                                }
                                Err(error) => record_error(
                                    &mut report,
                                    &mut journal,
                                    PackageError {
                                        name: name.clone(),
                                        error: format!("Failed to queue downloads: {error:#}"),
                                    },
                                )?,
                            }
                        }
                        Ok(MetadataOutcome::NotFound { metadata_url }) => {
                            snatchrs::simple::remove_project_page(&layout.root, &name)?;
                            record_unavailable(&mut report, &mut journal, &name, metadata_url)?;
                        }
                        Err(error) => record_error(
                            &mut report,
                            &mut journal,
                            PackageError {
                                name: name.clone(),
                                error: format!("{error:#}"),
                            },
                        )?,
                    }
                    Ok(())
                })();
                completed += 1;
                if let Err(error) = handled {
                    if report.checked + report.error_count + report.unavailable_count == classified
                    {
                        report.checked += 1;
                    }
                    stop_inspection(queue, &mut report, &mut journal, format!("{error:#}"));
                    let _ = queue.discovery(discovery(&report, total, completed, &name));
                    return Ok(false);
                }
                queue.discovery(discovery(&report, total, completed, &name))?;
                Ok(true)
            },
        )
    })();
    if let Err(error) = inspected {
        stop_inspection(queue, &mut report, &mut journal, format!("{error:#}"));
    }
    report.not_inspected = total.saturating_sub(completed);
    let saved = (|| -> Result<()> {
        journal.packages.get_ref().sync_all()?;
        journal.errors.get_ref().sync_all()?;
        journal.unavailable.get_ref().sync_all()?;
        write_json(&layout.root.join("check-report.json"), &report)
    })();
    if let Err(error) = saved {
        stop_inspection(
            queue,
            &mut report,
            &mut journal,
            format!("Cannot save inspection details: {error:#}"),
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_consumer_stop_or_error_releases_blocked_workers() {
        let root = tempfile::tempdir().unwrap();
        let layout = MirrorLayout::create(root.path()).unwrap();
        let source: Url = "http://127.0.0.1:1/simple/fixture/".parse().unwrap();
        let package = Package {
            name: "fixture".into(),
            normalized_name: "fixture".into(),
            index_url: source.clone(),
            last_serial: Some(1),
        };
        let mut cached = snatchrs::mirror::PackageMetadata::from_reader(
            br#"{"name":"fixture","meta":{"api-version":"1.4","_last-serial":1},"versions":[],"files":[]}"#.as_slice(),
            package.clone(), source.clone(), &layout,
        ).unwrap();
        cached.metadata_source_url = Some(source);
        layout.save_metadata(&cached).unwrap();
        let client = IndexClient::with_attempts(1).unwrap();
        let options = MetadataOptions {
            base: "http://127.0.0.1:1/simple/".parse().unwrap(),
            refresh: false,
            concurrency: 2,
        };
        for fail in [false, true] {
            let mut consumed = 0;
            let result = parallel_metadata(
                vec![package.clone(); 100],
                &client,
                &layout,
                &options,
                |_, metadata| {
                    assert!(matches!(metadata?, MetadataOutcome::Available(_)));
                    consumed += 1;
                    // Let both workers fill the bounded channel and block on send.
                    thread::sleep(Duration::from_millis(30));
                    if fail {
                        anyhow::bail!("simulated journal failure");
                    }
                    Ok(false)
                },
            );
            assert_eq!(consumed, 1);
            if fail {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("simulated journal failure")
                );
            } else {
                result.unwrap();
            }
        }
    }
}
