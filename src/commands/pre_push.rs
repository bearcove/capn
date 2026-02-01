//! Pre-push hook implementation.

use crate::task::{TaskResult, TaskRunner, UnitResult};
use crate::{command_with_color, maybe_strip_bytes};
use captain_config::CaptainConfig;
use cargo_metadata::Metadata;
use owo_colors::OwoColorize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use supports_color::Stream as ColorStream;

/// Data collected from git fetch + diff operations
#[derive(Clone)]
#[allow(dead_code)]
pub struct GitInfo {
    pub origin_main_sha: String,
    pub head_sha: String,
    pub commit_count: u32,
    pub changed_files: Vec<String>,
    pub fetch_failed: bool,
}

/// Data about affected crates
#[derive(Clone)]
#[allow(dead_code)]
pub struct AffectedCrates {
    pub crates: BTreeSet<String>,
    pub crate_to_files: HashMap<String, Vec<String>>,
}

pub fn run_pre_push(config: CaptainConfig) {
    let mut config = config;

    // HAVE_MERCY levels:
    // 1 (or just set) = skip slow checks (tests, doc tests, docs)
    // 2 = also skip clippy (just cargo-shear)
    // 3 = skip everything
    if let Ok(mercy) = std::env::var("HAVE_MERCY") {
        let level: u8 = mercy.parse().unwrap_or(1);
        let mut skipped = Vec::new();

        if level >= 1 {
            config.pre_push.nextest = false;
            config.pre_push.doc_tests = false;
            config.pre_push.docs = false;
            skipped.extend(["nextest", "doc-tests", "docs"]);
        }
        if level >= 2 {
            config.pre_push.clippy = false;
            skipped.push("clippy");
        }
        if level >= 3 {
            config.pre_push.cargo_shear = false;
            skipped.push("cargo-shear");
        }

        println!(
            "{}",
            format!("🙏 HAVE_MERCY={}: skipping {}", level, skipped.join(", "))
                .yellow()
                .bold()
        );
    }

    // Show what's disabled via config (if anything)
    let mut config_disabled = Vec::new();
    if !config.pre_push.clippy {
        config_disabled.push("clippy");
    }
    if !config.pre_push.nextest {
        config_disabled.push("nextest");
    }
    if !config.pre_push.doc_tests {
        config_disabled.push("doc-tests");
    }
    if !config.pre_push.docs {
        config_disabled.push("docs");
    }
    if !config.pre_push.cargo_shear {
        config_disabled.push("cargo-shear");
    }
    if !config_disabled.is_empty() && std::env::var("HAVE_MERCY").is_err() {
        println!(
            "{}",
            format!("⏭️  Disabled via config: {}", config_disabled.join(", ")).dimmed()
        );
    }

    // Set up shared target directory
    setup_shared_target_dir();

    let mut runner = TaskRunner::new();

    // Root tasks with no dependencies
    let metadata_id = runner.add("metadata", load_metadata_task);
    let git_id = runner.add("git", git_fetch_and_diff_task);

    // Compute affected crates from metadata + git info
    let affected_id = runner.add_dep2(
        "affected",
        metadata_id,
        git_id,
        compute_affected_crates_task,
    );

    // Workspace-wide checks (no deps on affected crates)
    if config.pre_push.clippy {
        let features = config.pre_push.clippy_features.clone();
        runner.add("clippy", move || clippy_task(features));
    }

    if config.pre_push.cargo_shear {
        runner.add("cargo-shear", cargo_shear_task);
    }

    // Crate-specific checks (depend on affected crates)
    if config.pre_push.nextest {
        runner.add_dep1("build tests", affected_id, build_tests_task);
        runner.add_dep1("run tests", affected_id, run_tests_task);
    }

    if config.pre_push.doc_tests {
        let features = config.pre_push.doc_test_features.clone();
        runner.add_dep1("doc tests", affected_id, move |affected| {
            doc_tests_task(affected, features)
        });
    }

    if config.pre_push.docs {
        let features = config.pre_push.docs_features.clone();
        runner.add_dep1("docs", affected_id, move |affected| {
            docs_task(affected, features)
        });
    }

    // Run all tasks
    let results = runner.run();

    // Check for failures
    if results.has_failures() {
        results.print_failures();
        std::process::exit(1);
    }

    println!();
    println!("{} {}", "✅".green(), "All checks passed!".green().bold());

    std::process::exit(0);
}

