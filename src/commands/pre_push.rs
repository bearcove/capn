//! Pre-push hook implementation.

use crate::config::load_captain_config;
use crate::utils::{TaskProgress, dir_size, format_size, run_command_with_spinner};
use crate::{command_with_color, maybe_strip_bytes};
use log::{error, warn};
use owo_colors::OwoColorize;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};
use supports_color::Stream as ColorStream;

// Move from main.rs:
// - run_pre_push (lines 1163-1925)
// - get_shared_target_dir (lines 1160-1162)
// - debug_packages (lines 1081-1158) - or separate debug_packages.rs
//
// Helper functions used by pre_push:
// - shell_escape (lines 944-952)
// - format_command_line (lines 954-960)
// - cargo_subcommand_missing_message (lines 962-973)
// - indicates_missing_cargo_subcommand (lines 975-977)
// - print_clippy_fix_hint (lines 979-997)
// - print_shear_fix_hint (lines 999-1004)
// - print_stream (lines 1006-1015)
// - print_env_vars (lines 1017-1021)
// - exit_with_command_failure (lines 1023-1040)
// - exit_with_command_error (lines 1042-1056)
// - should_skip_doc_tests (lines 1058-1066)

pub fn run_pre_push() {
    use std::collections::{BTreeSet, HashSet};

    let mut config = load_captain_config();

    // HAVE_MERCY levels:
    // 1 (or just set) = skip slow checks (tests, doc tests, docs)
    // 2 = also skip clippy (just cargo-shear)
    // 3 = skip everything, just check formatting basically
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
    let shared_target_dir = get_shared_target_dir();

    // Use a channel so we can do non-blocking receive for dir_size
    let (dir_size_tx, dir_size_rx) = mpsc::channel::<(PathBuf, u64)>();
    if let Some(ref target_dir) = shared_target_dir {
        // Create the directory if it doesn't exist
        let _ = fs::create_dir_all(target_dir);

        // Set CARGO_TARGET_DIR for all subsequent cargo commands
        // SAFETY: We're single-threaded at this point, before spawning any cargo commands
        unsafe { std::env::set_var("CARGO_TARGET_DIR", target_dir) };

        // Calculate dir size in background (it's just informational)
        let target_dir_clone = target_dir.clone();
        std::thread::spawn(move || {
            let size = dir_size(&target_dir_clone);
            let _ = dir_size_tx.send((target_dir_clone, size));
        });
    }

    // Create progress tracker for setup phase - must be before any spawned tasks
    let progress = TaskProgress::new();

    // Spawn git fetch in background - we'll check the result at the end
    let fetch_spinner = progress.add_task("fetch");
    let fetch_start = std::time::Instant::now();
    let fetch_handle = std::thread::spawn(|| {
        Command::new("git")
            .args(["fetch", "origin", "main"])
            .output()
    });

    // Load workspace metadata
    let metadata_spinner = progress.add_task("metadata");
    let metadata_start = std::time::Instant::now();
    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => {
            metadata_spinner.succeed(metadata_start.elapsed().as_secs_f32());
            m
        }
        Err(e) => {
            metadata_spinner.fail(metadata_start.elapsed().as_secs_f32());
            let err_str = e.to_string();
            // No Cargo.toml in this directory - not a Rust project
            if err_str.contains("could not find") {
                println!(
                    "{}",
                    "No Cargo.toml found, skipping pre-push checks".yellow()
                );
                std::process::exit(0);
            }
            // Check if this is an empty virtual workspace (no members)
            if err_str.contains("virtual manifest")
                || err_str.contains("no members")
                || err_str.contains("workspace has no members")
            {
                println!(
                    "{}",
                    "No workspace members found, skipping pre-push checks".yellow()
                );
                std::process::exit(0);
            }
            error!("Failed to get workspace metadata: {}", e);
            std::process::exit(1);
        }
    };

    // If this is a virtual workspace with no members, skip checks
    if metadata.workspace_members.is_empty() {
        println!(
            "{}",
            "No workspace members found, skipping pre-push checks".yellow()
        );
        std::process::exit(0);
    }

    let workspace_root = metadata.workspace_root.clone().into_std_path_buf();

    // Type alias for background task results
    type CommandResult = (
        Vec<String>,
        Result<std::process::Output, std::io::Error>,
        Duration,
    );

    // Start workspace-wide checks immediately (don't wait for git fetch/diff)
    // These don't need to know which specific crates changed

    // 1. Run clippy on entire workspace
    let clippy_spinner = if config.pre_push.clippy {
        Some(progress.add_task("clippy"))
    } else {
        None
    };

    if let Some(ref spinner) = clippy_spinner {
        let start = std::time::Instant::now();
        let mut clippy_command = vec!["cargo".to_string(), "clippy".to_string()];
        clippy_command.push("--workspace".to_string());
        clippy_command.push("--all-targets".to_string());
        // Use configured features, or --all-features if not specified
        match &config.pre_push.clippy_features {
            None => {
                clippy_command.push("--all-features".to_string());
            }
            Some(features) if !features.is_empty() => {
                clippy_command.push("--features".to_string());
                clippy_command.push(features.join(","));
            }
            Some(_) => {
                // Empty features list means no extra features
            }
        }
        clippy_command.extend(vec![
            "--".to_string(),
            "-D".to_string(),
            "warnings".to_string(),
        ]);

        let clippy_output = run_command_with_spinner(&clippy_command, &[], spinner);
        let elapsed = start.elapsed();

        match clippy_output {
            Ok(output) if output.status.success() => {
                spinner.succeed(elapsed.as_secs_f32());
            }
            Ok(output) => {
                spinner.fail(elapsed.as_secs_f32());
                let hint_command = clippy_command.clone();
                exit_with_command_failure(
                    &clippy_command,
                    &[],
                    output,
                    Some(Box::new(move || print_clippy_fix_hint(&hint_command))),
                );
            }
            Err(e) => {
                spinner.fail(elapsed.as_secs_f32());
                let hint_command = clippy_command.clone();
                exit_with_command_error(
                    &clippy_command,
                    &[],
                    e,
                    Some(Box::new(move || print_clippy_fix_hint(&hint_command))),
                );
            }
        }
    }

    // 2. Spawn cargo-shear in background (completely independent)
    let shear_spinner = if config.pre_push.cargo_shear {
        Some(progress.add_task("cargo-shear"))
    } else {
        None
    };

    let shear_handle: Option<std::thread::JoinHandle<CommandResult>> =
        if config.pre_push.cargo_shear {
            let handle = std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let shear_command = vec!["cargo".to_string(), "shear".to_string()];
                let mut cmd = command_with_color(&shear_command[0]);
                for arg in &shear_command[1..] {
                    cmd.arg(arg);
                }
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());
                let output = cmd.output();
                let elapsed = start.elapsed();
                (shear_command, output, elapsed)
            });
            Some(handle)
        } else {
            None
        };

    // Get the set of workspace member crate IDs
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| id.repr.clone())
        .collect();

    // Get the set of excluded crate names (those that are packages but not workspace members)
    let excluded_crates: HashSet<String> = metadata
        .packages
        .iter()
        .filter(|pkg| !workspace_member_ids.contains(&pkg.id.repr))
        .map(|pkg| pkg.name.to_string())
        .collect();

    // Wait for git fetch to complete before checking changed files
    // This ensures origin/main is up-to-date for an accurate diff
    let fetch_result = fetch_handle.join();
    let fetch_elapsed = fetch_start.elapsed().as_secs_f32();
    let fetch_failed = match &fetch_result {
        Ok(Ok(output)) if !output.status.success() => {
            fetch_spinner.fail(fetch_elapsed);
            warn!(
                "Failed to fetch from origin: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            true
        }
        Ok(Err(e)) => {
            fetch_spinner.fail(fetch_elapsed);
            warn!("Failed to run git fetch: {}", e);
            true
        }
        Err(_) => {
            fetch_spinner.fail(fetch_elapsed);
            warn!("git fetch thread panicked");
            true
        }
        Ok(Ok(_)) => false,
    };

    // Get commit range info for display
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

    // Update fetch spinner with commit range info
    let commit_label = if commit_count == 1 {
        "commit"
    } else {
        "commits"
    };
    fetch_spinner.succeed_with_message(format!(
        "  {} {:<14} {} {}..{} ({} {})",
        "✓".green(),
        "fetch",
        format!("{:.1}s", fetch_elapsed).dimmed(),
        origin_main_sha,
        head_sha,
        commit_count,
        commit_label
    ));

    // Get the list of changed files between origin/main and HEAD
    let diff_spinner = progress.add_task("diff");
    let diff_start = std::time::Instant::now();
    let mut changed_files: std::collections::BTreeSet<String> = BTreeSet::new();

    let diff_output = command_with_color("git")
        .args(["diff", "--name-only", "origin/main", "HEAD"])
        .output();

    match diff_output {
        Ok(output) if output.status.success() => {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                changed_files.insert(line.to_string());
            }
            diff_spinner.succeed(diff_start.elapsed().as_secs_f32());
        }
        Err(e) => {
            diff_spinner.fail(diff_start.elapsed().as_secs_f32());
            error!("Failed to get changed files: {}", e);
            std::process::exit(1);
        }
        Ok(output) => {
            diff_spinner.fail(diff_start.elapsed().as_secs_f32());
            error!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::process::exit(1);
        }
    };

    let changed_files: Vec<_> = changed_files.into_iter().collect();

    if changed_files.is_empty() {
        println!("{}", "No changes detected".green().bold());
        std::process::exit(0);
    }

    // Build a map from directory to crate name using workspace packages
    let mut dir_to_crate: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for package in &metadata.packages {
        if let Some(parent) = package.manifest_path.parent() {
            dir_to_crate.insert(parent.to_string(), package.name.to_string());
        }
    }

    // Find which crates are affected and track which files triggered each
    let mut crate_to_files: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for file in &changed_files {
        let initial_path = Path::new(file);
        let mut current_path = if initial_path.is_absolute() {
            PathBuf::from(initial_path)
        } else {
            workspace_root.join(initial_path)
        };

        // Find the crate directory by walking up the path
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

    if crate_to_files.is_empty() {
        println!("{}", "No crates affected by changes".yellow());
        std::process::exit(0);
    }

    // Filter affected crates to exclude those in the excluded list
    crate_to_files.retain(|crate_name, _| !excluded_crates.contains(crate_name));

    if crate_to_files.is_empty() {
        println!("{}", "No publishable crates affected by changes".yellow());
        std::process::exit(0);
    }

    // Sort for consistent output
    let affected_crates: BTreeSet<_> = crate_to_files.keys().cloned().collect();

    // Create spinners for crate-specific tasks
    let build_spinner = if config.pre_push.nextest {
        Some(progress.add_task("build tests"))
    } else {
        None
    };
    let test_spinner = if config.pre_push.nextest {
        Some(progress.add_task("run tests"))
    } else {
        None
    };
    let doctest_spinner = if config.pre_push.doc_tests {
        Some(progress.add_task("doc tests"))
    } else {
        None
    };
    let docs_spinner = if config.pre_push.docs {
        Some(progress.add_task("docs"))
    } else {
        None
    };

    // 1. Build nextest tests
    let test_handle: Option<std::thread::JoinHandle<CommandResult>> = if config.pre_push.nextest {
        let build_spinner = build_spinner.as_ref().unwrap();
        let start = std::time::Instant::now();
        let mut build_command = vec![
            "cargo".to_string(),
            "nextest".to_string(),
            "run".to_string(),
            "--no-run".to_string(),
        ];
        for crate_name in &affected_crates {
            build_command.push("-p".to_string());
            build_command.push(crate_name.to_string());
        }

        let build_output = run_command_with_spinner(&build_command, &[], build_spinner);
        let elapsed = start.elapsed();

        match build_output {
            Ok(output) if output.status.success() => {
                build_spinner.succeed(elapsed.as_secs_f32());
            }
            Ok(output) => {
                build_spinner.fail(elapsed.as_secs_f32());
                exit_with_command_failure(&build_command, &[], output, None);
            }
            Err(e) => {
                build_spinner.fail(elapsed.as_secs_f32());
                exit_with_command_error(&build_command, &[], e, None);
            }
        }

        // Spawn test runner in background
        let mut run_command = vec![
            "cargo".to_string(),
            "nextest".to_string(),
            "run".to_string(),
        ];
        for crate_name in &affected_crates {
            run_command.push("-p".to_string());
            run_command.push(crate_name.to_string());
        }
        run_command.push("--no-tests=pass".to_string());

        let handle = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut cmd = command_with_color(&run_command[0]);
            for arg in &run_command[1..] {
                cmd.arg(arg);
            }
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            let output = cmd.output();
            let elapsed = start.elapsed();
            (run_command, output, elapsed)
        });
        Some(handle)
    } else {
        None
    };

    // 2. Spawn doc tests in background (independent of other tasks)
    let doctest_handle: Option<std::thread::JoinHandle<CommandResult>> =
        if doctest_spinner.is_some() {
            let affected_crates_clone = affected_crates.clone();
            let doc_test_features = config.pre_push.doc_test_features.clone();
            let handle = std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let mut doctest_command =
                    vec!["cargo".to_string(), "test".to_string(), "--doc".to_string()];
                for crate_name in &affected_crates_clone {
                    doctest_command.push("-p".to_string());
                    doctest_command.push(crate_name.to_string());
                }
                // Use configured features, or --all-features if not specified
                match &doc_test_features {
                    None => {
                        doctest_command.push("--all-features".to_string());
                    }
                    Some(features) if !features.is_empty() => {
                        doctest_command.push("--features".to_string());
                        doctest_command.push(features.join(","));
                    }
                    Some(_) => {
                        // Empty features list means no extra features
                    }
                }

                let mut cmd = command_with_color(&doctest_command[0]);
                for arg in &doctest_command[1..] {
                    cmd.arg(arg);
                }
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());
                let output = cmd.output();
                let elapsed = start.elapsed();
                (doctest_command, output, elapsed)
            });
            Some(handle)
        } else {
            None
        };

    // 3. Spawn docs build in background (independent of other tasks)
    let docs_handle: Option<std::thread::JoinHandle<CommandResult>> = if docs_spinner.is_some() {
        let affected_crates_clone = affected_crates.clone();
        let docs_features = config.pre_push.docs_features.clone();
        let handle = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut doc_command = vec![
                "cargo".to_string(),
                "doc".to_string(),
                "--no-deps".to_string(),
            ];
            for crate_name in &affected_crates_clone {
                doc_command.push("-p".to_string());
                doc_command.push(crate_name.to_string());
            }
            // Use configured features, or --all-features if not specified
            match &docs_features {
                None => {
                    doc_command.push("--all-features".to_string());
                }
                Some(features) if !features.is_empty() => {
                    doc_command.push("--features".to_string());
                    doc_command.push(features.join(","));
                }
                Some(_) => {
                    // Empty features list means no extra features
                }
            }
            let mut doc_cmd = command_with_color(&doc_command[0]);
            for arg in &doc_command[1..] {
                doc_cmd.arg(arg);
            }
            doc_cmd.env("RUSTDOCFLAGS", "-D warnings");
            doc_cmd.stdout(Stdio::piped());
            doc_cmd.stderr(Stdio::piped());
            let output = doc_cmd.output();
            let elapsed = start.elapsed();
            (doc_command, output, elapsed)
        });
        Some(handle)
    } else {
        None
    };

    // 4. Wait for cargo-shear background task
    if let Some(handle) = shear_handle {
        let spinner = shear_spinner.as_ref().unwrap();

        match handle.join() {
            Ok((shear_command, output_result, elapsed)) => match output_result {
                Ok(output) if output.status.success() => {
                    spinner.succeed(elapsed.as_secs_f32());
                }
                Ok(output) if indicates_missing_cargo_subcommand(&output, "shear") => {
                    spinner.skip("not installed");
                }
                Ok(output) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_failure(
                        &shear_command,
                        &[],
                        output,
                        Some(Box::new(print_shear_fix_hint)),
                    );
                }
                Err(e) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_error(&shear_command, &[], e, None);
                }
            },
            Err(_) => {
                spinner.fail(0.0);
                error!("cargo-shear thread panicked");
                std::process::exit(1);
            }
        }
    }

    // 5. Wait for doc tests
    if let Some(handle) = doctest_handle {
        let spinner = doctest_spinner.as_ref().unwrap();

        match handle.join() {
            Ok((doctest_command, output_result, elapsed)) => match output_result {
                Ok(output) if output.status.success() => {
                    spinner.succeed(elapsed.as_secs_f32());
                }
                Ok(output) if should_skip_doc_tests(&output) => {
                    // No lib to test - just hide this task
                    spinner.clear();
                }
                Ok(output) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_failure(&doctest_command, &[], output, None);
                }
                Err(e) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_error(&doctest_command, &[], e, None);
                }
            },
            Err(_) => {
                spinner.fail(0.0);
                error!("doc tests thread panicked");
                std::process::exit(1);
            }
        }
    }

    // 6. Wait for docs build
    if let Some(handle) = docs_handle {
        let spinner = docs_spinner.as_ref().unwrap();

        match handle.join() {
            Ok((doc_command, output_result, elapsed)) => match output_result {
                Ok(output) if output.status.success() => {
                    spinner.succeed(elapsed.as_secs_f32());
                }
                Ok(output) => {
                    spinner.fail(elapsed.as_secs_f32());
                    let doc_env = [("RUSTDOCFLAGS", "-D warnings")];
                    exit_with_command_failure(&doc_command, &doc_env, output, None);
                }
                Err(e) => {
                    spinner.fail(elapsed.as_secs_f32());
                    let doc_env = [("RUSTDOCFLAGS", "-D warnings")];
                    exit_with_command_error(&doc_command, &doc_env, e, None);
                }
            },
            Err(_) => {
                spinner.fail(0.0);
                error!("docs build thread panicked");
                std::process::exit(1);
            }
        }
    }

    // 7. Wait for test results
    if let Some(handle) = test_handle {
        let spinner = test_spinner.as_ref().unwrap();

        match handle.join() {
            Ok((run_command, output_result, elapsed)) => match output_result {
                Ok(output) if output.status.success() => {
                    spinner.succeed(elapsed.as_secs_f32());
                }
                Ok(output) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_failure(&run_command, &[], output, None);
                }
                Err(e) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_error(&run_command, &[], e, None);
                }
            },
            Err(_) => {
                spinner.fail(0.0);
                error!("test runner thread panicked");
                std::process::exit(1);
            }
        }
    }

    println!();
    println!("{} {}", "✅".green(), "All checks passed!".green().bold());

    // Print affected crates summary
    println!();
    println!("{}", "Dirty crates:".cyan().bold());
    for crate_name in &affected_crates {
        if let Some(files) = crate_to_files.get(crate_name) {
            let file_list = if files.len() <= 3 {
                files.join(", ")
            } else {
                format!("{}, ... (+{} more)", files[..3].join(", "), files.len() - 3)
            };
            println!(
                "  {} {}",
                format!("{}:", crate_name).yellow(),
                file_list.dimmed()
            );
        }
    }

    // Print shared target dir size (non-blocking check)
    if shared_target_dir.is_some() {
        // Try to receive with a short timeout - don't block if still calculating
        if let Ok((target_dir, size)) = dir_size_rx.recv_timeout(Duration::from_millis(100)) {
            println!(
                "   {} {} ({})",
                "Target:".dimmed(),
                target_dir.display().to_string().blue(),
                format_size(size).dimmed()
            );
        }
    }

    // Check if fetch failed earlier (we already waited for it before diffing)
    print!("   {} ", "Branch:".dimmed());
    io::stdout().flush().unwrap();

    if fetch_failed {
        println!("{}", "fetch failed".red());
        std::process::exit(1);
    }

    // Check if current branch is fast-forward to origin/main
    let merge_base_output = command_with_color("git")
        .args(["merge-base", "HEAD", "origin/main"])
        .output();

    let merge_base = match merge_base_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => {
            println!("{}", "failed".red());
            error!("Failed to find merge base with origin/main");
            std::process::exit(1);
        }
    };

    // Get origin/main rev
    let origin_main_rev = match command_with_color("git")
        .args(["rev-parse", "origin/main"])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => {
            println!("{}", "failed".red());
            error!("Failed to get origin/main revision");
            std::process::exit(1);
        }
    };

    // Check if origin/main is ahead of merge_base (meaning we need to rebase)
    if origin_main_rev != merge_base {
        println!("{}", "rebase needed".yellow());
        println!();
        println!(
            "{} {}",
            "⚠️".yellow(),
            "Your branch has diverged from origin/main".yellow().bold()
        );
        println!("  Please rebase your changes and push again:");
        println!("    {}", "git rebase origin/main".cyan());
        std::process::exit(1);
    }

    println!("{}", "up to date with origin/main".dimmed());
    std::process::exit(0);
}

