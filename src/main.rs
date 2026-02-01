use log::{Level, LevelFilter, Log, Metadata, Record, debug, error, warn};
use owo_colors::{OwoColorize, Style};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::{
    borrow::Cow,
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};
use supports_color::{self, Stream as ColorStream};
use toml_edit::{DocumentMut, Item};

mod readme;
mod utils;

// Embed schema for zero-execution discovery by Styx tooling
styx_embed::embed_outdir_file!("schema.styx");

use captain_config::CaptainConfig;
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

#[derive(Debug, Clone)]
struct Job {
    path: PathBuf,
    old_content: Option<Vec<u8>>,
    new_content: Vec<u8>,
    #[cfg(unix)]
    executable: bool,
}

impl Job {
    fn is_noop(&self) -> bool {
        match &self.old_content {
            Some(old) => {
                if &self.new_content != old {
                    return false;
                }
                #[cfg(unix)]
                {
                    // Check if executable bit would change
                    let current_executable = self
                        .path
                        .metadata()
                        .map(|m| m.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false);
                    current_executable == self.executable
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
            None => {
                #[cfg(unix)]
                {
                    self.new_content.is_empty() && !self.executable
                }
                #[cfg(not(unix))]
                {
                    self.new_content.is_empty()
                }
            }
        }
    }

    /// Applies the job by writing out the new_content to path and staging the file.
    fn apply(&self) -> std::io::Result<()> {
        use std::fs;

        // Create parent directories if they don't exist
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&self.path, &self.new_content)?;

        // Set executable bit if needed
        #[cfg(unix)]
        if self.executable {
            let mut perms = fs::metadata(&self.path)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&self.path, perms)?;
        }

        // Now stage it, best effort
        let _ = command_with_color("git")
            .arg("add")
            .arg(&self.path)
            .status();
        Ok(())
    }
}

/// Check that all workspace crates use edition 2024. Bails with an error if not.
fn check_edition_2024(metadata: &cargo_metadata::Metadata) {
    use std::collections::HashSet;

    let mut errors: Vec<String> = Vec::new();

    // Check workspace.package.edition in root Cargo.toml (if it exists)
    let workspace_root = &metadata.workspace_root;
    let root_cargo_toml = workspace_root.join("Cargo.toml");
    if root_cargo_toml.as_std_path().exists()
        && let Ok(content) = fs::read_to_string(root_cargo_toml.as_std_path())
        && let Ok(doc) = content.parse::<DocumentMut>()
        && let Some(workspace) = doc.get("workspace").and_then(Item::as_table)
        && let Some(package) = workspace.get("package").and_then(Item::as_table)
        && let Some(edition) = package.get("edition").and_then(Item::as_str)
        && edition != "2024"
    {
        errors.push(format!(
            "{}: [workspace.package].edition = {:?} (expected \"2024\")",
            root_cargo_toml, edition
        ));
    }

    // Get workspace members
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    // Check each workspace crate's edition
    for package in &metadata.packages {
        if !workspace_member_ids.contains(&package.id.repr) {
            continue;
        }

        let edition = &package.edition;
        if edition.as_str() != "2024" {
            errors.push(format!(
                "{}: edition = \"{}\" (expected \"2024\")",
                package.manifest_path,
                edition.as_str()
            ));
        }
    }

    if !errors.is_empty() {
        error!(
            "{}",
            "You have been deemed OUTDATED - edition 2024 now or bust".red()
        );
        error!("");
        for err in &errors {
            error!("  {} {}", "fix:".yellow(), err);
        }
        error!("");
        error!("Set edition = \"2024\" in the above location(s) to proceed.");
        std::process::exit(1);
    }
}