// ============================================================================
// Task functions
// ============================================================================

fn load_metadata_task() -> TaskResult<Metadata> {
    match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => {
            // If this is a virtual workspace with no members, skip checks
            if m.workspace_members.is_empty() {
                return TaskResult::failed(
                    "empty workspace",
                    "No workspace members found, skipping pre-push checks",
                );
            }
            TaskResult::success(m)
        }
        Err(e) => {
            let err_str = e.to_string();
            // No Cargo.toml in this directory - not a Rust project
            if err_str.contains("could not find") {
                return TaskResult::failed(
                    "no Cargo.toml",
                    "No Cargo.toml found, skipping pre-push checks",
                );
            }
            // Check if this is an empty virtual workspace (no members)
            if err_str.contains("virtual manifest")
                || err_str.contains("no members")
                || err_str.contains("workspace has no members")
            {
                return TaskResult::failed(
                    "empty workspace",
                    "No workspace members found, skipping pre-push checks",
                );
            }
            TaskResult::failed("failed to load", e.to_string())
        }
    }
}

fn git_fetch_and_diff_task() -> TaskResult<GitInfo> {
    // Fetch from origin
    let fetch_output = Command::new("git")
        .args(["fetch", "origin", "main"])
        .output();

    let fetch_failed = match &fetch_output {
        Ok(output) if !output.status.success() => true,
        Err(_) => true,
        _ => false,
    };

    // Get commit range info
    let origin_main_sha = Command::new("git")
        .args(["rev-parse", "--short", "origin/main"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "origin/main".to_string());

    let head_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "HEAD".to_string());

    let commit_count = Command::new("git")
        .args(["rev-list", "--count", "origin/main..HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);

    // Get changed files
    let diff_output = command_with_color("git")
        .args(["diff", "--name-only", "origin/main", "HEAD"])
        .output();

    let changed_files: Vec<String> = match diff_output {
        Ok(output) if output.status.success() => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect(),
        Ok(output) => {
            return TaskResult::failed(
                "git diff failed",
                String::from_utf8_lossy(&output.stderr).to_string(),
            );
        }
        Err(e) => {
            return TaskResult::failed("git diff failed", e.to_string());
        }
    };

    TaskResult::success(GitInfo {
        origin_main_sha,
        head_sha,
        commit_count,
        changed_files,
        fetch_failed,
    })
}

fn compute_affected_crates_task(
    metadata: Arc<Metadata>,
    git: Arc<GitInfo>,
) -> TaskResult<AffectedCrates> {
    if git.changed_files.is_empty() {
        return TaskResult::failed("no changes", "No changes detected");
    }

    let workspace_root = metadata.workspace_root.clone().into_std_path_buf();

    // Get workspace member IDs
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| id.repr.clone())
        .collect();

    // Get excluded crates
    let excluded_crates: HashSet<String> = metadata
        .packages
        .iter()
        .filter(|pkg| !workspace_member_ids.contains(&pkg.id.repr))
        .map(|pkg| pkg.name.to_string())
        .collect();

    // Build directory to crate map
    let mut dir_to_crate: HashMap<String, String> = HashMap::new();
    for package in &metadata.packages {
        if let Some(parent) = package.manifest_path.parent() {
            dir_to_crate.insert(parent.to_string(), package.name.to_string());
        }
    }

    // Find affected crates
    let mut crate_to_files: HashMap<String, Vec<String>> = HashMap::new();

    for file in &git.changed_files {
        let initial_path = Path::new(file);
        let mut current_path = if initial_path.is_absolute() {
            PathBuf::from(initial_path)
        } else {
            workspace_root.join(initial_path)
        };

        loop {
            let current_str = current_path.to_string_lossy().to_string();
            if let Some(crate_name) = dir_to_crate.get(&current_str) {
                crate_to_files
                    .entry(crate_name.clone())
                    .or_default()
                    .push(file.clone());
                break;
            }

            if !current_path.pop() {
                break;
            }
        }
    }

    // Filter out excluded crates
    crate_to_files.retain(|crate_name, _| !excluded_crates.contains(crate_name));

    if crate_to_files.is_empty() {
        return TaskResult::failed(
            "no crates affected",
            "No publishable crates affected by changes",
        );
    }

    let crates: BTreeSet<_> = crate_to_files.keys().cloned().collect();

    TaskResult::success(AffectedCrates {
        crates,
        crate_to_files,
    })
}

fn clippy_task(features: Option<Vec<String>>) -> UnitResult {
    let mut args = vec!["clippy", "--workspace", "--all-targets"];

    let features_str: String;
    match &features {
        None => {
            args.push("--all-features");
        }
        Some(f) if !f.is_empty() => {
            args.push("--features");
            features_str = f.join(",");
            args.push(&features_str);
        }
        Some(_) => {}
    }

    args.extend(["--", "-D", "warnings"]);

    let output = command_with_color("cargo")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) if o.status.success() => TaskResult::success(()),
        Ok(o) => {
            let details = format_command_failure(&["cargo".to_string()], &args, &o);
            TaskResult::failed("clippy errors", details)
        }
        Err(e) => TaskResult::failed("failed to run", e.to_string()),
    }
}