/// Get the shared target directory for pre-push checks (~/.captain/target)
fn get_shared_target_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".captain").join("target"))
}

fn shell_escape(part: &str) -> String {
    if part
        .chars()
        .all(|c| !c.is_whitespace() && c != '"' && c != '\'' && c != '\\')
    {
        part.to_string()
    } else {
        format!("{:?}", part)
    }
}

fn format_command_line(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_escape(p))
        .collect::<Vec<_>>()
        .join(" ")
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

fn print_clippy_fix_hint(command: &[String]) {
    let mut fix_command = Vec::with_capacity(command.len() + 2);
    let mut inserted = false;

    for part in command {
        if !inserted && part == "--" {
            fix_command.push("--allow-dirty".to_string());
            fix_command.push("--fix".to_string());
            inserted = true;
        }
        fix_command.push(part.clone());
    }

    if !inserted {
        fix_command.push("--allow-dirty".to_string());
        fix_command.push("--fix".to_string());
    }

    println!(
        "    {} Try auto-fixing with:\n        {}\n        git commit --amend --no-edit",
        "💡".cyan(),
        format_command_line(&fix_command)
    );
}

fn print_shear_fix_hint() {
    println!(
        "    {} Try cleaning unused dependencies with:\n        cargo shear --fix",
        "💡".cyan()
    );
}

