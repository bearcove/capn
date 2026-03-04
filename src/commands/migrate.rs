//! Config migration command from legacy captain paths to capn paths.

use owo_colors::OwoColorize;
use std::fs;
use std::path::PathBuf;

/// Migrate `.config/captain` to `.config/capn`.
pub fn run_migrate() {
    let workspace_dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("{} Failed to read current directory: {err}", "✗".red());
            std::process::exit(1);
        }
    };

    let config_root = workspace_dir.join(".config");
    let old_dir = config_root.join("captain");
    let new_dir = config_root.join("capn");

    if !old_dir.exists() {
        println!(
            "{} No legacy config found at {}",
            "ℹ".blue(),
            old_dir.display().to_string().cyan()
        );
        println!(
            "{} Current config path: {}",
            "ℹ".blue(),
            new_dir.display().to_string().cyan()
        );
        return;
    }

    if !new_dir.exists() {
        if let Err(err) = fs::create_dir_all(&config_root) {
            eprintln!(
                "{} Failed to create {}: {err}",
                "✗".red(),
                config_root.display().to_string().cyan()
            );
            std::process::exit(1);
        }

        if let Err(err) = fs::rename(&old_dir, &new_dir) {
            eprintln!(
                "{} Failed to move {} -> {}: {err}",
                "✗".red(),
                old_dir.display().to_string().cyan(),
                new_dir.display().to_string().cyan()
            );
            std::process::exit(1);
        }

        println!(
            "{} Migrated config directory {} -> {}",
            "✓".green(),
            old_dir.display().to_string().cyan(),
            new_dir.display().to_string().cyan()
        );
        return;
    }

    let archive = next_archive_path(&config_root);
    if let Err(err) = fs::rename(&old_dir, &archive) {
        eprintln!(
            "{} Found both legacy and new config directories, but failed to archive legacy {} -> {}: {err}",
            "✗".red(),
            old_dir.display().to_string().cyan(),
            archive.display().to_string().cyan()
        );
        std::process::exit(1);
    }

    println!(
        "{} {} already exists and takes precedence.",
        "✓".green(),
        new_dir.display().to_string().cyan()
    );
    println!(
        "{} Archived legacy config to {}",
        "✓".green(),
        archive.display().to_string().cyan()
    );
}

fn next_archive_path(config_root: &std::path::Path) -> PathBuf {
    let base = config_root.join("captain.migrated");
    if !base.exists() {
        return base;
    }

    for idx in 1.. {
        let candidate = config_root.join(format!("captain.migrated.{idx}"));
        if !candidate.exists() {
            return candidate;
        }
    }

    unreachable!()
}
