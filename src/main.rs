use captain_config::CaptainConfig;
use facet::Facet;
use facet_styx::StyxFormat;
use figue::{self as args, Driver};
use log::{Level, LevelFilter, Log, Metadata, Record, debug, error};
use owo_colors::{OwoColorize, Style};
use std::sync::mpsc;
use std::{
    borrow::Cow,
    ffi::OsStr,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};
use supports_color::{self, Stream as ColorStream};

mod checks;
mod commands;
mod jobs;
mod readme;
mod utils;

// Embed schema for zero-execution discovery by Styx tooling
styx_embed::embed_outdir_file!("schema.styx");

use checks::{check_edition_2024, check_external_path_deps};
pub use commands::debug_packages;
use commands::{run_init, run_pre_push, show_and_apply_jobs};
use jobs::{
    Job, enqueue_arborium_jobs, enqueue_cargo_lock_jobs, enqueue_readme_jobs, enqueue_rustfmt_jobs,
};
use utils::TaskProgress;

/// Git pre-commit and pre-push hooks for Rust projects.
#[derive(Facet, Debug)]
struct Args {
    /// Standard CLI options (--help, --version, --completions)
    #[facet(flatten)]
    builtins: args::FigueBuiltins,

    /// Command to run (defaults to pre-commit)
    #[facet(default, args::subcommand)]
    command: Option<Commands>,

    /// Configuration (from .config/captain/config.styx)
    #[facet(args::config)]
    config: CaptainConfig,
}

/// Available commands
#[derive(Facet, Debug)]
#[repr(u8)]
enum Commands {
    /// Run pre-commit checks (default when no command specified)
    PreCommit {
        /// Template directory for README generation
        #[facet(default, args::named)]
        template_dir: Option<PathBuf>,
    },
    /// Run pre-push checks
    PrePush,
    /// Initialize captain hooks in the repository
    Init,
    /// Debug package detection
    DebugPackages,
}

fn terminal_supports_color(stream: ColorStream) -> bool {
    supports_color::on_cached(stream).is_some()
}

fn maybe_strip_bytes<'a>(data: &'a [u8], stream: ColorStream) -> Cow<'a, [u8]> {
    if terminal_supports_color(stream) {
        Cow::Borrowed(data)
    } else {
        Cow::Owned(strip_ansi_escapes::strip(data))
    }
}

fn apply_color_env(cmd: &mut Command) {
    cmd.env("FORCE_COLOR", "1");
    cmd.env("CARGO_TERM_COLOR", "always");
}

fn command_with_color<S: AsRef<OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    apply_color_env(&mut cmd);
    cmd
}

/// Returns true if the given path is gitignored.
fn is_gitignored(path: &Path) -> bool {
    Command::new("git")
        .arg("check-ignore")
        .arg("-q")
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn main() {
    setup_logger();

    // Accept allowed log levels: trace, debug, error, warn, info
    log::set_max_level(LevelFilter::Info);
    if let Ok(log_level) = std::env::var("RUST_LOG") {
        let allowed = ["trace", "debug", "error", "warn", "info"];
        let log_level_lc = log_level.to_lowercase();
        if allowed.contains(&log_level_lc.as_str()) {
            let level = match log_level_lc.as_str() {
                "trace" => LevelFilter::Trace,
                "debug" => LevelFilter::Debug,
                "info" => LevelFilter::Info,
                "warn" => LevelFilter::Warn,
                "error" => LevelFilter::Error,
                _ => LevelFilter::Info,
            };
            log::set_max_level(level);
        }
    }

    // Parse config from CLI args and config file using figue
    let figue_config = args::builder::<Args>()
        .expect("failed to create figue builder")
        .cli(|c| c.args(std::env::args().skip(1)))
        .file(|f| {
            f.format(StyxFormat)
                .default_paths([".config/captain/config.styx"])
        })
        .help(|h| {
            h.program_name("captain")
                .description("Git pre-commit and pre-push hooks for Rust projects")
                .version(env!("CARGO_PKG_VERSION"))
        })
        .build();

    let args: Args = Driver::new(figue_config).run().unwrap();

    // Dispatch to the appropriate command
    match args.command {
        Some(Commands::PrePush) => {
            run_pre_push(args.config);
        }
        Some(Commands::Init) => {
            run_init();
        }
        Some(Commands::DebugPackages) => {
            debug_packages();
        }
        Some(Commands::PreCommit { template_dir }) | None => {
            run_pre_commit(args.config, template_dir);
        }
    }
}

fn run_pre_commit(config: CaptainConfig, template_dir: Option<PathBuf>) {
    let staged_files = match collect_staged_files() {
        Ok(sf) => sf,
        Err(e) => {
            error!(
                "Failed to collect staged files: {e}\n\
                    This tool requires Git to be installed and a Git repository initialized."
            );
            std::process::exit(1);
        }
    };

    // Create progress tracker with spinners
    let progress = TaskProgress::new();
    let start_time = std::time::Instant::now();

    // Load cargo metadata once (used by multiple operations)
    let metadata_spinner = progress.add_task("metadata");
    let metadata_start = std::time::Instant::now();
    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => {
            metadata_spinner.succeed(metadata_start.elapsed().as_secs_f32());
            m
        }
        Err(e) => {
            metadata_spinner.fail(metadata_start.elapsed().as_secs_f32());
            debug!("Failed to load workspace metadata: {}", e);
            std::process::exit(1);
        }
    };

    // Check edition 2024 requirement (bails if not met)
    if config.pre_commit.edition_2024
        && let Err(e) = check_edition_2024(&metadata)
    {
        eprintln!("{}\n{}", e.summary, e.details);
        std::process::exit(1);
    }

    // Check for external path dependencies (bails if found)
    if config.pre_commit.external_path_deps
        && let Err(e) = check_external_path_deps(&metadata)
    {
        eprintln!("{}\n{}", e.summary, e.details);
        std::process::exit(1);
    }

    // Use a channel to collect jobs from all tasks.
    let (tx_job, rx_job) = mpsc::channel();

    let mut handles = vec![];
    let mut spinners = vec![];

    if config.pre_commit.generate_readmes {
        let spinner = progress.add_task("readmes");
        spinners.push(("readmes", spinner, std::time::Instant::now()));
        handles.push(std::thread::spawn({
            let sender = tx_job.clone();
            let template_dir = template_dir.clone();
            let staged_files_clone = staged_files.clone();
            let metadata_clone = metadata.clone();
            move || {
                enqueue_readme_jobs(
                    sender,
                    template_dir.as_deref(),
                    &staged_files_clone,
                    &metadata_clone,
                );
            }
        }));
    }

    if config.pre_commit.rustfmt {
        let spinner = progress.add_task("rustfmt");
        spinners.push(("rustfmt", spinner, std::time::Instant::now()));
        handles.push(std::thread::spawn({
            let sender = tx_job.clone();
            move || {
                enqueue_rustfmt_jobs(sender, &staged_files);
            }
        }));
    }

    if config.pre_commit.cargo_lock {
        let spinner = progress.add_task("cargo-lock");
        spinners.push(("cargo-lock", spinner, std::time::Instant::now()));
        handles.push(std::thread::spawn({
            let sender = tx_job.clone();
            move || {
                enqueue_cargo_lock_jobs(sender);
            }
        }));
    }

    if config.pre_commit.arborium {
        let spinner = progress.add_task("arborium");
        spinners.push(("arborium", spinner, std::time::Instant::now()));
        handles.push(std::thread::spawn({
            let sender = tx_job.clone();
            let metadata_clone = metadata.clone();
            move || {
                enqueue_arborium_jobs(sender, &metadata_clone);
            }
        }));
    }

    drop(tx_job);

    let mut jobs: Vec<Job> = Vec::new();
    for job in rx_job {
        jobs.push(job);
    }

    for handle in handles.drain(..) {
        handle.join().unwrap();
    }

    // Mark all async spinners as complete
    for (_name, spinner, task_start) in spinners {
        spinner.succeed(task_start.elapsed().as_secs_f32());
    }

    jobs.retain(|job| !job.is_noop());
    jobs.retain(|job| !is_gitignored(&job.path));

    let total_elapsed = start_time.elapsed().as_secs_f32();
    println!(
        "\n  {} Pre-commit checks completed in {:.1}s\n",
        "✓".green(),
        total_elapsed
    );

    show_and_apply_jobs(&mut jobs);
}