fn print_stream(label: &str, data: &[u8], stream: ColorStream) {
    if data.is_empty() {
        println!("    {}: <empty>", label);
    } else {
        let cleaned = maybe_strip_bytes(data, stream);
        let text = String::from_utf8_lossy(&cleaned);
        println!("    {}:\n{}", label, text.trim_end());
    }
}

fn print_env_vars(envs: &[(&str, &str)]) {
    for (key, value) in envs {
        println!("    env: {}={}", key, value);
    }
}

fn exit_with_command_failure(
    command: &[String],
    envs: &[(&str, &str)],
    output: std::process::Output,
    hint: Option<Box<dyn FnOnce()>>,
) -> ! {
    println!("    command: {}", format_command_line(command));
    if !envs.is_empty() {
        print_env_vars(envs);
    }
    match output.status.code() {
        Some(code) => println!("    exit code: {}", code),
        None => println!("    exit code: terminated by signal"),
    }
    print_stream("stdout", &output.stdout, ColorStream::Stdout);
    print_stream("stderr", &output.stderr, ColorStream::Stderr);
    if let Some(action) = hint {
        action();
    }
    std::process::exit(1);
}

fn exit_with_command_error(
    command: &[String],
    envs: &[(&str, &str)],
    error: std::io::Error,
    hint: Option<Box<dyn FnOnce()>>,
) -> ! {
    println!("    command: {}", format_command_line(command));
    if !envs.is_empty() {
        print_env_vars(envs);
    }
    println!("    error: {}", error);
    if let Some(action) = hint {
        action();
    }
    std::process::exit(1);
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
