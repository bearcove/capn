//! Pre-commit and pre-push validation checks.
//!
//! These checks return errors instead of printing/exiting directly,
//! so callers can properly integrate with the spinner infrastructure.

use owo_colors::OwoColorize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
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

/// Check for internal workspace dev-dependencies that confuse release-plz.
///
/// release-plz struggles with internal dev-dependencies that inherit `workspace = true`
/// (which pulls in both path + version from workspace deps), or those that explicitly set
/// both `path` and `version` in dev-dependencies.
pub fn check_internal_dev_deps_release_plz(
    metadata: &cargo_metadata::Metadata,
) -> Result<(), CheckError> {
    let workspace_root = &metadata.workspace_root;
    let root_manifest = workspace_root.join("Cargo.toml");

    let root_content = fs::read_to_string(root_manifest.as_std_path()).map_err(|e| {
        CheckError::new(format!(
            "{}",
            "Failed to read workspace Cargo.toml".red().bold()
        ))
        .with_details(format!("{}: {e}", root_manifest))
    })?;
    let root_doc = root_content.parse::<DocumentMut>().map_err(|e| {
        CheckError::new(format!(
            "{}",
            "Failed to parse workspace Cargo.toml".red().bold()
        ))
        .with_details(format!("{}: {e}", root_manifest))
    })?;

    let internal_workspace_deps = workspace_internal_dependency_paths(&root_doc);
    if internal_workspace_deps.is_empty() {
        return Ok(());
    }

    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    let mut violations = Vec::new();

    for package in &metadata.packages {
        if !workspace_member_ids.contains(&package.id.repr) {
            continue;
        }

        let manifest_path = &package.manifest_path;
        let content = fs::read_to_string(manifest_path.as_std_path()).map_err(|e| {
            CheckError::new(format!(
                "{}",
                "Failed to read workspace member manifest".red().bold()
            ))
            .with_details(format!("{}: {e}", manifest_path))
        })?;
        let doc = content.parse::<DocumentMut>().map_err(|e| {
            CheckError::new(format!(
                "{}",
                "Failed to parse workspace member manifest".red().bold()
            ))
            .with_details(format!("{}: {e}", manifest_path))
        })?;

        if let Some(dev_deps) = doc.get("dev-dependencies").and_then(Item::as_table) {
            collect_dev_dep_violations(
                &mut violations,
                manifest_path.as_std_path(),
                "[dev-dependencies]",
                dev_deps,
                &internal_workspace_deps,
                workspace_root.as_std_path(),
            );
        }

        if let Some(targets) = doc.get("target").and_then(Item::as_table) {
            for (target_name, target_item) in targets {
                if let Some(target_table) = target_item.as_table()
                    && let Some(dev_deps) = target_table
                        .get("dev-dependencies")
                        .and_then(Item::as_table)
                {
                    let section = format!("[target.{target_name}.dev-dependencies]");
                    collect_dev_dep_violations(
                        &mut violations,
                        manifest_path.as_std_path(),
                        &section,
                        dev_deps,
                        &internal_workspace_deps,
                        workspace_root.as_std_path(),
                    );
                }
            }
        }
    }

    if violations.is_empty() {
        return Ok(());
    }

    let summary = format!(
        "{}",
        "Internal dev-dependencies incompatible with release-plz detected!"
            .red()
            .bold()
    );
    let mut details = String::new();
    details.push_str(
        "Found internal workspace crates in dev-dependencies using unsupported forms:\n\n",
    );
    for violation in violations {
        details.push_str(&format!("  {} {}\n", "✗".red(), violation));
    }
    details.push_str(
        "\nUse path-only entries for internal dev-dependencies. Avoid `workspace = true`\n",
    );
    details.push_str("and avoid `version` in dev-dependencies for internal workspace crates.");

    Err(CheckError::new(summary).with_details(details))
}

fn workspace_internal_dependency_paths(root_doc: &DocumentMut) -> HashMap<String, String> {
    let mut internal = HashMap::new();
    let Some(workspace) = root_doc.get("workspace").and_then(Item::as_table) else {
        return internal;
    };
    let Some(deps) = workspace.get("dependencies").and_then(Item::as_table) else {
        return internal;
    };

    for (dep_name, dep_item) in deps {
        if let Some(path) = item_string_field(dep_item, "path") {
            internal.insert(dep_name.to_owned(), path);
        }
    }

    internal
}

fn item_string_field(item: &Item, key: &str) -> Option<String> {
    if let Some(table) = item.as_table() {
        return table.get(key).and_then(Item::as_str).map(ToOwned::to_owned);
    }

    item.as_value()
        .and_then(|v| v.as_inline_table())
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
}

fn item_has_field(item: &Item, key: &str) -> bool {
    if let Some(table) = item.as_table() {
        return table.contains_key(key);
    }

    item.as_value()
        .and_then(|v| v.as_inline_table())
        .is_some_and(|t| t.contains_key(key))
}

fn item_bool_field(item: &Item, key: &str) -> Option<bool> {
    if let Some(table) = item.as_table() {
        return table.get(key).and_then(Item::as_bool);
    }

    item.as_value()
        .and_then(|v| v.as_inline_table())
        .and_then(|t| t.get(key))
        .and_then(|v| v.as_bool())
}