/// Check for path dependencies that point outside the workspace.
/// These are typically local development overrides that should not be committed.
/// Bails with an error if any are found.
fn check_external_path_deps(metadata: &cargo_metadata::Metadata) {
    let workspace_root = &metadata.workspace_root;

    let external_deps: Vec<_> = metadata
        .packages
        .iter()
        .filter(|pkg| {
            // source == None means it's a path dependency (not from crates.io or git)
            pkg.source.is_none()
        })
        .filter(|pkg| {
            // Check if manifest_path is outside workspace_root
            !pkg.manifest_path.starts_with(workspace_root)
        })
        .collect();

    if external_deps.is_empty() {
        return;
    }

    error!("{}", "External path dependencies detected!".red().bold());
    error!("");
    error!(
        "The following path dependencies point outside the workspace root ({}):",
        workspace_root
    );
    error!("");
    for pkg in &external_deps {
        error!(
            "  {} {} → {}",
            "✗".red(),
            pkg.name.as_str().yellow(),
            pkg.manifest_path.parent().unwrap_or(&pkg.manifest_path)
        );
    }
    error!("");
    error!("These are typically local development overrides (e.g., in [patch] sections)");
    error!("that should not be committed. They will break builds for other developers.");
    error!("");
    error!("To fix: comment out or remove the path dependencies before committing.");
    std::process::exit(1);
}

enum ConfigFormat {
    Styx,
    Yaml,
}

/// Strip @schema directive from config content.
/// The @schema directive is metadata for editors/LSPs, not actual config data.
fn strip_schema_directive(content: &str) -> String {
    content
        .lines()
        .filter(|line| !line.trim_start().starts_with("@schema"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn load_captain_config() -> CaptainConfig {
    let captain_dir = std::env::current_dir().unwrap().join(".config/captain");
    let styx_path = captain_dir.join("config.styx");
    let yaml_path = captain_dir.join("config.yaml");

    // Check for old KDL config and error out
    let old_kdl_path = captain_dir.join("config.kdl");
    if old_kdl_path.exists() {
        error!(
            "Found old KDL config file at {}\n\
             Captain now uses Styx configuration.\n\
             Please migrate your config.kdl to config.styx and remove the .kdl file.\n\
             See the README for the Styx syntax.",
            old_kdl_path.display()
        );
        std::process::exit(1);
    }

    // Check for config.yml (we standardize on .yaml for legacy, .styx for new)
    let yml_path = captain_dir.join("config.yml");
    if yml_path.exists() {
        error!(
            "Found config.yml at {}\n\
             Captain uses config.styx (or config.yaml for legacy).\n\
             Please rename config.yml to config.styx.",
            yml_path.display()
        );
        std::process::exit(1);
    }

    // Error if both .styx and .yaml exist
    if styx_path.exists() && yaml_path.exists() {
        error!(
            "Found both config.styx and config.yaml in {}\n\
             Please remove config.yaml and keep only config.styx.",
            captain_dir.display()
        );
        std::process::exit(1);
    }

    // Determine which config file to use
    let (config_path, format) = if styx_path.exists() {
        (styx_path, ConfigFormat::Styx)
    } else if yaml_path.exists() {
        warn!(
            "Using deprecated config.yaml at {}\n\
             Please migrate to config.styx. The YAML format will be removed in a future version.",
            yaml_path.display()
        );
        (yaml_path, ConfigFormat::Yaml)
    } else {
        debug!("No config file at {}, using defaults", styx_path.display());
        return CaptainConfig::default();
    };

    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to read config file {}: {e}", config_path.display());
            std::process::exit(1);
        }
    };

    // Handle empty file as defaults
    if content.trim().is_empty() {
        return CaptainConfig::default();
    }

    // Strip @schema directive (metadata for editors, not data)
    let content = strip_schema_directive(&content);

    match format {
        ConfigFormat::Styx => match facet_styx::from_str(&content) {
            Ok(config) => config,
            Err(e) => {
                error!("Failed to parse {}:\n{e}", config_path.display());
                std::process::exit(1);
            }
        },
        ConfigFormat::Yaml => match facet_yaml::from_str(&content) {
            Ok(config) => config,
            Err(e) => {
                error!("Failed to parse {}:\n{e}", config_path.display());
                std::process::exit(1);
            }
        },
    }
}

fn main() {
    setup_logger();

    miette::set_hook(Box::new(|_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .terminal_links(true)
                .unicode(true)
                .context_lines(3)
                .tab_width(4)
                .build(),
        )
    }))
    .expect("Failed to set miette hook");

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
    if config.pre_commit.edition_2024 {
        check_edition_2024(&metadata);
    }

    // Check for external path dependencies (bails if found)
    if config.pre_commit.external_path_deps {
        check_external_path_deps(&metadata);
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
struct StagedFiles {
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
    use super::*;

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