#[derive(Debug, Clone)]
pub struct StagedFiles {
    /// Files that are staged (in the index) and not dirty (working tree matches index).
    clean: Vec<PathBuf>,
}

fn collect_staged_files() -> io::Result<StagedFiles> {
    let output = command_with_color("git")
        .arg("status")
        .arg("--porcelain")
        .output()?;
    if !output.status.success() {
        panic!("Failed to run `git status --porcelain`");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut clean = Vec::new();
    let cwd = std::env::current_dir()?;

    for line in stdout.lines() {
        // E.g. "M  src/main.rs", "A  foo.rs", "AM foo/bar.rs"
        if line.len() < 3 {
            log::trace!("Skipping short line: {:?}", line.dimmed());
            continue;
        }
        let x = line.chars().next().unwrap();
        let y = line.chars().nth(1).unwrap();
        let path = line[3..].to_string();

        log::trace!(
            "x: {:?}, y: {:?}, path: {:?}",
            x.magenta(),
            y.cyan(),
            path.dimmed()
        );

        // Staged and not dirty (to be formatted/committed)
        // Exclude deleted files (D) - they don't exist to read
        if x != ' ' && x != '?' && x != 'D' && y == ' ' {
            // Convert relative path to absolute for consistent comparison
            let abs_path = cwd.join(&path);
            log::debug!(
                "{} {}",
                "-> clean (staged, not dirty):".green().bold(),
                abs_path.display().to_string().blue()
            );
            clean.push(abs_path);
        }
    }
    Ok(StagedFiles { clean })
}

struct SimpleLogger;

impl Log for SimpleLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        // Create style based on log level
        let level_style = match record.level() {
            Level::Error => Style::new().fg_rgb::<243, 139, 168>(), // Catppuccin red (Maroon)
            Level::Warn => Style::new().fg_rgb::<249, 226, 175>(),  // Catppuccin yellow (Peach)
            Level::Info => Style::new().fg_rgb::<166, 227, 161>(),  // Catppuccin green (Green)
            Level::Debug => Style::new().fg_rgb::<137, 180, 250>(), // Catppuccin blue (Blue)
            Level::Trace => Style::new().fg_rgb::<148, 226, 213>(), // Catppuccin teal (Teal)
        };

        // Convert level to styled display
        eprintln!(
            "{} - {}: {}",
            record.level().style(level_style),
            record
                .target()
                .style(Style::new().fg_rgb::<137, 180, 250>()), // Blue for the target
            record.args()
        );
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Set up a simple logger.
fn setup_logger() {
    let logger = Box::new(SimpleLogger);
    log::set_boxed_logger(logger).unwrap();
    log::set_max_level(LevelFilter::Trace);
}
