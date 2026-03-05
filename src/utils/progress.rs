use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use std::io::IsTerminal;
use std::time::Duration;

/// Style for spinning tasks (while running)
fn spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("  {spinner:.cyan} {wide_msg}")
        .expect("valid template")
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
}

/// Style for finished tasks (no spinner)
fn finished_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("{msg}")
        .expect("valid template")
}

enum SpinnerInner {
    Tty(ProgressBar),
    Plain,
}

/// A task being tracked with a spinner
pub struct TaskSpinner {
    inner: SpinnerInner,
    name: String,
}

impl TaskSpinner {
    /// Mark task as successful with elapsed time
    pub fn succeed(&self, elapsed_secs: f32) {
        match &self.inner {
            SpinnerInner::Tty(bar) => {
                bar.set_style(finished_style());
                bar.finish_with_message(format!(
                    "  {} {:<14} {}",
                    "✓".green(),
                    self.name,
                    format!("{:.1}s", elapsed_secs).dimmed()
                ));
            }
            SpinnerInner::Plain => {
                eprintln!("  {} {:<14} {:.1}s", "✓".green(), self.name, elapsed_secs);
            }
        }
    }

    /// Mark task as failed with elapsed time
    pub fn fail(&self, elapsed_secs: f32) {
        match &self.inner {
            SpinnerInner::Tty(bar) => {
                bar.set_style(finished_style());
                bar.finish_with_message(format!(
                    "  {} {:<14} {}",
                    "✗".red(),
                    self.name,
                    format!("{:.1}s", elapsed_secs).dimmed()
                ));
            }
            SpinnerInner::Plain => {
                eprintln!("  {} {:<14} {:.1}s", "✗".red(), self.name, elapsed_secs);
            }
        }
    }

    /// Mark task as skipped with reason
    pub fn skip(&self, reason: &str) {
        match &self.inner {
            SpinnerInner::Tty(bar) => {
                bar.set_style(finished_style());
                bar.finish_with_message(format!(
                    "  {} {:<14} {}",
                    "⊘".yellow(),
                    self.name,
                    reason.dimmed()
                ));
            }
            SpinnerInner::Plain => {
                eprintln!("  {} {:<14} {}", "⊘".yellow(), self.name, reason);
            }
        }
    }

    /// Update the spinner message while running (e.g., show current status)
    pub fn set_message(&self, msg: &str) {
        match &self.inner {
            SpinnerInner::Tty(bar) => {
                let max_len = 50;
                let display_msg = if msg.len() > max_len {
                    format!("…{}", &msg[msg.len() - max_len + 1..])
                } else {
                    msg.to_string()
                };
                bar.set_message(format!("{:<14} {}", self.name, display_msg.dimmed()));
            }
            SpinnerInner::Plain => {}
        }
    }

    /// Clear the spinner without showing any result
    pub fn clear(&self) {
        match &self.inner {
            SpinnerInner::Tty(bar) => bar.finish_and_clear(),
            SpinnerInner::Plain => {}
        }
    }
}

/// Manages multiple concurrent task spinners
pub struct TaskProgress {
    mp: Option<MultiProgress>,
}

impl TaskProgress {
    pub fn new() -> Self {
        let is_tty = std::io::stderr().is_terminal();
        Self {
            mp: if is_tty {
                Some(MultiProgress::new())
            } else {
                None
            },
        }
    }

    /// Add a new task spinner (starts spinning immediately)
    pub fn add_task(&self, name: &str) -> TaskSpinner {
        match &self.mp {
            Some(mp) => {
                let bar = mp.add(ProgressBar::new_spinner());
                bar.set_style(spinner_style());
                bar.set_message(format!("{:<14}", name));
                bar.enable_steady_tick(Duration::from_millis(80));
                TaskSpinner {
                    inner: SpinnerInner::Tty(bar),
                    name: name.to_string(),
                }
            }
            None => TaskSpinner {
                inner: SpinnerInner::Plain,
                name: name.to_string(),
            },
        }
    }
}

impl Default for TaskProgress {
    fn default() -> Self {
        Self::new()
    }
}
