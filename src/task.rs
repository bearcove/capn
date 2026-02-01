//! Unified task system for checks and jobs.
//!
//! All work in captain (checks, jobs, etc.) runs as tasks that:
//! 1. Run in parallel (spawned in threads)
//! 2. Report progress through the spinner infrastructure
//! 3. Support typed dependencies between tasks
//! 4. Report success/failure consistently
//! 5. Collect detailed failure info for display after all tasks complete

use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use owo_colors::OwoColorize;

use crate::jobs::Job;
use crate::utils::{TaskProgress, TaskSpinner};

// ============================================================================
// CloneAny trait for type-erased cloneable storage
// ============================================================================

trait CloneAny: Any + Send {
    fn clone_box(&self) -> Box<dyn CloneAny>;
    fn as_any(&self) -> &dyn Any;
}

impl<T: Clone + Send + 'static> CloneAny for T {
    fn clone_box(&self) -> Box<dyn CloneAny> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Typed identifier for a task that produces a value of type `T`.
pub struct TaskId<T>(u64, PhantomData<T>);

// Manual impls because PhantomData<T> doesn't require T: Clone/Copy
impl<T> Clone for TaskId<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for TaskId<T> {}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

impl<T> TaskId<T> {
    fn new() -> Self {
        Self(NEXT_ID.fetch_add(1, Ordering::SeqCst), PhantomData)
    }

    fn raw(&self) -> u64 {
        self.0
    }
}

/// Output from a task - contains the produced value and any jobs.
pub struct TaskOutput<T> {
    /// The value produced by this task (will be wrapped in Arc for sharing)
    pub value: T,
    /// Jobs (file changes) to apply after all tasks complete
    pub jobs: Vec<Job>,
}

impl<T> TaskOutput<T> {
    pub fn new(value: T) -> Self {
        Self {
            value,
            jobs: Vec::new(),
        }
    }

    pub fn with_jobs(value: T, jobs: Vec<Job>) -> Self {
        Self { value, jobs }
    }
}

/// Result of a task execution.
pub enum TaskResult<T> {
    /// Task succeeded with output
    Success(TaskOutput<T>),

    /// Task was skipped (e.g., not enabled in config)
    Skipped { reason: String },

    /// Task failed
    Failed { summary: String, details: String },
}

impl<T> TaskResult<T> {
    pub fn success(value: T) -> Self {
        Self::Success(TaskOutput::new(value))
    }