fn cargo_shear_task() -> UnitResult {
    let output = command_with_color("cargo")
        .args(["shear"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) if o.status.success() => TaskResult::success(()),
        Ok(o) if indicates_missing_cargo_subcommand(&o, "shear") => {
            TaskResult::skipped("not installed")
        }
        Ok(o) => {
            let details = format_command_failure(&["cargo".to_string()], &["shear"], &o);
            TaskResult::failed("unused deps", details)
        }
        Err(e) => TaskResult::failed("failed to run", e.to_string()),
    }
}

fn build_tests_task(affected: Arc<AffectedCrates>) -> UnitResult {
    let mut args = vec!["nextest", "run", "--no-run"];

    for crate_name in &affected.crates {
        args.push("-p");
        args.push(crate_name);
    }

    let output = command_with_color("cargo")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) if o.status.success() => TaskResult::success(()),
        Ok(o) => {
            let details = format_command_failure(&["cargo".to_string()], &args, &o);
            TaskResult::failed("build failed", details)
        }
        Err(e) => TaskResult::failed("failed to run", e.to_string()),
    }
}

fn run_tests_task(affected: Arc<AffectedCrates>) -> UnitResult {
    let mut args = vec!["nextest", "run"];

    for crate_name in &affected.crates {
        args.push("-p");
        args.push(crate_name);
    }
    args.push("--no-tests=pass");

    let output = command_with_color("cargo")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) if o.status.success() => TaskResult::success(()),
        Ok(o) => {
            let details = format_command_failure(&["cargo".to_string()], &args, &o);
            TaskResult::failed("tests failed", details)
        }
        Err(e) => TaskResult::failed("failed to run", e.to_string()),
    }
}

