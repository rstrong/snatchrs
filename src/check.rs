use std::{
    io::{self, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use clap::Args;
use serde::Serialize;
use snatchrs::{
    DEFAULT_INDEX_URL, IndexClient, IndexProgress, Package,
    mirror::{
        CheckSummary, CheckedPackage, DownloadQueueStub, MirrorLayout, check_package,
        sample_packages, write_json,
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
    /// Print the complete inspection report as JSON.
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
    packages: Vec<CheckedPackage>,
    errors: Vec<PackageError>,
}

#[derive(Args)]
pub struct MirrorArgs {
    #[command(flatten)]
    selection: CheckArgs,
    /// Simultaneous download workers (1–64).
    #[arg(short = 'j', long, alias = "threads", default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    concurrency: u16,
    /// Maximum attempts per file, including the initial attempt.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=10))]
    attempts: u32,
}

pub fn run_mirror(args: MirrorArgs, quiet: bool, no_progress: bool) -> Result<()> {
    run_inner(
        args.selection,
        quiet,
        no_progress,
        Some(snatchrs::download::DownloadOptions {
            concurrency: usize::from(args.concurrency),
            attempts: args.attempts,
            ..Default::default()
        }),
    )
}

pub fn run(args: CheckArgs, quiet: bool, no_progress: bool) -> Result<()> {
    run_inner(args, quiet, no_progress, None)
}

fn run_inner(
    args: CheckArgs,
    quiet: bool,
    no_progress: bool,
    download: Option<snatchrs::download::DownloadOptions>,
) -> Result<()> {
    let start = Instant::now();
    let interactive = crate::interactive(quiet, no_progress, args.json);
    let mut dashboard = interactive.then(crate::progress::Dashboard::new);
    let layout = MirrorLayout::create(&args.mirror)?;
    let _lock = layout.lock()?;
    let client = IndexClient::new()?;
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
    let mut report = CheckReport {
        mirror_root: layout.root.clone(),
        seed,
        selected_packages: packages
            .iter()
            .map(|package| package.normalized_name.clone())
            .collect(),
        summary: CheckSummary::default(),
        queue_stub: DownloadQueueStub::default(),
        packages: Vec::new(),
        errors: Vec::new(),
    };
    if let Some(options) = download {
        if let Some(dashboard) = &dashboard {
            dashboard.clear();
        }
        return run_pipeline(
            &layout,
            &client,
            packages,
            report,
            metadata_url,
            options,
            OutputOptions {
                quiet,
                no_progress,
                json: args.json,
            },
        );
    }
    let total = packages.len();
    for (position, package) in packages.into_iter().enumerate() {
        let name = package.normalized_name.clone();
        if let Some(dashboard) = &dashboard {
            dashboard.checking(position, total, &name, &report.summary);
        }
        let result = client
            .fetch_metadata(package, metadata_url.clone(), &layout)
            .and_then(|metadata| {
                layout.save_metadata(&metadata)?;
                Ok(metadata)
            });
        match result {
            Ok(metadata) => report.packages.push(check_package(
                metadata,
                &mut report.summary,
                &mut report.queue_stub,
            )),
            Err(error) => report.errors.push(PackageError {
                name: name.clone(),
                error: format!("{error:#}"),
            }),
        }
        if let Some(dashboard) = &dashboard {
            dashboard.checking(position + 1, total, &name, &report.summary);
        }
    }
    let report_path = layout.root.join("check-report.json");
    write_json(&report_path, &report)?;
    if let Some(dashboard) = &dashboard {
        dashboard.finish_check(&report.summary, report.errors.len(), start.elapsed());
    }
    if !quiet {
        eprintln!("Report: {}", report_path.display());
    }
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    if args.json {
        serde_json::to_writer_pretty(&mut stdout, &report)?;
        writeln!(stdout)?;
    } else if !interactive {
        for package in &report.packages {
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
        }
        writeln!(
            stdout,
            "Checked {} packages, {} releases, {} files · {} missing, {} size mismatches, {} matching sizes, {} blocked · {} package errors",
            report.summary.packages,
            report.summary.releases,
            report.summary.files,
            report.summary.missing,
            report.summary.size_mismatch,
            report.summary.matching_size,
            report.summary.blocked,
            report.errors.len()
        )?;
        writeln!(
            stdout,
            "Download queue stub: {} candidates; no downloads performed",
            report.queue_stub.candidates_seen
        )?;
    }
    stdout.flush()?;
    ensure!(
        report.errors.is_empty() && report.summary.blocked == 0,
        "Inspection had {} package errors and {} blocked files; see {}",
        report.errors.len(),
        report.summary.blocked,
        report_path.display()
    );
    Ok(())
}

#[derive(Serialize)]
struct MirrorReport<'a> {
    inspection: &'a CheckReport,
    downloads: &'a snatchrs::download::DownloadReport,
    final_check: snatchrs::mirror::CheckSummary,
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
    inspection: CheckReport,
    metadata_url: Url,
    options: snatchrs::download::DownloadOptions,
    output: OutputOptions,
) -> Result<()> {
    let OutputOptions {
        quiet,
        no_progress,
        json,
    } = output;
    let mut ui = crate::interactive(quiet, no_progress, json)
        .then(|| crate::download_ui::DownloadDashboard::new(options.concurrency));
    let mut checkpoint = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or_else(Instant::now);
    let mut log_time = Instant::now();
    let mut checkpoint_error = None;
    let (inspection, downloads) = snatchrs::download::download_stream(
        &layout.root,
        options,
        |queue| inspect_stream(queue, layout, client, metadata_url, packages, inspection),
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
                    "Checked {}/{} packages · downloaded {}/{} discovered files · {} failed · {:.1} MiB transferred",
                    progress.discovery.checked_packages,
                    progress.discovery.total_packages,
                    progress.downloaded,
                    progress.total_files,
                    progress.failed,
                    progress.transferred_bytes as f64 / 1_048_576.0
                );
                log_time = Instant::now();
            }
        },
    )?;
    if let Some(ui) = &ui {
        ui.finish(&downloads.progress);
    }
    let report = MirrorReport {
        final_check: snatchrs::download::final_summary(&inspection.packages),
        inspection: &inspection,
        downloads: &downloads,
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
            "Downloaded {} files · {} failed · {} files with matching sizes · {} inspection errors",
            downloads.progress.downloaded,
            downloads.progress.failed,
            report.final_check.matching_size,
            inspection.errors.len()
        )?;
    }
    stdout.flush()?;
    ensure!(
        inspection.errors.is_empty()
            && downloads.progress.failed == 0
            && report.final_check.blocked == 0
            && report.final_check.missing == 0
            && report.final_check.size_mismatch == 0,
        "Mirror incomplete: {} inspection errors, {} failed downloads, {} missing, {} wrong-sized, {} blocked; see {}",
        inspection.errors.len(),
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
        errors: report.errors.len(),
        finished: false,
    }
}

