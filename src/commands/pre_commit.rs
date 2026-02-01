//! Pre-commit hook implementation.

use captain_config::CaptainConfig;
use log::{debug, error};
use owo_colors::OwoColorize;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use crate::checks::{check_edition_2024, check_external_path_deps};
use crate::jobs::{
    Job, enqueue_arborium_jobs, enqueue_cargo_lock_jobs, enqueue_readme_jobs, enqueue_rustfmt_jobs,
};
use crate::utils::TaskProgress;

pub fn run_pre_commit(config: CaptainConfig, template_dir: Option<PathBuf>) {
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
    pub clean: Vec<PathBuf>,
}

fn collect_staged_files() -> io::Result<StagedFiles> {
    let output = Command::new("git")
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

pub fn show_and_apply_jobs(jobs: &mut [Job]) {
    jobs.sort_by_key(|job| job.path.clone());

    if jobs.is_empty() {
        println!("{}", "All generated files are up-to-date".green().bold());
        return;
    }

    // Apply all jobs first
    for job in jobs.iter() {
        if let Err(e) = job.apply() {
            eprintln!("Failed to apply {}: {e}", job.path.display());
            std::process::exit(1);
        }
    }

    // Print clean summary
    println!(
        "\n{}",
        "These files have been automatically formatted and staged:".green()
    );
    for job in jobs.iter() {
        let ext = job.path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let icon = icon_for_extension(ext);
        println!("  {} {}", icon.cyan(), job.path.display());
    }
    println!(
        "\n{}",
        "The commit is ready to push - no 'git amend' is necessary.".green()
    );
    std::process::exit(0);
}

/// Returns a Nerd Font icon for the given file extension
fn icon_for_extension(ext: &str) -> &'static str {
    match ext {
        // Languages
        "rs" => "\u{e7a8}",                         //  Rust
        "js" => "\u{e74e}",                         //  JavaScript
        "ts" => "\u{e628}",                         //  TypeScript
        "jsx" | "tsx" => "\u{e7ba}",                //  React
        "py" => "\u{e73c}",                         //  Python
        "rb" => "\u{e791}",                         //  Ruby
        "go" => "\u{e626}",                         //  Go
        "java" => "\u{e738}",                       //  Java
        "c" | "h" => "\u{e61e}",                    //  C
        "cpp" | "cc" | "cxx" | "hpp" => "\u{e61d}", //  C++
        "cs" => "\u{f031b}",                        // 󰌛 C#
        "swift" => "\u{e755}",                      //  Swift
        "kt" | "kts" => "\u{e634}",                 //  Kotlin
        "php" => "\u{e73d}",                        //  PHP
        "lua" => "\u{e620}",                        //  Lua
        "zig" => "\u{e6a9}",                        //  Zig
        "hs" => "\u{e777}",                         //  Haskell
        "ex" | "exs" => "\u{e62d}",                 //  Elixir
        "erl" => "\u{e7b1}",                        //  Erlang
        "scala" => "\u{e737}",                      //  Scala
        "clj" | "cljs" => "\u{e768}",               //  Clojure
        "r" => "\u{f07d4}",                         // 󰟔 R
        "jl" => "\u{e624}",                         //  Julia
        "pl" | "pm" => "\u{e769}",                  //  Perl
        "sh" | "bash" | "zsh" => "\u{e795}",        //  Shell
        "fish" => "\u{f489}",                       //  Fish
        "ps1" => "\u{e70f}",                        //  PowerShell
        "vim" => "\u{e62b}",                        //  Vim
        "el" => "\u{e779}",                         //  Emacs Lisp

        // Web
        "html" | "htm" => "\u{e736}",  //  HTML
        "css" => "\u{e749}",           //  CSS
        "scss" | "sass" => "\u{e74b}", //  Sass
        "less" => "\u{e758}",          //  Less
        "vue" => "\u{e6a0}",           //  Vue
        "svelte" => "\u{e697}",        //  Svelte
        "astro" => "\u{e6b3}",         //  Astro
        "wasm" => "\u{e6a1}",          //  WebAssembly

        // Data/Config
        "json" => "\u{e60b}",            //  JSON
        "yaml" | "yml" => "\u{e6a8}",    //  YAML
        "toml" => "\u{e6b2}",            //  TOML
        "xml" => "\u{f05c0}",            // 󰗀 XML
        "csv" => "\u{f0219}",            // 󰈙 CSV
        "sql" => "\u{e706}",             //  SQL
        "graphql" | "gql" => "\u{e662}", //  GraphQL
        "proto" => "\u{e6a5}",           //  Protobuf

        // Documentation
        "md" | "markdown" => "\u{e73e}", //  Markdown
        "txt" => "\u{f0219}",            // 󰈙 Text
        "pdf" => "\u{f0226}",            // 󰈦 PDF
        "doc" | "docx" => "\u{f0219}",   // 󰈙 Word
        "rst" => "\u{f0219}",            // 󰈙 reStructuredText

        // Build/Package
        "lock" => "\u{f023}",       //  Lock file
        "dockerfile" => "\u{e7b0}", //  Docker
        "nix" => "\u{f313}",        //  Nix
        "cmake" => "\u{e615}",      //  CMake

        // Images
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "webp" => "\u{f03e}", //  Image
        "svg" => "\u{f0721}",                                                  // 󰜡 SVG

        // Git
        "gitignore" | "gitattributes" | "gitmodules" => "\u{e702}", //  Git

        // Default
        _ => "\u{f15b}", //  Generic file
    }
}
