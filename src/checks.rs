//! Pre-commit and pre-push validation checks.
//!
//! These checks return errors instead of printing/exiting directly,
//! so callers can properly integrate with the spinner infrastructure.

use owo_colors::OwoColorize;
use std::collections::HashSet;
use std::fs;
use toml_edit::{DocumentMut, Item};

/// Error from a validation check, with formatted details for display.
pub struct CheckError {
    pub summary: String,
    pub details: String,
}

impl CheckError {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            details: String::new(),
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = details.into();
        self
    }
}

/// Check that all workspace crates use edition 2024.
pub fn check_edition_2024(metadata: &cargo_metadata::Metadata) -> Result<(), CheckError> {
    let mut errors: Vec<String> = Vec::new();

    // Check workspace.package.edition in root Cargo.toml (if it exists)
    let workspace_root = &metadata.workspace_root;
    let root_cargo_toml = workspace_root.join("Cargo.toml");
    if root_cargo_toml.as_std_path().exists()
        && let Ok(content) = fs::read_to_string(root_cargo_toml.as_std_path())
        && let Ok(doc) = content.parse::<DocumentMut>()
        && let Some(workspace) = doc.get("workspace").and_then(Item::as_table)
        && let Some(package) = workspace.get("package").and_then(Item::as_table)
        && let Some(edition) = package.get("edition").and_then(Item::as_str)
        && edition != "2024"
    {
        errors.push(format!(
            "{}: [workspace.package].edition = {:?} (expected \"2024\")",
            root_cargo_toml, edition
        ));
    }

    // Get workspace members
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    // Check each workspace crate's edition
    for package in &metadata.packages {
        if !workspace_member_ids.contains(&package.id.repr) {
            continue;
        }

        let edition = &package.edition;
        if edition.as_str() != "2024" {
            errors.push(format!(
                "{}: edition = \"{}\" (expected \"2024\")",
                package.manifest_path,
                edition.as_str()
            ));
        }
    }

    if errors.is_empty() {
        return Ok(());
    }

    let summary = format!(
        "{}",
        "You have been deemed OUTDATED - edition 2024 now or bust".red()
    );

    let mut details = String::new();
    for err in &errors {
        details.push_str(&format!("  {} {}\n", "fix:".yellow(), err));
    }
    details.push_str("\nSet edition = \"2024\" in the above location(s) to proceed.");

    Err(CheckError::new(summary).with_details(details))
}

/// Check for path dependencies that point outside the workspace.
/// These are typically local development overrides that should not be committed.
pub fn check_external_path_deps(metadata: &cargo_metadata::Metadata) -> Result<(), CheckError> {
    let workspace_root = &metadata.workspace_root;

    let external_deps: Vec<_> = metadata
        .packages
        .iter()
        .filter(|pkg| {
            // source == None means it's a path dependency (not from crates.io or git)
            pkg.source.is_none()
        })
        .filter(|pkg| {
            // Check if manifest_path is outside workspace_root
            !pkg.manifest_path.starts_with(workspace_root)
        })
        .collect();

    if external_deps.is_empty() {
        return Ok(());
    }

    let summary = format!("{}", "External path dependencies detected!".red().bold());

    let mut details = String::new();
    details.push_str(&format!(
        "The following path dependencies point outside the workspace root ({}):\n\n",
        workspace_root
    ));
    for pkg in &external_deps {
        details.push_str(&format!(
            "  {} {} → {}\n",
            "✗".red(),
            pkg.name.as_str().yellow(),
            pkg.manifest_path.parent().unwrap_or(&pkg.manifest_path)
        ));
    }
    details.push_str(
        "\nThese are typically local development overrides (e.g., in [patch] sections)\n",
    );
    details
        .push_str("that should not be committed. They will break builds for other developers.\n\n");
    details.push_str("To fix: comment out or remove the path dependencies before committing.");

    Err(CheckError::new(summary).with_details(details))
}