fn inspect_stream(
    queue: &mut snatchrs::download::DownloadQueue,
    layout: &MirrorLayout,
    client: &IndexClient,
    metadata_url: Url,
    packages: Vec<Package>,
    mut report: CheckReport,
) -> Result<CheckReport> {
    let total = packages.len();
    queue.discovery(discovery(&report, total, 0, ""))?;
    for (position, package) in packages.into_iter().enumerate() {
        let name = package.normalized_name.clone();
        queue.discovery(discovery(&report, total, position, &name))?;
        let result = client
            .fetch_metadata(package, metadata_url.clone(), layout)
            .and_then(|metadata| {
                layout.save_metadata(&metadata)?;
                Ok(metadata)
            });
        let mut stop = false;
        match result {
            Ok(metadata) => {
                let checked = check_package(metadata, &mut report.summary, &mut report.queue_stub);
                let tasks = snatchrs::download::build_tasks(std::slice::from_ref(&checked));
                report.packages.push(checked);
                let queued = tasks.and_then(|tasks| {
                    for task in tasks {
                        queue.enqueue(task)?;
                    }
                    Ok(())
                });
                if let Err(error) = queued {
                    report.errors.push(PackageError {
                        name: name.clone(),
                        error: format!("Failed to queue downloads: {error:#}"),
                    });
                    stop = true;
                }
            }
            Err(error) => report.errors.push(PackageError {
                name: name.clone(),
                error: format!("{error:#}"),
            }),
        }
        queue.discovery(discovery(&report, total, position + 1, &name))?;
        if stop {
            break;
        }
    }
    write_json(&layout.root.join("check-report.json"), &report)?;
    Ok(report)
}
