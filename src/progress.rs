//! Progress UI for long-running operations (uv-style).
//!
//! A [`SyncProgress`] owns an indicatif [`MultiProgress`]: one overall bar
//! plus one spinner per repository. When stderr is not a terminal the bars
//! are hidden and only the final summary lines are printed.

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::time::Duration;

pub struct SyncProgress {
    mp: MultiProgress,
    main: ProgressBar,
}

impl SyncProgress {
    pub fn new(total: u64, enabled: bool) -> Self {
        let mp = MultiProgress::new();
        if !enabled {
            mp.set_draw_target(ProgressDrawTarget::hidden());
        }
        let main = mp.add(ProgressBar::new(total));
        main.set_style(
            ProgressStyle::with_template("{bar:40.cyan/blue} {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("##-"),
        );
        main.set_message("syncing");
        Self { mp, main }
    }

    /// A spinner line for one repository; the caller finishes it with a
    /// message describing the outcome.
    pub fn repo_spinner(&self, label: &str) -> ProgressBar {
        let pb = self.mp.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg}")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠺", "⠦", "⠧", "⠇", "⠏"]),
        );
        pb.enable_steady_tick(Duration::from_millis(80));
        pb.set_message(label.to_string());
        pb
    }

    pub fn inc(&self) {
        self.main.inc(1);
    }

    pub fn finish(&self) {
        self.main.finish_with_message("done");
    }
}
