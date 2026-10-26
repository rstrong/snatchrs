use std::{
    fs::File,
    io::{self, BufReader, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use snatchrs::{DEFAULT_INDEX_URL, IndexClient, PackageIndex};
use url::Url;

mod check;
mod download_ui;
mod progress;
mod report;
mod verify;

#[derive(Parser)]
#[command(version, about = "Discover, inspect, and mirror PyPI packages")]
struct Cli {
    /// Suppress status messages.
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Disable interactive progress.
    #[arg(long, global = true)]
    no_progress: bool,
    /// Maximum attempts per request or download.
    #[arg(long, value_name = "N", global = true, help_heading = "Advanced", default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=10))]
    attempts: u32,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download projects and publish a static Simple index
    Mirror(check::MirrorArgs),
    /// Report which files are missing, without downloading
    Check(check::CheckArgs),
    /// Rebuild the static Simple index from saved metadata
    Simple {
        /// Mirror directory.
        #[arg(value_name = "DIR")]
        mirror: PathBuf,
    },
    /// Check every mirrored file against its recorded SHA256
    Verify(verify::VerifyArgs),
    /// Load the Simple Index and list its packages.
    Index {
        /// Root Simple Index URL.
        #[arg(long, value_name = "URL", default_value = DEFAULT_INDEX_URL, help_heading = "Advanced")]
        index_url: Url,
        /// Read a saved index file.
        #[arg(long)]
        from_file: Option<PathBuf>,
        /// Print the index as JSON.
        #[arg(long, help_heading = "Advanced")]
        json: bool,
    },
}

fn run(cli: Cli, interrupt: &snatchrs::interrupt::StopToken) -> Result<()> {
    let start = Instant::now();
    let (index_url, from_file, json) = match cli.command {
        Command::Mirror(args) => {
            return check::run_mirror(args, cli.quiet, cli.no_progress, cli.attempts, interrupt);
        }
        Command::Check(args) => {
            return check::run(args, cli.quiet, cli.no_progress, cli.attempts, interrupt);
        }
        Command::Verify(args) => return verify::run(args, cli.quiet, cli.no_progress, interrupt),
        Command::Simple { mirror } => {
            let layout = snatchrs::mirror::MirrorLayout::create(&mirror)?;
            let _lock = layout.lock()?;
            let mut summary = snatchrs::simple::regenerate_interruptible(&layout.root, interrupt)?;
            summary.stale_temp_files_removed += _lock.stale_temp_files_removed;
            snatchrs::cleanup::announce(summary.stale_temp_files_removed, cli.quiet);
            let mut stdout = io::BufWriter::new(io::stdout().lock());
            writeln!(
                stdout,
                "Simple index: {} projects written · {} files linked · {} files skipped as not present · {} projects removed · {} stale temporary files removed",
                summary.projects,
                summary.files_linked,
                summary.files_skipped,
                summary.projects_removed,
                summary.stale_temp_files_removed
            )?;
            stdout.flush()?;
            ensure!(summary.page_errors == 0, "{}", summary.failure_message());
            return Ok(());
        }
        Command::Index {
            index_url,
            from_file,
            json,
        } => (index_url, from_file, json),
    };
    let interactive = interactive(cli.quiet, cli.no_progress, json);
    let mut dashboard = interactive.then(progress::Dashboard::new);
    let mut on_progress = |event: snatchrs::IndexProgress<'_>| {
        if let Some(dashboard) = dashboard.as_mut() {
            dashboard.update(event);
        }
    };
    let index = if let Some(path) = from_file {
        let file =
            File::open(&path).with_context(|| format!("Failed to open {}", path.display()))?;
        PackageIndex::from_reader_with_progress(BufReader::new(file), index_url, &mut on_progress)?
    } else {
        IndexClient::with_attempts(cli.attempts)?
            .fetch_with_progress(index_url, &mut on_progress)?
    };
    if let Some(dashboard) = &dashboard {
        dashboard.finish(index.packages.len(), start.elapsed());
        return Ok(());
    }
    if !cli.quiet {
        eprintln!(
            "Loaded {} packages in {:.2}s",
            index.packages.len(),
            start.elapsed().as_secs_f64()
        );
    }
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    if json {
        serde_json::to_writer_pretty(&mut stdout, &index)?;
        writeln!(stdout)?;
    } else {
        for package in &index {
            writeln!(stdout, "{}", package.normalized_name)?;
        }
    }
    stdout.flush()?;
    Ok(())
}

fn interactive(quiet: bool, no_progress: bool, json: bool) -> bool {
    !quiet
        && !no_progress
        && !json
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb")
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let interrupt = snatchrs::interrupt::StopToken::default();
    if !matches!(&cli.command, Command::Index { .. }) {
        let signal = interrupt.clone();
        if let Err(error) = ctrlc::set_handler(move || {
            if !signal.interrupt() {
                eprintln!("Interrupted again; exiting immediately");
                std::process::exit(130);
            }
        }) {
            eprintln!("error: cannot install signal handler: {error}");
            return ExitCode::FAILURE;
        }
    }
    let result = run(cli, &interrupt);
    if interrupt.is_interrupted() {
        if let Err(error) = result {
            eprintln!("{error:#}");
        } else {
            eprintln!("Interrupted. Completed files are kept; rerun to continue.");
        }
        return ExitCode::from(130);
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
            }) =>
        {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
