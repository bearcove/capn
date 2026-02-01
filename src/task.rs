//! Unified task system for checks and jobs.
//!
//! All work in captain (checks, jobs, etc.) runs as tasks that:
//! 1. Run in parallel (spawned in threads)
//! 2. Report progress through the spinner infrastructure
//! 3. Can update their status label dynamically
//! 4. Report success/failure consistently
//! 5. Collect detailed failure info for display after all tasks complete

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use owo_colors::OwoColorize;

use crate::utils::{TaskProgress, TaskSpinner};

/// Unique identifier for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(u64);

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

impl TaskId {
    fn new() -> Self {
        Self(NEXT_ID.fetch_add(1, Ordering::SeqCst))
    }
}

use crate::jobs::Job;

/// Result of a task execution.
pub enum TaskResult {
    /// Task succeeded, optionally with a message and jobs to apply
    Success {
        /// Optional message to show (e.g., "3 files formatted")
        message: Option<String>,
        /// Jobs (file changes) to apply after all tasks complete
        jobs: Vec<Job>,
    },

    /// Task was skipped (e.g., not enabled in config, nothing to do)
    Skipped {
        /// Why it was skipped
        reason: String,
    },

    /// Task failed
    Failed {
        /// One-line summary shown in the spinner output
        summary: String,
        /// Detailed error info printed after all tasks complete
        details: String,
    },
}

impl TaskResult {
    pub fn success() -> Self {
        Self::Success {
            message: None,
            jobs: Vec::new(),
        }
    }

    pub fn success_with(message: impl Into<String>) -> Self {
        Self::Success {
            message: Some(message.into()),
            jobs: Vec::new(),
        }
    }

    pub fn success_with_jobs(jobs: Vec<Job>) -> Self {
        Self::Success {
            message: None,
            jobs,
        }
    }

    pub fn success_with_jobs_and_message(message: impl Into<String>, jobs: Vec<Job>) -> Self {
        Self::Success {
            message: Some(message.into()),
            jobs,
        }
    }

    pub fn skipped(reason: impl Into<String>) -> Self {
        Self::Skipped {
            reason: reason.into(),
        }
    }

    pub fn failed(summary: impl Into<String>, details: impl Into<String>) -> Self {
        Self::Failed {
            summary: summary.into(),
            details: details.into(),
        }
    }
}

/// A boxed task function.
type BoxedTask = Box<dyn FnOnce(&TaskHandle) -> TaskResult + Send>;

/// Event sent to the runner.
enum Event {
    /// Update spinner message
    SetMessage(TaskId, String),
    /// Spawn a new child task
    Spawn(String, BoxedTask),
    /// Task completed
    Complete(TaskId, String, TaskResult),
}

/// Handle for a task to update its spinner and spawn subtasks.
#[derive(Clone)]
pub struct TaskHandle {
    id: TaskId,
    event_tx: mpsc::Sender<Event>,
}

impl TaskHandle {
    /// Update the spinner message (e.g., "checking 3/10 files...")
    pub fn set_message(&self, msg: impl Into<String>) {
        let _ = self.event_tx.send(Event::SetMessage(self.id, msg.into()));
    }

    /// Spawn a child task that runs in parallel with its own spinner.
    pub fn spawn(
        &self,
        name: impl Into<String>,
        task: impl FnOnce(&TaskHandle) -> TaskResult + Send + 'static,
    ) {
        let _ = self
            .event_tx
            .send(Event::Spawn(name.into(), Box::new(task)));
    }
}

/// Collected results from running all tasks.
pub struct TaskResults {
    results: Vec<(String, TaskResult)>,
}

impl TaskResults {
    pub fn has_failures(&self) -> bool {
        self.results
            .iter()
            .any(|(_, r)| matches!(r, TaskResult::Failed { .. }))
    }

