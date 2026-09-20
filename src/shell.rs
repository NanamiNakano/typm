use git2::Progress;
use indicatif::{ProgressBar, ProgressStyle};
use std::io::{self, IsTerminal};

#[derive(Clone)]
pub struct Shell {
    quiet: bool,
    terminal: bool,
}

pub struct TransferProgress {
    bar: Option<ProgressBar>,
}

impl Shell {
    pub fn new(quiet: bool) -> Self {
        Self {
            quiet,
            terminal: io::stderr().is_terminal(),
        }
    }

    pub fn status(&self, action: &str, message: &str) {
        if !self.quiet {
            eprintln!("{action:>12} {message}");
        }
    }

    pub fn is_quiet(&self) -> bool {
        self.quiet
    }

    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    pub fn progress(&self, label: &str) -> TransferProgress {
        let bar = (!self.quiet && self.terminal).then(|| {
            let style = ProgressStyle::with_template(
                "{prefix:>12} [{bar:24.cyan/blue}] {percent:>3}% {msg}",
            )
            .expect("valid Git transfer progress template")
            .progress_chars("=> ");
            ProgressBar::new(0)
                .with_style(style)
                .with_prefix("Receiving")
                .with_message(label.to_owned())
        });
        TransferProgress { bar }
    }
}

impl TransferProgress {
    pub fn update(&self, progress: &Progress<'_>) {
        let Some(bar) = &self.bar else {
            return;
        };
        let (phase, completed, total) = if progress.received_objects() < progress.total_objects() {
            (
                "Receiving",
                progress.received_objects(),
                progress.total_objects(),
            )
        } else if progress.total_deltas() > 0 {
            (
                "Indexing",
                progress.indexed_deltas(),
                progress.total_deltas(),
            )
        } else {
            (
                "Indexing",
                progress.indexed_objects(),
                progress.total_objects(),
            )
        };
        if total == 0 {
            return;
        }
        bar.set_prefix(phase);
        bar.set_length(total as u64);
        bar.set_position(completed as u64);
        if completed == total {
            bar.force_draw();
        }
    }

    pub fn finish(&self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

impl Drop for TransferProgress {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_and_redirected_shells_do_not_create_progress_bars() {
        for shell in [
            Shell {
                quiet: true,
                terminal: true,
            },
            Shell {
                quiet: false,
                terminal: false,
            },
        ] {
            assert!(shell.progress("repository").bar.is_none());
        }
    }

    #[test]
    fn dropping_transfer_progress_finishes_its_bar() {
        let bar = ProgressBar::hidden();
        let progress = TransferProgress {
            bar: Some(bar.clone()),
        };
        assert!(!bar.is_finished());
        drop(progress);
        assert!(bar.is_finished());
    }

    #[test]
    fn finishing_progress_is_idempotent() {
        let bar = ProgressBar::hidden();
        let progress = TransferProgress {
            bar: Some(bar.clone()),
        };
        progress.finish();
        progress.finish();
        assert!(bar.is_finished());
    }
}
