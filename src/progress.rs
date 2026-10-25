use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};
use snatchrs::IndexProgress;

/// A compact dashboard that leaves the final result in terminal history.
pub struct Dashboard {
    bar: ProgressBar,
    last_draw: Instant,
    packages: bool,
}

impl Dashboard {
    pub fn clear(&self) {
        self.bar.finish_and_clear();
    }
    pub fn checking(
        &self,
        processed: usize,
        total: usize,
        name: &str,
        summary: &snatchrs::mirror::CheckSummary,
    ) {
        self.bar.set_style(style(
            "{spinner:.cyan} {prefix:.bold}  {elapsed_precise}\n  {wide_bar:.cyan/blue} {percent:>3}%\n  {human_pos} / {human_len} packages\n  {msg}",
        ));
        self.bar.set_prefix("snatchrs · Checking mirror");
        self.bar.set_length(total as u64);
        self.bar.set_position(processed as u64);
        self.bar.set_message(format!(
            "Current: {name}\n  {} releases · {} files · {} missing · {} size mismatches",
            summary.releases, summary.files, summary.missing, summary.size_mismatch
        ));
    }

    pub fn finish_check(
        &self,
        summary: &snatchrs::mirror::CheckSummary,
        errors: usize,
        elapsed: Duration,
    ) {
        self.bar.set_style(style("{msg}"));
        self.bar.finish_with_message(format!(
            "Checked {} packages · {} releases · {} files in {:.2}s\n  {} missing · {} size mismatches · {} matching sizes\n  {} blocked files · {errors} package errors\n  Download queue stub: {} candidates; no downloads performed",
            summary.packages, summary.releases, summary.files, elapsed.as_secs_f64(), summary.missing, summary.size_mismatch, summary.matching_size, summary.blocked, summary.missing + summary.size_mismatch,
        ));
    }

    pub fn new() -> Self {
        let bar = ProgressBar::new_spinner();
        bar.set_style(style(
            "{spinner:.cyan} {prefix:.bold}  {elapsed_precise}\n  {msg}",
        ));
        bar.set_prefix("snatchrs · Reading index");
        bar.set_message("Waiting for response…");
        bar.enable_steady_tick(Duration::from_millis(80));
        Self {
            bar,
            last_draw: Instant::now(),
            packages: false,
        }
    }

    pub fn update(&mut self, event: IndexProgress<'_>) {
        match event {
            IndexProgress::Reading { bytes } => {
                if self.last_draw.elapsed() >= Duration::from_millis(80) {
                    self.bar.set_message(format!(
                        "{:.2} MiB read · decoding JSON",
                        bytes as f64 / 1_048_576.0
                    ));
                    self.last_draw = Instant::now();
                }
            }
            IndexProgress::Packages {
                processed,
                total,
                name,
            } => {
                if !self.packages {
                    self.packages = true;
                    self.bar.set_style(style(
                        "{spinner:.cyan} {prefix:.bold}  {elapsed_precise}\n  {wide_bar:.cyan/blue} {percent:>3}%\n  {human_pos} / {human_len} packages\n  {msg}",
                    ));
                    self.bar.set_prefix("snatchrs · Building packages");
                    self.bar.set_length(total as u64);
                }
                if processed == 0
                    || processed == total
                    || self.last_draw.elapsed() >= Duration::from_millis(80)
                {
                    self.bar.set_position(processed as u64);
                    // Names have already been validated as ASCII package names.
                    self.bar.set_message(format!(
                        "Current: {}",
                        if name.is_empty() { "—" } else { name }
                    ));
                    self.last_draw = Instant::now();
                }
            }
        }
    }

    pub fn finish(&self, count: usize, elapsed: Duration) {
        self.bar.set_style(style("{msg:.green}"));
        self.bar.finish_with_message(format!(
            "✓ Loaded {count} packages in {:.2}s",
            elapsed.as_secs_f64()
        ));
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        if !self.bar.is_finished() {
            self.bar.finish_and_clear();
        }
    }
}

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .expect("static progress template is valid")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
        .progress_chars("━╸─")
}