    pub fn success_with_jobs(value: T, jobs: Vec<Job>) -> Self {
        Self::Success(TaskOutput::with_jobs(value, jobs))
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

/// Shorthand for tasks that don't produce a meaningful value.
pub type UnitResult = TaskResult<()>;

// ============================================================================
// Internal types for type-erased storage
// ============================================================================

/// Type-erased task result for internal storage
enum InternalResult {
    Success {
        /// The output value, boxed as CloneAny (actually Arc<T>)
        value: Box<dyn CloneAny>,
        jobs: Vec<Job>,
    },
    Skipped {
        reason: String,
    },
    Failed {
        summary: String,
        details: String,
    },
}

/// Type-erased task function
type BoxedTask = Box<dyn FnOnce() -> InternalResult + Send>;

/// Internal event sent to the runner
enum Event {
    /// Task completed
    Complete(u64, String, InternalResult),
}

/// Pending task waiting for dependencies
struct PendingTask {
    id: u64,
    name: String,
    deps: Vec<u64>,
    /// Creates the BoxedTask once dependencies are resolved
    make_task: Box<dyn FnOnce(&HashMap<u64, Box<dyn CloneAny>>) -> BoxedTask + Send>,
}

/// Info about a running task
struct RunningTask {
    name: String,
    spinner: TaskSpinner,
    start: Instant,
}

/// Collected results from running all tasks.
pub struct TaskResults {
    results: Vec<(String, InternalResult)>,
}

impl TaskResults {
    pub fn has_failures(&self) -> bool {
        self.results
            .iter()
            .any(|(_, r)| matches!(r, InternalResult::Failed { .. }))
    }

    /// Print detailed failure information for all failed tasks.
    pub fn print_failures(&self) {
        for (name, result) in &self.results {
            if let InternalResult::Failed { summary, details } = result {
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
            if let InternalResult::Success {
                jobs: task_jobs, ..
            } = result
            {
                jobs.extend(task_jobs);
            }
        }
        jobs
    }
}

/// Runs tasks in parallel with progress spinners and dependency resolution.
pub struct TaskRunner {
    progress: TaskProgress,
    pending: Vec<PendingTask>,
}

impl TaskRunner {
    pub fn new() -> Self {
        Self {
            progress: TaskProgress::new(),
            pending: Vec::new(),
        }
    }

    /// Add a task with no dependencies.
    pub fn add<T, F>(&mut self, name: impl Into<String>, task: F) -> TaskId<T>
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> TaskResult<T> + Send + 'static,
    {
        let id = TaskId::<T>::new();
        let name = name.into();

        self.pending.push(PendingTask {
            id: id.raw(),
            name,
            deps: vec![],
            make_task: Box::new(move |_outputs| Box::new(move || run_task(task))),
        });

        id
    }

    /// Add a task with one dependency.
    pub fn add_dep1<T, D1, F>(
        &mut self,
        name: impl Into<String>,
        dep: TaskId<D1>,
        task: F,
    ) -> TaskId<T>
    where
        T: Send + Sync + 'static,
        D1: Send + Sync + 'static,
        F: FnOnce(Arc<D1>) -> TaskResult<T> + Send + 'static,
    {
        let id = TaskId::<T>::new();
        let name = name.into();
        let dep_id = dep.raw();

        self.pending.push(PendingTask {
            id: id.raw(),
            name,
            deps: vec![dep_id],
            make_task: Box::new(move |outputs| {
                let d1: Arc<D1> = outputs
                    .get(&dep_id)
                    .expect("dependency not found")
                    .as_any()
                    .downcast_ref::<Arc<D1>>()
                    .expect("dependency type mismatch")
                    .clone();
                Box::new(move || run_task(move || task(d1)))
            }),
        });

        id
    }

    /// Add a task with two dependencies.
    pub fn add_dep2<T, D1, D2, F>(
        &mut self,
        name: impl Into<String>,
        dep1: TaskId<D1>,
        dep2: TaskId<D2>,
        task: F,
    ) -> TaskId<T>
    where
        T: Send + Sync + 'static,
        D1: Send + Sync + 'static,
        D2: Send + Sync + 'static,
        F: FnOnce(Arc<D1>, Arc<D2>) -> TaskResult<T> + Send + 'static,
    {
        let id = TaskId::<T>::new();
        let name = name.into();
        let dep1_id = dep1.raw();
        let dep2_id = dep2.raw();

        self.pending.push(PendingTask {
            id: id.raw(),
            name,
            deps: vec![dep1_id, dep2_id],
            make_task: Box::new(move |outputs| {
                let d1: Arc<D1> = outputs
                    .get(&dep1_id)
                    .expect("dependency not found")
                    .as_any()
                    .downcast_ref::<Arc<D1>>()
                    .expect("dependency type mismatch")
                    .clone();
                let d2: Arc<D2> = outputs
                    .get(&dep2_id)
                    .expect("dependency not found")
                    .as_any()
                    .downcast_ref::<Arc<D2>>()
                    .expect("dependency type mismatch")
                    .clone();
                Box::new(move || run_task(move || task(d1, d2)))
            }),
        });

        id
    }

    /// Run all tasks, respecting dependencies.
    pub fn run(self) -> TaskResults {
        let (event_tx, event_rx) = mpsc::channel::<Event>();

        // Completed task outputs (id -> Arc<T> as Box<dyn CloneAny>)
        let mut outputs: HashMap<u64, Box<dyn CloneAny>> = HashMap::new();
        // Currently running tasks
        let mut running: HashMap<u64, RunningTask> = HashMap::new();
        // Tasks waiting for dependencies
        let mut waiting: Vec<PendingTask> = self.pending;
        // Collected results
        let mut results = Vec::new();

        // Helper to check if all deps are satisfied
        let deps_ready = |deps: &[u64], outputs: &HashMap<u64, Box<dyn CloneAny>>| {
            deps.iter().all(|d| outputs.contains_key(d))
        };

        // Initial spawn of tasks with no dependencies
        let mut i = 0;
        while i < waiting.len() {
            if deps_ready(&waiting[i].deps, &outputs) {
                let pending = waiting.remove(i);
                spawn_pending(pending, &self.progress, &event_tx, &mut running, &outputs);
            } else {
                i += 1;
            }
        }

        // Event loop
        while !running.is_empty() || !waiting.is_empty() {
            let event = match event_rx.recv() {
                Ok(e) => e,
                Err(_) => break,
            };

            match event {
                Event::Complete(id, name, result) => {
                    // Finalize spinner
                    if let Some(info) = running.remove(&id) {
                        let elapsed = info.start.elapsed().as_secs_f32();
                        finalize_spinner(info.spinner, &info.name, &result, elapsed);
                    }

                    // Store output if successful
                    if let InternalResult::Success { ref value, .. } = result {
                        outputs.insert(id, value.clone_box());
                    }

                    results.push((name, result));

                    // Check if any waiting tasks can now run
                    let mut i = 0;
                    while i < waiting.len() {
                        if deps_ready(&waiting[i].deps, &outputs) {
                            let pending = waiting.remove(i);
                            spawn_pending(
                                pending,
                                &self.progress,
                                &event_tx,
                                &mut running,
                                &outputs,
                            );
                        } else {
                            i += 1;
                        }
                    }
                }
            }
        }

        TaskResults { results }
    }
}

/// Run a typed task and convert to internal result
fn run_task<T: Send + Sync + 'static>(task: impl FnOnce() -> TaskResult<T>) -> InternalResult {
    match task() {
        TaskResult::Success(output) => InternalResult::Success {
            value: Box::new(Arc::new(output.value)),
            jobs: output.jobs,
        },
        TaskResult::Skipped { reason } => InternalResult::Skipped { reason },
        TaskResult::Failed { summary, details } => InternalResult::Failed { summary, details },
    }
}

/// Spawn a pending task
fn spawn_pending(
    pending: PendingTask,
    progress: &TaskProgress,
    event_tx: &mpsc::Sender<Event>,
    running: &mut HashMap<u64, RunningTask>,
    outputs: &HashMap<u64, Box<dyn CloneAny>>,
) {
    let id = pending.id;
    let spinner = progress.add_task(&pending.name);
    let name = pending.name.clone();

    running.insert(
        id,
        RunningTask {
            name: pending.name,
            spinner,
            start: Instant::now(),
        },
    );

    let task = (pending.make_task)(outputs);
    let tx = event_tx.clone();

    thread::spawn(move || {
        let result = task();
        let _ = tx.send(Event::Complete(id, name, result));
    });
}

fn finalize_spinner(spinner: TaskSpinner, _name: &str, result: &InternalResult, elapsed: f32) {
    match result {
        InternalResult::Success { .. } => {
            spinner.succeed(elapsed);
        }
        InternalResult::Skipped { reason } => {
            spinner.skip(reason);
        }
        InternalResult::Failed { .. } => {
            spinner.fail(elapsed);
        }
    }
}

impl Default for TaskRunner {
    fn default() -> Self {
        Self::new()
    }
}
