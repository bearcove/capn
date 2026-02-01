//! Arborium header and docs.rs metadata jobs.

use super::Job;
use log::error;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

// Move from main.rs:
// - ensure_docsrs_metadata (lines 187-222)

pub fn collect_arborium_jobs(metadata: &cargo_metadata::Metadata) -> Vec<Job> {
    let mut jobs = Vec::new();

    // Get workspace members
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    // Filter to get publishable workspace crates (excluding demos and test crates)
    let arborium_header = br#"<!-- Rustdoc doesn't highlight some languages natively -- let's do it ourselves: https://github.com/bearcove/arborium -->
<script defer src="https://cdn.jsdelivr.net/npm/@arborium/arborium@2/dist/arborium.iife.js"></script>"#;

    for package in &metadata.packages {
        // Only process workspace members
        if !workspace_member_ids.contains(&package.id.repr) {
            continue;
        }

        // Skip test/example crates based on common patterns
        if package.name.contains("test") || package.name.contains("example") {
            continue;
        }

        if let Some(manifest_dir) = package.manifest_path.parent() {
            let crate_dir: PathBuf = manifest_dir.into();
            let header_path = crate_dir.join("arborium-header.html");

            // Check if the file already exists with correct content
            let old_content = fs::read(&header_path).ok();
            let new_content = arborium_header.to_vec();

            // Only create a job if the file doesn't exist or content differs
            if old_content.as_ref() != Some(&new_content) {
                jobs.push(Job {
                    path: header_path,
                    old_content,
                    new_content,
                    #[cfg(unix)]
                    executable: false,
                });
            }

            // Also update Cargo.toml to add docsrs metadata if not present
            let cargo_toml_path = crate_dir.join("Cargo.toml");
            if cargo_toml_path.exists()
                && let Some(job) = rewrite_cargo_toml(&cargo_toml_path, ensure_docsrs_metadata)
            {
                jobs.push(job);
            }
        }
    }

    jobs
}

fn rewrite_cargo_toml<F>(cargo_toml_path: &Path, mut transform: F) -> Option<Job>
where
    F: FnMut(&mut DocumentMut) -> bool,
{
    let content = fs::read_to_string(cargo_toml_path).ok()?;
    let mut document: DocumentMut = match content.parse() {
        Ok(doc) => doc,
        Err(e) => {
            error!(
                "Failed to parse {} as TOML: {}",
                cargo_toml_path.display(),
                e
            );
            return None;
        }
    };

    if !transform(&mut document) {
        return None;
    }

    let new_content = document.to_string();
    if new_content == content {
        return None;
    }

    Some(Job {
        path: cargo_toml_path.to_path_buf(),
        old_content: Some(content.into_bytes()),
        new_content: new_content.into_bytes(),
        #[cfg(unix)]
        executable: false,
    })
}

fn ensure_table(item: &mut Item) -> &mut Table {
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("item to be a table")
}

fn array_matches(array: &Array, expected: &[&str]) -> bool {
    if array.len() != expected.len() {
        return false;
    }

    array
        .iter()
        .zip(expected.iter())
        .all(|(value, expected_value)| value.as_str() == Some(*expected_value))
}

fn ensure_docsrs_metadata(document: &mut DocumentMut) -> bool {
    let package_table = match document.get_mut("package").and_then(Item::as_table_mut) {
        Some(table) => table,
        None => return false,
    };

    let metadata_table = ensure_table(
        package_table
            .entry("metadata")
            .or_insert(Item::Table(Table::new())),
    );
    let docs_table = ensure_table(
        metadata_table
            .entry("docs.rs")
            .or_insert(Item::Table(Table::new())),
    );

    let desired = ["--html-in-header", "arborium-header.html"];
    let already_correct = match docs_table.get("rustdoc-args") {
        Some(item) => item
            .as_array()
            .map(|array| array_matches(array, &desired))
            .unwrap_or(false),
        None => false,
    };

    if already_correct {
        return false;
    }

    let mut args_array = Array::new();
    for arg in desired {
        args_array.push(Value::from(arg));
    }
    docs_table.insert("rustdoc-args", Item::Value(Value::Array(args_array)));
    true
}