fn collect_dev_dep_violations(
    violations: &mut Vec<String>,
    manifest_path: &Path,
    section: &str,
    dev_deps: &toml_edit::Table,
    internal_workspace_deps: &HashMap<String, String>,
    workspace_root: &Path,
) {
    for (dep_name, dep_item) in dev_deps {
        let Some(workspace_dep_path) = internal_workspace_deps.get(dep_name) else {
            continue;
        };

        let has_workspace_true = item_bool_field(dep_item, "workspace").unwrap_or(false);
        let has_path_and_version =
            item_has_field(dep_item, "path") && item_has_field(dep_item, "version");
        if !has_workspace_true && !has_path_and_version {
            continue;
        }

        if has_workspace_true {
            let suggested_path =
                suggested_dev_dep_path(manifest_path, workspace_root, workspace_dep_path);
            violations.push(format!(
                "{} {}: `{dep_name}.workspace = true` is not allowed for internal dev-dependencies. Use `{dep_name} = {{ path = \"{suggested_path}\" }}`.",
                manifest_path.display(),
                section,
            ));
        }

        if has_path_and_version {
            violations.push(format!(
                "{} {}: `{dep_name}` sets both `path` and `version`. Remove `version` for internal dev-dependencies.",
                manifest_path.display(),
                section,
            ));
        }
    }
}

fn suggested_dev_dep_path(
    member_manifest_path: &Path,
    workspace_root: &Path,
    workspace_dep_path: &str,
) -> String {
    let member_dir = member_manifest_path
        .parent()
        .unwrap_or(member_manifest_path);
    let dep_abs = workspace_root.join(workspace_dep_path);

    let member_components: Vec<_> = member_dir.components().collect();
    let dep_components: Vec<_> = dep_abs.components().collect();

    let mut common_prefix_len = 0;
    while common_prefix_len < member_components.len()
        && common_prefix_len < dep_components.len()
        && member_components[common_prefix_len] == dep_components[common_prefix_len]
    {
        common_prefix_len += 1;
    }

    let mut parts: Vec<String> = Vec::new();
    for _ in common_prefix_len..member_components.len() {
        parts.push("..".to_owned());
    }
    for component in dep_components.iter().skip(common_prefix_len) {
        parts.push(component.as_os_str().to_string_lossy().to_string());
    }

    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

#[cfg(test)]
mod tests {
    use super::check_internal_dev_deps_release_plz;
    use std::path::Path;

    fn get_metadata_for_fixture(fixture_name: &str) -> cargo_metadata::Metadata {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let fixture_path = manifest_dir.join("tests/fixtures").join(fixture_name);

        cargo_metadata::MetadataCommand::new()
            .current_dir(&fixture_path)
            .exec()
            .unwrap_or_else(|e| {
                panic!("Failed to get metadata for fixture {}: {}", fixture_name, e)
            })
    }

    #[test]
    fn allows_internal_path_only_dev_dependency() {
        let metadata = get_metadata_for_fixture("workspace_with_internal_dev_dep_path_only");
        let result = check_internal_dev_deps_release_plz(&metadata);
        assert!(result.is_ok(), "{}", result.err().unwrap().details);
    }

    #[test]
    fn allows_external_workspace_dev_dependency() {
        let metadata = get_metadata_for_fixture("workspace_with_external_dev_dep_workspace");
        let result = check_internal_dev_deps_release_plz(&metadata);
        assert!(result.is_ok(), "{}", result.err().unwrap().details);
    }

    #[test]
    fn rejects_internal_workspace_true_dev_dependency() {
        let metadata = get_metadata_for_fixture("workspace_with_internal_dev_dep_workspace");
        let err = check_internal_dev_deps_release_plz(&metadata)
            .expect_err("internal workspace=true dev dependency should fail");

        assert!(
            err.details.contains("my-crate/Cargo.toml"),
            "{:#?}",
            err.details
        );
        assert!(
            err.details.contains("[dev-dependencies]"),
            "{:#?}",
            err.details
        );
        assert!(
            err.details.contains("my-internal.workspace = true"),
            "{:#?}",
            err.details
        );
    }

    #[test]
    fn rejects_internal_path_and_version_dev_dependency() {
        let metadata = get_metadata_for_fixture("workspace_with_internal_dev_dep_path_version");
        let err = check_internal_dev_deps_release_plz(&metadata)
            .expect_err("internal dev dependency with path+version should fail");

        assert!(
            err.details.contains("my-crate/Cargo.toml"),
            "{:#?}",
            err.details
        );
        assert!(
            err.details.contains("[dev-dependencies]"),
            "{:#?}",
            err.details
        );
        assert!(
            err.details.contains("sets both `path` and `version`"),
            "{:#?}",
            err.details
        );
    }

    #[test]
    fn rejects_internal_target_workspace_true_dev_dependency() {
        let metadata = get_metadata_for_fixture("workspace_with_internal_target_dev_dep_workspace");
        let err = check_internal_dev_deps_release_plz(&metadata)
            .expect_err("internal target-specific workspace=true dev dependency should fail");

        assert!(
            err.details.contains("my-crate/Cargo.toml"),
            "{:#?}",
            err.details
        );
        assert!(
            err.details.contains("[target.cfg(unix).dev-dependencies]"),
            "{:#?}",
            err.details
        );
    }
}
