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
mod config;
mod jobs;
mod readme;
mod utils;

// Embed schema for zero-execution discovery by Styx tooling
styx_embed::embed_outdir_file!("schema.styx");

use checks::{check_edition_2024, check_external_path_deps};
pub use commands::debug_packages;
use commands::{run_init, run_pre_push, show_and_apply_jobs};
use config::load_captain_config;
use jobs::{
    Job, enqueue_arborium_jobs, enqueue_cargo_lock_jobs, enqueue_readme_jobs, enqueue_rustfmt_jobs,
};
use utils::TaskProgress;

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

    // Parse CLI arguments
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 && args[1] == "pre-push" {
        run_pre_push();
        return;
    }
    if args.len() > 1 && args[1] == "init" {
        run_init();
        return;
    }
    if args.len() > 1 && args[1] == "debug-packages" {
        debug_packages();
        return;
    }

    // Parse --template-dir argument
    let mut template_dir: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--template-dir" && i + 1 < args.len() {
            template_dir = Some(PathBuf::from(&args[i + 1]));
            i += 2;
        } else {
            i += 1;
        }
    }

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

    // Load captain config
    let config = load_captain_config();

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

#[cfg(test)]
mod tests {
    use captain_config::CaptainConfig;

    fn parse_config(yaml: &str) -> CaptainConfig {
        facet_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn empty_config_uses_defaults() {
        // Empty YAML document (just empty object)
        let config: CaptainConfig = parse_config("{}");
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt);
        assert!(config.pre_commit.cargo_lock);
        assert!(config.pre_commit.arborium);
        assert!(config.pre_commit.edition_2024);
        assert!(config.pre_push.clippy);
        assert!(config.pre_push.nextest);
        assert!(!config.pre_push.doc_tests);
        assert!(config.pre_push.docs);
        assert!(config.pre_push.cargo_shear);
        assert!(config.pre_push.clippy_features.is_none());
        assert!(config.pre_push.doc_test_features.is_none());
        assert!(config.pre_push.docs_features.is_none());
    }

    #[test]
    fn empty_blocks_use_defaults() {
        let yaml = r#"
pre-commit: {}
pre-push: {}
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_push.clippy);
    }

    #[test]
    fn disable_pre_commit_options() {
        let yaml = r#"
pre-commit:
  generate-readmes: false
  rustfmt: false
  cargo-lock: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(!config.pre_commit.generate_readmes);
        assert!(!config.pre_commit.rustfmt);
        assert!(!config.pre_commit.cargo_lock);
        // Others still default to true
        assert!(config.pre_commit.arborium);
        assert!(config.pre_commit.edition_2024);
    }

    #[test]
    fn disable_pre_push_options() {
        let yaml = r#"
pre-push:
  clippy: false
  nextest: false
  doc-tests: false
  docs: false
  cargo-shear: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(!config.pre_push.clippy);
        assert!(!config.pre_push.nextest);
        assert!(!config.pre_push.doc_tests);
        assert!(!config.pre_push.docs);
        assert!(!config.pre_push.cargo_shear);
    }

    #[test]
    fn feature_lists() {
        let yaml = r#"
pre-push:
  clippy-features:
    - serde
    - async
  doc-test-features:
    - full
  docs-features:
    - all-features
    - experimental
"#;
        let config: CaptainConfig = parse_config(yaml);

        let clippy_features = config.pre_push.clippy_features.unwrap();
        assert_eq!(clippy_features, vec!["serde", "async"]);

        let doc_test_features = config.pre_push.doc_test_features.unwrap();
        assert_eq!(doc_test_features, vec!["full"]);

        let docs_features = config.pre_push.docs_features.unwrap();
        assert_eq!(docs_features, vec!["all-features", "experimental"]);
    }

    #[test]
    fn mixed_config() {
        let yaml = r#"
pre-commit:
  generate-readmes: false
  arborium: false
pre-push:
  nextest: false
  clippy-features:
    - serde
"#;
        let config: CaptainConfig = parse_config(yaml);

        assert!(!config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt); // default
        assert!(!config.pre_commit.arborium);

        assert!(config.pre_push.clippy); // default
        assert!(!config.pre_push.nextest);

        let clippy_features = config.pre_push.clippy_features.unwrap();
        assert_eq!(clippy_features, vec!["serde"]);
    }

    #[test]
    fn only_pre_commit_block() {
        let yaml = r#"
pre-commit:
  rustfmt: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(!config.pre_commit.rustfmt);
        // pre-push defaults
        assert!(config.pre_push.clippy);
        assert!(config.pre_push.nextest);
    }

    #[test]
    fn only_pre_push_block() {
        let yaml = r#"
pre-push:
  clippy: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        // pre-commit defaults
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt);
        // pre-push override
        assert!(!config.pre_push.clippy);
    }

    #[test]
    fn comment_only_blocks_use_defaults() {
        // This is what users get when they have a config with only comments under a key
        // YAML parses `pre-commit:` with only comments as null
        let yaml = r#"
# Captain configuration
# All options default to true unless noted. Set to false to disable.

pre-commit:
  # generate-readmes: false
  # rustfmt: false
  # cargo-lock: false
  # arborium: false
  # edition-2024: false

pre-push:
  # clippy: false
  # nextest: false
  # doc-tests: false
  # docs: false
  # cargo-shear: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        // All defaults should apply
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt);
        assert!(config.pre_commit.cargo_lock);
        assert!(config.pre_commit.arborium);
        assert!(config.pre_commit.edition_2024);
        assert!(config.pre_push.clippy);
        assert!(config.pre_push.nextest);
        assert!(!config.pre_push.doc_tests);
        assert!(config.pre_push.docs);
        assert!(config.pre_push.cargo_shear);
    }

    #[test]
    fn top_level_comments_only() {
        // File with only comments should use all defaults
        let yaml = r#"
# Captain configuration
# This file is intentionally empty - all defaults apply
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_push.clippy);
    }

    #[test]
    fn mixed_comments_and_values() {
        let yaml = r#"
pre-commit:
  # Keep rustfmt enabled (default)
  generate-readmes: false
  # cargo-lock: false

pre-push:
  clippy: false
  # nextest stays enabled
  clippy-features:
    - serde
    # - disabled-feature
    - tokio
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(!config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt); // default
        assert!(config.pre_commit.cargo_lock); // default (commented out)
        assert!(!config.pre_push.clippy);
        assert!(config.pre_push.nextest); // default
        let features = config.pre_push.clippy_features.unwrap();
        assert_eq!(features, vec!["serde", "tokio"]);
    }
}
