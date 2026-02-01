//! Rustfmt formatting jobs.

use super::Job;
use crate::{StagedFiles, command_with_color, maybe_strip_bytes};
use log::{debug, error};
use owo_colors::OwoColorize;
use std::fs;
use std::io::Write;
use std::process::Stdio;
use supports_color::Stream as ColorStream;

pub fn collect_rustfmt_jobs(staged_files: &StagedFiles) -> Vec<Job> {
    use log::trace;
    use std::time::Instant;

    let mut jobs = Vec::new();

    for path in &staged_files.clean {
        // Only process .rs files
        if let Some(ext) = path.extension() {
            if ext != "rs" {
                continue;
            }
        } else {
            continue;
        }

        trace!("rustfmt: formatting {}", path.display());

        let original = match fs::read(path) {
            Ok(val) => val,
            Err(e) => {
                error!(
                    "{} {}: {}",
                    "❌".red(),
                    path.display().to_string().blue(),
                    format_args!("Failed to read: {e}").dimmed()
                );
                continue;
            }
        };

        let size_mb = (original.len() as f64) / (1024.0 * 1024.0);

        // Format the content via rustfmt (edition 2024)
        let start = Instant::now();
        let cmd = command_with_color("rustfmt")
            .arg("--edition")
            .arg("2024")
            .arg("--emit")
            .arg("stdout")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();

        let mut cmd = match cmd {
            Ok(child) => child,
            Err(e) => {
                error!("Failed to spawn rustfmt for {}: {}", path.display(), e);
                continue;
            }
        };

        // Write source to rustfmt's stdin
        {
            let mut stdin = cmd.stdin.take().expect("Failed to take rustfmt stdin");
            if stdin.write_all(&original).is_err() {
                error!(
                    "{} {}: {}",
                    "❌".red(),
                    path.display().to_string().blue(),
                    "Failed to write src to rustfmt".dimmed()
                );
                continue;
            }
        }

        let output = match cmd.wait_with_output() {
            Ok(out) => out,
            Err(e) => {
                error!("Failed to get rustfmt output for {}: {}", path.display(), e);
                continue;
            }
        };

        let duration = start.elapsed();
        let secs = duration.as_secs_f64();
        let mbps = if secs > 0.0 { size_mb / secs } else { 0.0 };
        debug!(
            "rustfmt: {} formatted {:.2} MiB in {:.2} s ({:.2} MiB/s)",
            path.display(),
            size_mb,
            secs,
            mbps.magenta()
        );

        if !output.status.success() {
            let stderr_clean = maybe_strip_bytes(&output.stderr, ColorStream::Stderr);
            let stdout_clean = maybe_strip_bytes(&output.stdout, ColorStream::Stdout);
            error!(
                "{} {}: rustfmt failed\n{}\n{}",
                "❌".red(),
                path.display().to_string().blue(),
                String::from_utf8_lossy(&stderr_clean).dimmed(),
                String::from_utf8_lossy(&stdout_clean).dimmed()
            );
            continue;
        }

        let formatted = output.stdout;
        jobs.push(Job {
            path: path.clone(),
            old_content: Some(original),
            new_content: formatted,
            #[cfg(unix)]
            executable: false,
        });
    }

    jobs
}
