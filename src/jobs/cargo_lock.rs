//! Cargo.lock staging jobs.

use super::Job;
use crate::command_with_color;
use std::fs;
use std::path::Path;

pub fn collect_cargo_lock_jobs() -> Vec<Job> {
    let lock_path = Path::new("Cargo.lock");

    // Check if Cargo.lock has unstaged changes
    let status_output = command_with_color("git")
        .args(["status", "--porcelain", "Cargo.lock"])
        .output();

    if let Ok(output) = status_output {
        let status = String::from_utf8_lossy(&output.stdout);

        // If there are unstaged changes (starts with space in second column, meaning modified in working tree)
        if status.contains(" M ") {
            // Stage the Cargo.lock changes
            if let Ok(content) = fs::read(lock_path) {
                let old_content = command_with_color("git")
                    .args(["show", "HEAD:Cargo.lock"])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| o.stdout);

                return vec![Job {
                    path: lock_path.to_path_buf(),
                    old_content,
                    new_content: content,
                    #[cfg(unix)]
                    executable: false,
                }];
            }
        }
    }

    Vec::new()
}
