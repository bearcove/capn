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

/// Walk ancestor process names from immediate parent upward.
fn ancestor_names() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        linux_ancestor_names()
    }
    #[cfg(target_os = "macos")]
    {
        macos_ancestor_names()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        vec![]
    }
}

#[cfg(target_os = "linux")]
fn linux_ancestor_names() -> Vec<String> {
    let mut names = Vec::new();
    let mut pid = std::process::id();
    for _ in 0..16 {
        let status = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Ok(s) => s,
            Err(_) => break,
        };
        let ppid: u32 = match status
            .lines()
            .find(|l| l.starts_with("PPid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
        {
            Some(p) => p,
            None => break,
        };
        if ppid <= 1 {
            break;
        }
        let name = match std::fs::read_to_string(format!("/proc/{ppid}/comm")) {
            Ok(s) => s.trim().to_string(),
            Err(_) => break,
        };
        names.push(name);
        pid = ppid;
    }
    names
}

/// On macOS, one `ps` call gets the whole process table; we walk up from our pid.
#[cfg(target_os = "macos")]
fn macos_ancestor_names() -> Vec<String> {
    use std::collections::HashMap;

    let output = match std::process::Command::new("ps")
        .args(["-ax", "-o", "pid=,ppid=,comm="])
        .output()
    {
        Ok(o) => o,
        Err(_) => return vec![],
    };

    // pid -> (ppid, comm)
    let mut by_pid: HashMap<u32, (u32, String)> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid_s), Some(ppid_s), Some(comm)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid_s.parse::<u32>(), ppid_s.parse::<u32>()) else {
            continue;
        };
        by_pid.insert(pid, (ppid, comm.to_string()));
    }

    let mut names = Vec::new();
    let mut pid = std::process::id();
    for _ in 0..16 {
        let Some((ppid, name)) = by_pid.get(&pid) else {
            break;
        };
        names.push(name.clone());
        if *ppid <= 1 {
            break;
        }
        pid = *ppid;
    }
    names
}

fn spawned_by_lefthook() -> bool {
    ancestor_names().iter().any(|name| name == "lefthook")
}

fn should_use_tui() -> bool {
    !spawned_by_lefthook() && std::io::stderr().is_terminal()
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
        Self {
            mp: if should_use_tui() {
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
            None => {
                eprintln!("  … {}", name);
                TaskSpinner {
                    inner: SpinnerInner::Plain,
                    name: name.to_string(),
                }
            }
        }
    }
}

impl Default for TaskProgress {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_spawned_by_lefthook_in_tests() {
        // When running under cargo test, no ancestor should be named "lefthook"
        assert!(!spawned_by_lefthook());
    }

    #[test]
    fn test_ancestors_are_non_empty() {
        let names = ancestor_names();
        assert!(
            !names.is_empty(),
            "should find at least one ancestor (cargo, sh, etc.)"
        );
    }

    #[test]
    fn test_plain_spinner_succeeds() {
        let s = TaskSpinner {
            inner: SpinnerInner::Plain,
            name: "test".into(),
        };
        s.set_message("hello");
        s.succeed(1.0);
    }

    #[test]
    fn test_plain_spinner_fails() {
        let s = TaskSpinner {
            inner: SpinnerInner::Plain,
            name: "test".into(),
        };
        s.fail(0.5);
    }

    #[test]
    fn test_plain_spinner_skips() {
        let s = TaskSpinner {
            inner: SpinnerInner::Plain,
            name: "test".into(),
        };
        s.skip("not needed");
        s.clear();
    }
}
