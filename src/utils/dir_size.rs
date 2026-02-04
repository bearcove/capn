use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use walkdir::WalkDir;

/// Calculate directory size recursively (returns bytes).
pub fn dir_size(path: &Path) -> u64 {
    dir_size_with_cancel(path, &AtomicBool::new(false))
}

/// Calculate directory size recursively with cancellation support (returns bytes).
///
/// Checks the `cancelled` flag during traversal and returns the partial sum
/// accumulated so far if cancellation is requested.
pub fn dir_size_with_cancel(path: &Path, cancelled: &AtomicBool) -> u64 {
    if !path.exists() {
        return 0;
    }

    let mut total: u64 = 0;
    for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        if entry.file_type().is_file()
            && let Ok(meta) = entry.metadata()
        {
            total += meta.len();
        }
    }
    total
}

/// Format bytes as human-readable size
pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
