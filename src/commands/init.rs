//! Capn initialization command.

use owo_colors::OwoColorize;
use std::fs;
use std::io::{self, Write};
use std::process::Command;
use tracing::error;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Initialize capn in the current repository
pub fn run_init() {
    println!("{}", "Capn initialization".cyan().bold());
    println!();

    let workspace_dir = std::env::current_dir().unwrap();

    // Check if we're in a git repo
    let git_check = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output();

    if git_check.is_err() || !git_check.unwrap().status.success() {
        error!("Not in a git repository. Please run 'git init' first.");
        std::process::exit(1);
    }

    let mut files_created = Vec::new();

    // 1. Create hooks directory and hook files
    if prompt_yes_no("Create git hooks (pre-commit, pre-push)?", true) {
        let hooks_dir = workspace_dir.join("hooks");

        // Create hooks directory
        if !hooks_dir.exists() {
            fs::create_dir_all(&hooks_dir).expect("Failed to create hooks directory");
        }

        // pre-commit hook
        let pre_commit_path = hooks_dir.join("pre-commit");
        let pre_commit_content = r#"#!/bin/bash
capn
"#;
        fs::write(&pre_commit_path, pre_commit_content).expect("Failed to write pre-commit hook");
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&pre_commit_path)
                .expect("Failed to get pre-commit metadata")
                .permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&pre_commit_path, perms)
                .expect("Failed to set pre-commit permissions");
        }
        files_created.push(pre_commit_path);

        // pre-push hook
        let pre_push_path = hooks_dir.join("pre-push");
        let pre_push_content = r#"#!/bin/bash
capn pre-push
"#;
        fs::write(&pre_push_path, pre_push_content).expect("Failed to write pre-push hook");
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&pre_push_path)
                .expect("Failed to get pre-push metadata")
                .permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&pre_push_path, perms).expect("Failed to set pre-push permissions");
        }
        files_created.push(pre_push_path);

        // install.sh script
        let install_path = hooks_dir.join("install.sh");
        let install_content = r#"#!/usr/bin/env bash
set -euo pipefail

HOOK_SOURCE_DIR="$(git rev-parse --show-toplevel)/hooks"
GIT_DIR="$(git rev-parse --git-dir)"

copy_hook() {
  local src="$1"
  local dst="$2"

  mkdir -p "$(dirname "$dst")"
  cp "$src" "$dst"
  chmod +x "$dst"

  echo "✔ installed $(basename "$src") → $dst"
}

install_for_dir() {
  local hook_dir="$1"

  for hook in "$HOOK_SOURCE_DIR"/*; do
    local name
    name="$(basename "$hook")"
    # Skip install.sh itself
    if [ "$name" = "install.sh" ]; then
      continue
    fi
    local target="$hook_dir/$name"

    copy_hook "$hook" "$target"
  done
}

echo "Installing hooks from $HOOK_SOURCE_DIR …"

# main repo
install_for_dir "$GIT_DIR/hooks"

# worktrees
for wt in "$GIT_DIR"/worktrees/*; do
  if [ -d "$wt" ]; then
    install_for_dir "$wt/hooks"
  fi
done

echo "All hooks installed successfully."
"#;
        fs::write(&install_path, install_content).expect("Failed to write install.sh");
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&install_path)
                .expect("Failed to get install.sh metadata")
                .permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&install_path, perms)
                .expect("Failed to set install.sh permissions");
        }
        files_created.push(install_path);

        println!("  {} Created hooks/pre-commit", "✔".green());
        println!("  {} Created hooks/pre-push", "✔".green());
        println!("  {} Created hooks/install.sh", "✔".green());
    }

    // 2. Create conductor.json for https://www.conductor.build/
    println!();
    if prompt_yes_no(
        "Create conductor.json for Conductor (conductor.build)?",
        true,
    ) {
        let conductor_json_path = workspace_dir.join("conductor.json");
        let conductor_content = r#"{
  "scripts": {
    "setup": "hooks/install.sh"
  }
}
"#;
        fs::write(&conductor_json_path, conductor_content).expect("Failed to write conductor.json");
        files_created.push(conductor_json_path);

        println!("  {} Created conductor.json", "✔".green());
    }

    // 3. Create .config/capn/ directory with config.styx
    println!();
    let capn_dir = workspace_dir.join(".config/capn");
    let config_path = capn_dir.join("config.styx");

    if !capn_dir.exists() {
        if prompt_yes_no("Create .config/capn/ with config.styx?", true) {
            fs::create_dir_all(&capn_dir).expect("Failed to create capn config directory");

            // Create default config.styx
            let config_content = r#"@schema {id crate:capn-config@1, cli capn}

// Capn configuration
// Most options default to true. Set to false to disable.

pre-commit {
  // generate-readmes true // deprecated and ignored; use cargo-reedme
  // rustfmt false
  // cargo-lock false
  // arborium false
  // edition-2024 false
  // external-path-deps false
  // internal-dev-deps-release-plz false
}

pre-push {
  // clippy false
  // nextest false
  // doc-tests false
  // docs false
  // cargo-shear false

  // Feature configuration (uncomment and customize as needed)
  // clippy-features (feature1 feature2)
  // doc-test-features (feature1)
  // docs-features (feature1)
}
"#;
            fs::write(&config_path, config_content).expect("Failed to write config.styx");
            files_created.push(config_path);
            println!("  {} Created .config/capn/config.styx", "✔".green());
        }
    } else {
        println!("  {} .config/capn/ already exists, skipping", "ℹ".blue());
    }

    println!();

    if files_created.is_empty() {
        println!("{}", "No files created.".yellow());
    } else {
        println!("{}", "Initialization complete!".green().bold());
        println!();
        println!("Next steps:");
        println!(
            "  1. Run {} to install git hooks",
            "hooks/install.sh".cyan()
        );
        println!("  2. Commit the new files");
    }
}

/// Prompt the user for yes/no confirmation
fn prompt_yes_no(question: &str, default: bool) -> bool {
    let default_hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("{} {} ", question, default_hint);
    io::stdout().flush().unwrap();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return default;
    }

    let input = input.trim().to_lowercase();
    if input.is_empty() {
        return default;
    }

    matches!(input.as_str(), "y" | "yes")
}