fn doc_tests_task(affected: Arc<AffectedCrates>, features: Option<Vec<String>>) -> UnitResult {
    let mut args = vec!["test", "--doc"];

    for crate_name in &affected.crates {
        args.push("-p");
        args.push(crate_name);
    }

    let features_str: String;
    match &features {
        None => {
            args.push("--all-features");
        }
        Some(f) if !f.is_empty() => {
            args.push("--features");
            features_str = f.join(",");
            args.push(&features_str);
        }
        Some(_) => {}
    }

    let output = command_with_color("cargo")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) if o.status.success() => TaskResult::success(()),
        Ok(o) if should_skip_doc_tests(&o) => TaskResult::skipped("no lib to test"),
        Ok(o) => {
            let details = format_command_failure(&["cargo".to_string()], &args, &o);
            TaskResult::failed("doc tests failed", details)
        }
        Err(e) => TaskResult::failed("failed to run", e.to_string()),
    }
}

fn docs_task(affected: Arc<AffectedCrates>, features: Option<Vec<String>>) -> UnitResult {
    let mut args = vec!["doc", "--no-deps"];

    for crate_name in &affected.crates {
        args.push("-p");
        args.push(crate_name);
    }

    let features_str: String;
    match &features {
        None => {
            args.push("--all-features");
        }
        Some(f) if !f.is_empty() => {
            args.push("--features");
            features_str = f.join(",");
            args.push(&features_str);
        }
        Some(_) => {}
    }

    let output = command_with_color("cargo")
        .args(&args)
        .env("RUSTDOCFLAGS", "-D warnings")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();

    match output {
        Ok(o) if o.status.success() => TaskResult::success(()),
        Ok(o) => {
            let details = format_command_failure(&["cargo".to_string()], &args, &o);
            TaskResult::failed("doc warnings", details)
        }
        Err(e) => TaskResult::failed("failed to run", e.to_string()),
    }
}

// ============================================================================
// Helper functions
// ============================================================================

fn setup_shared_target_dir() {
    if let Some(home) = dirs::home_dir() {
        let target_dir = home.join(".captain").join("target");
        let _ = fs::create_dir_all(&target_dir);
        // SAFETY: We're single-threaded at this point
        unsafe { std::env::set_var("CARGO_TARGET_DIR", &target_dir) };
    }
}

fn format_command_failure(cmd: &[String], args: &[&str], output: &std::process::Output) -> String {
    let mut details = String::new();

    let full_cmd: Vec<String> = cmd
        .iter()
        .cloned()
        .chain(args.iter().map(|s| s.to_string()))
        .collect();
    details.push_str(&format!("command: {}\n", full_cmd.join(" ")));

    match output.status.code() {
        Some(code) => details.push_str(&format!("exit code: {}\n", code)),
        None => details.push_str("exit code: terminated by signal\n"),
    }

    if !output.stdout.is_empty() {
        let cleaned = maybe_strip_bytes(&output.stdout, ColorStream::Stdout);
        details.push_str(&format!(
            "stdout:\n{}\n",
            String::from_utf8_lossy(&cleaned).trim_end()
        ));
    }

    if !output.stderr.is_empty() {
        let cleaned = maybe_strip_bytes(&output.stderr, ColorStream::Stderr);
        details.push_str(&format!(
            "stderr:\n{}\n",
            String::from_utf8_lossy(&cleaned).trim_end()
        ));
    }

    details
}

fn cargo_subcommand_missing_message(stderr: &str, subcommand: &str) -> bool {
    let stderr_lower = stderr.to_lowercase();
    let patterns = [
        format!("no such command: `{}`", subcommand),
        format!("no such command: '{}'", subcommand),
        format!("no such subcommand: `{}`", subcommand),
        format!("no such subcommand: '{}'", subcommand),
    ];
    patterns
        .iter()
        .any(|pattern| stderr_lower.contains(&pattern.to_lowercase()))
}

fn indicates_missing_cargo_subcommand(output: &std::process::Output, subcommand: &str) -> bool {
    cargo_subcommand_missing_message(&String::from_utf8_lossy(&output.stderr), subcommand)
}

fn should_skip_doc_tests(output: &std::process::Output) -> bool {
    if output.status.code() != Some(101) {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("there is nothing to test")
        || stderr.contains("found no library targets to test")
        || stderr.contains("found no binaries to test")
        || stderr.contains("no library targets found")
}