    /// Print detailed failure information for all failed tasks.
    pub fn print_failures(&self) {
        for (name, result) in &self.results {
            if let TaskResult::Failed { summary, details } = result {
                eprintln!("\n{} {}: {}", "✗".red(), name.yellow(), summary);
                if !details.is_empty() {
                    eprintln!("{}", details);
                }
            }
        }
    }

    /// Collect all jobs from successful tasks.
    pub fn collect_jobs(self) -> Vec<Job> {
        let mut jobs = Vec::new();
        for (_, result) in self.results {
            if let TaskResult::Success {
                jobs: task_jobs, ..
            } = result
            {
                jobs.extend(task_jobs);
            }
        }
        jobs
    }
}

/// Info about a running task.
struct TaskInfo {
    name: String,
    spinner: TaskSpinner,
    start: Instant,
}

/// Runs tasks in parallel with progress spinners.
pub struct TaskRunner {
    progress: TaskProgress,
    tasks: Vec<(String, BoxedTask)>,
}

impl TaskRunner {
    pub fn new() -> Self {
        Self {
            progress: TaskProgress::new(),
            tasks: Vec::new(),
        }
    }

    /// Add a task to run.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        task: impl FnOnce(&TaskHandle) -> TaskResult + Send + 'static,
    ) {
        self.tasks.push((name.into(), Box::new(task)));
    }

    /// Run all tasks in parallel, collecting results.
    pub fn run(self) -> TaskResults {
        let (event_tx, event_rx) = mpsc::channel::<Event>();

        let mut spinners: HashMap<TaskId, TaskInfo> = HashMap::new();
        let mut pending = 0usize;
        let mut results = Vec::new();

        // Spawn initial tasks
        for (name, task) in self.tasks {
            spawn_task(name, task, &self.progress, &event_tx, &mut spinners);
            pending += 1;
        }

        // Event loop - single blocking recv, no polling
        while pending > 0 {
            let event = match event_rx.recv() {
                Ok(e) => e,
                Err(_) => break, // Channel closed
            };

            match event {
                Event::SetMessage(id, msg) => {
                    if let Some(info) = spinners.get(&id) {
                        info.spinner.set_message(&msg);
                    }
                }
                Event::Spawn(name, task) => {
                    spawn_task(name, task, &self.progress, &event_tx, &mut spinners);
                    pending += 1;
                }
                Event::Complete(id, name, result) => {
                    if let Some(info) = spinners.remove(&id) {
                        let elapsed = info.start.elapsed().as_secs_f32();
                        finalize_spinner(info.spinner, &info.name, &result, elapsed);
                        results.push((name, result));
                        pending -= 1;
                    }
                }
            }
        }

        TaskResults { results }
    }
}

fn spawn_task(
    name: String,
    task: BoxedTask,
    progress: &TaskProgress,
    event_tx: &mpsc::Sender<Event>,
    spinners: &mut HashMap<TaskId, TaskInfo>,
) {
    let id = TaskId::new();
    let spinner = progress.add_task(&name);
    spinners.insert(
        id,
        TaskInfo {
            name: name.clone(),
            spinner,
            start: Instant::now(),
        },
    );

    let handle = TaskHandle {
        id,
        event_tx: event_tx.clone(),
    };

    thread::spawn(move || {
        let result = task(&handle);
        let _ = handle.event_tx.send(Event::Complete(id, name, result));
    });
}

fn finalize_spinner(spinner: TaskSpinner, name: &str, result: &TaskResult, elapsed: f32) {
    match result {
        TaskResult::Success { message, .. } => {
            if let Some(msg) = message {
                spinner.succeed_with_message(format!(
                    "  {} {:<14} {} {}",
                    "✓".green(),
                    name,
                    format!("{:.1}s", elapsed).dimmed(),
                    msg.dimmed(),
                ));
            } else {
                spinner.succeed(elapsed);
            }
        }
        TaskResult::Skipped { reason } => {
            spinner.skip(reason);
        }
        TaskResult::Failed { .. } => {
            spinner.fail(elapsed);
        }
    }
}

impl Default for TaskRunner {
    fn default() -> Self {
        Self::new()
    }
}
