use std::{
    fs::File,
    io::{self, BufReader, IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
    time::Instant,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use snatchrs::{DEFAULT_INDEX_URL, IndexClient, PackageIndex};
use url::Url;

mod check;
mod download_ui;
mod progress;

#[derive(Parser)]
#[command(version, about = "Discover, inspect, and mirror PyPI packages")]
struct Cli {
    /// Suppress status messages (result output is still written).
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Disable interactive terminal progress.
    #[arg(long, global = true)]
    no_progress: bool,
    /// Maximum attempts per index/metadata request or file (including the first).
    #[arg(long, global = true, default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=10))]
    attempts: u32,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Download all releases for a random project sample (default: 100).
    Mirror(check::MirrorArgs),
    /// Inspect release metadata and local files for a small package sample.
    Check(check::CheckArgs),
    /// Load the Simple Index and list its packages.
    Index {
        #[arg(long, default_value = DEFAULT_INDEX_URL)]
        index_url: Url,
        /// Read a saved Simple Index JSON response without using the network.
        #[arg(long)]
        from_file: Option<PathBuf>,
        /// Print the complete structured index as JSON.
        #[arg(long)]
        json: bool,
    },
}

fn run(cli: Cli) -> Result<()> {
    let start = Instant::now();
    let (index_url, from_file, json) = match cli.command {
        Command::Mirror(args) => {
            return check::run_mirror(args, cli.quiet, cli.no_progress, cli.attempts);
        }
        Command::Check(args) => return check::run(args, cli.quiet, cli.no_progress, cli.attempts),
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
    match run(Cli::parse()) {
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
