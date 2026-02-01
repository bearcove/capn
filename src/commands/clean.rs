//! Clean command to remove captain's shared target directory.

use crate::utils::{TaskProgress, dir_size, format_size};
use owo_colors::OwoColorize;
use std::fs;
use std::io::{self, Write};

/// Run the clean command - removes captain's shared target directory
pub fn run_clean() {
    let target_dir = if let Some(home) = dirs::home_dir() {
        home.join(".captain").join("target")
    } else {
        eprintln!("{}", "Could not determine home directory".red());
        std::process::exit(1);
    };

    if !target_dir.exists() {
        println!(
            "{}",
            format!("Target directory does not exist: {}", target_dir.display()).dimmed()
        );
        return;
    }

    // Show spinner while computing size
    let progress = TaskProgress::new();
    let spinner = progress.add_task("calculating");
    let size = dir_size(&target_dir);
    let size_str = format_size(size);
    spinner.clear();

    println!("Captain's shared target directory:");
    println!(
        "  {} {}",
        target_dir.display().to_string().cyan(),
        size_str.yellow().bold()
    );
    println!();

    print!("Delete this directory? [y/N] ");
    io::stdout().flush().unwrap();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return;
    }

    let input = input.trim().to_lowercase();
    if input != "y" && input != "yes" {
        println!("{}", "Cancelled.".dimmed());
        return;
    }

    println!("Removing {}...", target_dir.display());

    match fs::remove_dir_all(&target_dir) {
        Ok(()) => {
            println!("{} Cleaned {} of build artifacts", "✓".green(), size_str);
        }
        Err(e) => {
            eprintln!("{} Failed to remove directory: {}", "✗".red(), e);
            std::process::exit(1);
        }
    }
}
