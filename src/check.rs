use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::Serialize;
use snatchrs::{
    DEFAULT_INDEX_URL, IndexClient, IndexProgress, Package,
    mirror::{
        CheckSummary, CheckedPackage, DownloadQueueStub, MetadataOutcome, MirrorLayout,
        check_package, sample_packages, write_json,
    },
};
use url::Url;

#[derive(Args)]
pub struct CheckArgs {
    /// Mirror root; creates metadata and empty package directories as needed.
    #[arg(long, default_value = "mirror-shell")]
    mirror: PathBuf,
    /// Random project count (check defaults to 10, mirror defaults to 100).
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
    sample: Option<u32>,
    /// Repeatable random selection for an unchanged root index.
    #[arg(long)]
    seed: Option<u64>,
    /// Inspect a specific package instead of sampling; repeat to select several.
    #[arg(long = "package", conflicts_with_all = ["sample", "seed"])]
    packages: Vec<String>,
    #[arg(long, default_value = DEFAULT_INDEX_URL)]
    index_url: Url,
    /// Override the per-project Simple Index base (defaults to --index-url).
    #[arg(long)]
    metadata_url: Option<Url>,
    /// Fetch fresh project metadata even when its cached serial is unchanged.
    #[arg(long)]
    refresh: bool,
    /// Print the summary report as JSON (details are stored in JSONL files).
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct PackageError {
    name: String,
    error: String,
}

#[derive(Serialize)]
struct CheckReport {
    mirror_root: PathBuf,
    seed: Option<u64>,
    selected_packages: Vec<String>,
    summary: CheckSummary,
    queue_stub: DownloadQueueStub,
    packages_path: PathBuf,
    errors_path: PathBuf,
    error_count: usize,
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
    append_jsonl(&mut journal.errors, &error)?;
    report.error_count += 1;
    Ok(())
}

fn record_unavailable(
    report: &mut CheckReport,
    journal: &mut InspectionJournal,
    name: &str,
    metadata_url: Url,
) -> Result<()> {
    append_jsonl(
        &mut journal.unavailable,
        &serde_json::json!({
            "name": name, "status": "not_found", "http_status": 404,
            "metadata_url": metadata_url,
        }),
    )?;
    report.unavailable_count += 1;
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
    /// Simultaneous download workers (1–64).
    #[arg(short = 'j', long, alias = "threads", default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    concurrency: u16,
}

pub fn run_mirror(args: MirrorArgs, quiet: bool, no_progress: bool, attempts: u32) -> Result<()> {
    run_inner(
        args.selection,
        quiet,
        no_progress,
        Some(snatchrs::download::DownloadOptions {
            concurrency: usize::from(args.concurrency),
            attempts,
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
    let seed = args
        .packages
        .is_empty()
        .then(|| args.seed.unwrap_or_else(rand::random));
    let packages = if let Some(seed) = seed {
        let index =
            client.fetch_with_progress(args.index_url.clone(), |event: IndexProgress<'_>| {
                if let Some(dashboard) = dashboard.as_mut() {
                    dashboard.update(event);
                }
            })?;
        sample_packages(
            &index,
            args.sample
                .unwrap_or(if download.is_some() { 100 } else { 10 }) as usize,
            seed,
        )
    } else {
        let base = snatchrs::normalize_index_url(args.index_url)?;
        args.packages
            .into_iter()
            .map(|name| {
                let normalized_name = snatchrs::normalize_name(&name)?;
                Ok(Package {
                    index_url: base.join(&format!("{normalized_name}/"))?,
                    name,
                    normalized_name,
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
    let packages_path = run_dir.join("checked-packages.jsonl");
    let errors_path = run_dir.join("package-errors.jsonl");
    let unavailable_path = run_dir.join("unavailable-projects.jsonl");
    let mut journal = InspectionJournal {
        packages: BufWriter::new(File::create(&packages_path)?),
        errors: BufWriter::new(File::create(&errors_path)?),
        unavailable: BufWriter::new(File::create(&unavailable_path)?),
    };
    let mut report = CheckReport {
        mirror_root: layout.root.clone(),
        seed,
        selected_packages: packages
            .iter()
            .map(|package| package.normalized_name.clone())
            .collect(),
        summary: CheckSummary::default(),
        queue_stub: DownloadQueueStub::default(),
        packages_path,
        errors_path,
        error_count: 0,
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
            (report, journal),
            metadata_url,
            options,
            OutputOptions {
                quiet,
                no_progress,
                json: args.json,
                refresh: args.refresh,
            },
        );
    }
    let total = packages.len();
    for (position, package) in packages.into_iter().enumerate() {
        let name = package.normalized_name.clone();
        if let Some(dashboard) = &dashboard {
            dashboard.checking(
                position,
                total,
                &name,
                &report.summary,
                report.unavailable_count,
            );
        }
        let result = client.load_metadata(package, metadata_url.clone(), &layout, args.refresh);
        match result {
            Ok(MetadataOutcome::Available(metadata)) => {
                let checked = check_package(*metadata, &mut report.summary, &mut report.queue_stub);
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
        if let Some(dashboard) = &dashboard {
            dashboard.checking(
                position + 1,
                total,
                &name,
                &report.summary,
                report.unavailable_count,
            );
        }
    }
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
        visit_packages(&report.packages_path, |package| {
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
            "Download queue stub: {} candidates; no downloads performed",
            report.queue_stub.candidates_seen
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
}

#[derive(Serialize)]
struct MirrorDownloads<'a> {
    concurrency: usize,
    progress: &'a snatchrs::download::DownloadProgress,
    outcomes_path: PathBuf,
}

struct OutputOptions {
    quiet: bool,
    no_progress: bool,
    json: bool,
    refresh: bool,
}

fn run_pipeline(
    layout: &MirrorLayout,
    client: &IndexClient,
    packages: Vec<Package>,
    inspection: (CheckReport, InspectionJournal),
    metadata_url: Url,
    options: snatchrs::download::DownloadOptions,
    output: OutputOptions,
) -> Result<()> {
    let OutputOptions {
        quiet,
        no_progress,
        json,
        refresh,
    } = output;
    let outcomes_path = inspection
        .0
        .packages_path
        .parent()
        .context("Missing run directory")?
        .join("download-outcomes.jsonl");
    let mut outcomes_file = BufWriter::new(File::create(&outcomes_path)?);
    let mut ui = crate::interactive(quiet, no_progress, json)
        .then(|| crate::download_ui::DownloadDashboard::new(options.concurrency));
    let mut checkpoint = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or_else(Instant::now);
    let mut log_time = Instant::now();
    let mut checkpoint_error = None;
    let (inspection, downloads) = snatchrs::download::download_stream_to(
        &layout.root,
        options,
        |queue| {
            inspect_stream(
                queue,
                layout,
                client,
                metadata_url,
                packages,
                inspection,
                refresh,
            )
        },
        |progress| {
            if let Some(ui) = ui.as_mut() {
                ui.update(progress);
            }
            if checkpoint.elapsed() >= Duration::from_secs(1)
                || (progress.discovery.finished
                    && progress.downloaded + progress.failed == progress.total_files)
            {
                if let Err(error) = write_json(&layout.root.join("mirror-progress.json"), progress)
                {
                    checkpoint_error = Some(format!("{error:#}"));
                }
                checkpoint = Instant::now();
            }
            if ui.is_none() && !quiet && log_time.elapsed() >= Duration::from_secs(5) {
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
        |outcome| append_jsonl(&mut outcomes_file, outcome),
    )?;
    outcomes_file.get_ref().sync_all()?;
    if let Some(ui) = &ui {
        ui.finish(&downloads.progress);
    }
    let mut final_check = CheckSummary::default();
    visit_packages(&inspection.packages_path, |package| {
        check_package(
            package.metadata,
            &mut final_check,
            &mut DownloadQueueStub::default(),
        );
        Ok(())
    })?;
    let report = MirrorReport {
        final_check,
        inspection: &inspection,
        downloads: MirrorDownloads {
            concurrency: downloads.concurrency,
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
    ensure!(
        inspection.error_count == 0
            && downloads.progress.failed == 0
            && report.final_check.blocked == 0
            && report.final_check.missing == 0
            && report.final_check.size_mismatch == 0,
        "Mirror incomplete: {} inspection errors, {} failed downloads, {} missing, {} wrong-sized, {} blocked; see {}",
        inspection.error_count,
        downloads.progress.failed,
        report.final_check.missing,
        report.final_check.size_mismatch,
        report.final_check.blocked,
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

fn inspect_stream(
    queue: &mut snatchrs::download::DownloadQueue,
    layout: &MirrorLayout,
    client: &IndexClient,
    metadata_url: Url,
    packages: Vec<Package>,
    inspection: (CheckReport, InspectionJournal),
    refresh: bool,
) -> Result<CheckReport> {
    let (mut report, mut journal) = inspection;
    let total = packages.len();
    queue.discovery(discovery(&report, total, 0, ""))?;
    for (position, package) in packages.into_iter().enumerate() {
        let name = package.normalized_name.clone();
        queue.discovery(discovery(&report, total, position, &name))?;
        let result = client.load_metadata(package, metadata_url.clone(), layout, refresh);
        let mut stop = false;
        match result {
            Ok(MetadataOutcome::Available(metadata)) => {
                let checked = check_package(*metadata, &mut report.summary, &mut report.queue_stub);
                let tasks = snatchrs::download::build_tasks(std::slice::from_ref(&checked));
                append_jsonl(&mut journal.packages, &checked)?;
                let queued = tasks.and_then(|tasks| {
                    for task in tasks {
                        queue.enqueue(task)?;
                    }
                    Ok(())
                });
                if let Err(error) = queued {
                    record_error(
                        &mut report,
                        &mut journal,
                        PackageError {
                            name: name.clone(),
                            error: format!("Failed to queue downloads: {error:#}"),
                        },
                    )?;
                    stop = true;
                }
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
        queue.discovery(discovery(&report, total, position + 1, &name))?;
        if stop {
            break;
        }
    }
    journal.packages.get_ref().sync_all()?;
    journal.errors.get_ref().sync_all()?;
    journal.unavailable.get_ref().sync_all()?;
    write_json(&layout.root.join("check-report.json"), &report)?;
    Ok(report)
}
