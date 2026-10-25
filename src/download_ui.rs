use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use snatchrs::download::{DownloadProgress, WorkerState};

pub struct DownloadDashboard {
    _multi: MultiProgress,
    checking: ProgressBar,
    overall: ProgressBar,
    bytes: ProgressBar,
    workers: Vec<ProgressBar>,
}

impl DownloadDashboard {
    pub fn new(concurrency: usize) -> Self {
        let multi = MultiProgress::new();
        let checking = multi.add(ProgressBar::new(0));
        checking.set_style(style("{spinner:.cyan} snatchrs · Checking and downloading  {elapsed_precise}\n  Checking {human_pos}/{human_len} packages · {wide_msg}"));
        checking.enable_steady_tick(Duration::from_millis(100));
        let overall = multi.add(ProgressBar::new(0));
        overall.set_style(style("  {wide_bar:.cyan/blue} {percent:>3}%\n  {human_pos}/{human_len} discovered files · {msg}"));
        let bytes = multi.add(ProgressBar::new(0));
        bytes.set_style(style(
            "  {bytes}/{total_bytes} verified or in flight · {msg}",
        ));
        let workers = (0..concurrency)
            .map(|index| {
                let bar = multi.add(ProgressBar::new(0));
                bar.set_prefix(format!("{:02}", index + 1));
                bar.set_style(style(
                    "  {prefix:.dim} {bar:14.green/blue} {bytes:>10}/{total_bytes:<10} {wide_msg}",
                ));
                bar
            })
            .collect();
        Self {
            _multi: multi,
            checking,
            overall,
            bytes,
            workers,
        }
    }

    pub fn update(&mut self, progress: &DownloadProgress) {
        self.checking
            .set_length(progress.discovery.total_packages as u64);
        self.checking
            .set_position(progress.discovery.checked_packages as u64);
        let phase = if progress.discovery.finished {
            if progress.discovery.checked_packages < progress.discovery.total_packages {
                "stopped early"
            } else {
                "finished"
            }
        } else {
            "checking"
        };
        self.checking.set_message(format!(
            "{phase} · {} · {} metadata/queue errors · {} unavailable projects",
            progress.discovery.current_package,
            progress.discovery.errors,
            progress.discovery.unavailable
        ));
        self.overall.set_length(progress.total_files as u64);
        self.overall
            .set_position((progress.downloaded + progress.failed) as u64);
        let active = progress
            .workers
            .iter()
            .filter(|worker| worker.state == WorkerState::Downloading)
            .count();
        let retrying = progress
            .workers
            .iter()
            .filter(|worker| worker.state == WorkerState::Retrying)
            .count();
        let queued = progress
            .total_files
            .saturating_sub(progress.downloaded + progress.failed + active + retrying);
        let total = if progress.discovery.finished {
            "final total"
        } else {
            "total still growing"
        };
        self.overall.set_message(format!(
            "{queued} queued · {active} active · {retrying} retrying · {} failed · {total}",
            progress.failed
        ));
        self.bytes.set_length(progress.total_bytes);
        let inflight: u64 = progress
            .workers
            .iter()
            .filter(|worker| worker.state == WorkerState::Downloading)
            .map(|worker| worker.bytes.min(worker.size))
            .sum();
        self.bytes.set_position(progress.verified_bytes + inflight);
        self.bytes.set_message(format!(
            "{:.2} MiB/s",
            progress.transferred_bytes as f64 / 1_048_576.0 / progress.elapsed_seconds.max(0.001)
        ));
        for (bar, worker) in self.workers.iter().zip(&progress.workers) {
            bar.set_length(worker.size);
            bar.set_position(worker.bytes.min(worker.size));
            if worker.filename.is_empty() {
                bar.set_message("Waiting for queued files");
            } else if let Some(delay) = worker.retry_delay_seconds {
                bar.set_message(format!(
                    "retry wait ({delay:.1}s) · {} · {} (try {})",
                    worker.package, worker.filename, worker.attempt
                ));
            } else {
                bar.set_message(format!(
                    "{} · {} · {} (try {})",
                    worker.state, worker.package, worker.filename, worker.attempt
                ));
            }
        }
    }

    pub fn finish(&self, progress: &DownloadProgress) {
        for bar in &self.workers {
            bar.finish_and_clear();
        }
        self.bytes.finish_and_clear();
        self.checking.finish_and_clear();
        self.overall.set_style(style("{msg}"));
        self.overall.finish_with_message(format!(
            "Checked {}/{} packages · {} metadata/queue errors · {} unavailable projects\nDownloaded {} files · {} failed · {:.2} MiB verified in {:.1}s",
            progress.discovery.checked_packages,
            progress.discovery.total_packages,
            progress.discovery.errors,
            progress.discovery.unavailable,
            progress.downloaded,
            progress.failed,
            progress.verified_bytes as f64 / 1_048_576.0,
            progress.elapsed_seconds
        ));
    }
}

impl Drop for DownloadDashboard {
    fn drop(&mut self) {
        for bar in &self.workers {
            if !bar.is_finished() {
                bar.finish_and_clear();
            }
        }
        if !self.bytes.is_finished() {
            self.bytes.finish_and_clear();
        }
        if !self.overall.is_finished() {
            self.overall.finish_and_clear();
        }
        if !self.checking.is_finished() {
            self.checking.finish_and_clear();
        }
    }
}

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .expect("static template is valid")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
        .progress_chars("━╸─")
}
