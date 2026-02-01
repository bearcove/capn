use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
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

/// A task being tracked with a spinner
pub struct TaskSpinner {
    bar: ProgressBar,
    name: String,
}

impl TaskSpinner {
    /// Mark task as successful with elapsed time
    pub fn succeed(&self, elapsed_secs: f32) {
        self.bar.set_style(finished_style());
        self.bar.finish_with_message(format!(
            "  {} {:<14} {}",
            "✓".green(),
            self.name,
            format!("{:.1}s", elapsed_secs).dimmed()
        ));
    }

    /// Mark task as failed with elapsed time
    pub fn fail(&self, elapsed_secs: f32) {
        self.bar.set_style(finished_style());
        self.bar.finish_with_message(format!(
            "  {} {:<14} {}",
            "✗".red(),
            self.name,
            format!("{:.1}s", elapsed_secs).dimmed()
        ));
    }

    /// Mark task as skipped with reason
    pub fn skip(&self, reason: &str) {
        self.bar.set_style(finished_style());
        self.bar.finish_with_message(format!(
            "  {} {:<14} {}",
            "⊘".yellow(),
            self.name,
            reason.dimmed()
        ));
    }

    /// Update the spinner message while running (e.g., show current status)
    pub fn set_message(&self, msg: &str) {
        // Truncate message if too long, show last part
        let max_len = 50;
        let display_msg = if msg.len() > max_len {
            format!("…{}", &msg[msg.len() - max_len + 1..])
        } else {
            msg.to_string()
        };
        self.bar
            .set_message(format!("{:<14} {}", self.name, display_msg.dimmed()));
    }
}

/// Manages multiple concurrent task spinners
pub struct TaskProgress {
    mp: MultiProgress,
}

impl TaskProgress {
    pub fn new() -> Self {
        Self {
            mp: MultiProgress::new(),
        }
    }

    /// Add a new task spinner (starts spinning immediately)
    pub fn add_task(&self, name: &str) -> TaskSpinner {
        let bar = self.mp.add(ProgressBar::new_spinner());
        bar.set_style(spinner_style());
        bar.set_message(format!("{:<14}", name));
        bar.enable_steady_tick(Duration::from_millis(80));

        TaskSpinner {
            bar,
            name: name.to_string(),
        }
    }
}

impl Default for TaskProgress {
    fn default() -> Self {
        Self::new()
    }
}
