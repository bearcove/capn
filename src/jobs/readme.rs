//! README.md generation from templates.

use super::Job;
use crate::{StagedFiles, command_with_color, readme};
use owo_colors::OwoColorize;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{error, warn};

pub fn collect_readme_jobs(
    template_dir: Option<&Path>,
    staged_files: &StagedFiles,
    metadata: &cargo_metadata::Metadata,
) -> Vec<Job> {
    let mut jobs = Vec::new();
    let workspace_dir = std::env::current_dir().unwrap();

    // Collect crate directories from workspace root and crates/ subdirectory
    let mut crate_dirs: Vec<PathBuf> = Vec::new();

    // Scan workspace root
    if let Ok(entries) = fs_err::read_dir(&workspace_dir) {
        for entry in entries.flatten() {
            crate_dirs.push(entry.path());
        }
    }

    // Also scan crates/ subdirectory if it exists
    let crates_subdir = workspace_dir.join("crates");
    if crates_subdir.is_dir()
        && let Ok(entries) = fs_err::read_dir(&crates_subdir)
    {
        for entry in entries.flatten() {
            crate_dirs.push(entry.path());
        }
    }

    let template_name = "README.md.in";

    // Load custom header and footer from template directory
    let template_dirs = [workspace_dir.join(".config/captain/readme-templates")];

    let find_template = |filename: &str| -> Option<String> {
        for dir in &template_dirs {
            if dir.exists()
                && let Ok(content) = fs::read_to_string(dir.join(filename))
            {
                return Some(content);
            }
        }
        None
    };

    let custom_header = find_template("readme-header.md");
    let custom_footer = find_template("readme-footer.md");

    // Helper function to process a README template
    let process_readme_template = |template_path: &Path,
                                   output_dir: &Path,
                                   crate_name: &str|
     -> Option<Job> {
        if !template_path.exists() {
            error!(
                "🚫 {} Please add a README.md.in template here that describes what this crate is for:\n   {}",
                "Missing template!".red().bold(),
                template_path.display().yellow()
            );
            return None;
        }

        // Read the template file
        let template_input = match fs::read_to_string(template_path) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to read template {}: {e}", template_path.display());
                return None;
            }
        };

        let readme_content = readme::generate(readme::GenerateReadmeOpts {
            crate_name: crate_name.to_string(),
            input: template_input,
            header: custom_header.clone(),
            footer: custom_footer.clone(),
        });

        let readme_path = output_dir.join("README.md");

        // Check if this README is staged and would be modified
        if staged_files.clean.contains(&readme_path) {
            // Get the relative path for git commands (git show doesn't like absolute paths)
            let relative_path = readme_path
                .strip_prefix(&workspace_dir)
                .unwrap_or(&readme_path);

            // Get the staged content
            let staged_content = command_with_color("git")
                .args(["show", &format!(":{}", relative_path.display())])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| o.stdout);

            if let Some(staged) = staged_content {
                let new_content_bytes = readme_content.as_bytes();
                if staged != new_content_bytes {
                    // The staged version differs from what we would generate!
                    error!("");
                    error!("{}", "❌ GENERATED FILE CONFLICT DETECTED".red().bold());
                    error!("");
                    error!(
                        "You modified {} directly, but this file is auto-generated.",
                        readme_path.display().yellow()
                    );
                    error!("This pre-commit hook would overwrite your changes.");
                    error!("");
                    error!(
                        "{} Edit {} instead (the template source)",
                        "→".cyan(),
                        template_path.display().yellow()
                    );
                    error!("");
                    error!("{}", "To fix this:".cyan().bold());
                    error!("  1. Undo changes to the generated file:");
                    error!("     git restore --staged {}", readme_path.display());
                    error!("     git restore {}", readme_path.display());
                    error!("");
                    error!("  2. OR edit the template and regenerate:");
                    error!("     # Edit {}", template_path.display());
                    error!("     cargo run --release  # regenerate");
                    error!(
                        "     git add {}  # stage the generated file",
                        readme_path.display()
                    );
                    error!("");
                    error!("Refusing to commit until this conflict is resolved.");
                    std::process::exit(1);
                }
            }
        }

        let old_content = fs::read(&readme_path).ok();

        Some(Job {
            path: readme_path,
            old_content,
            new_content: readme_content.into_bytes(),
            #[cfg(unix)]
            executable: false,
        })
    };

    for crate_path in crate_dirs {
        if !crate_path.is_dir()
            || crate_path.file_name().is_some_and(|name| {
                name.to_string_lossy().starts_with('.') || name.to_string_lossy().starts_with('_')
            })
        {
            continue;
        }

        let dir_name = crate_path.file_name().unwrap().to_string_lossy();

        // Skip common non-publishable directories
        if matches!(
            dir_name.as_ref(),
            "target" | "xtask" | "examples" | "benches" | "tests" | "fuzz"
        ) {
            continue;
        }

        let cargo_toml_path = crate_path.join("Cargo.toml");
        if !cargo_toml_path.exists() {
            continue;
        }

        // Check if this crate has generate-readmes = false in its package metadata
        if crate_has_readme_disabled(&cargo_toml_path) {
            continue;
        }

        let crate_name = dir_name.to_string();

        // Check for custom template path (from --template-dir or config)
        let template_path = if let Some(custom_dir) = template_dir {
            let custom_path = custom_dir.join(&crate_name).with_extension("md.in");
            if custom_path.exists() {
                custom_path
            } else {
                // Fall back to crate's own template
                crate_path.join(template_name)
            }
        } else if crate_name == "captain" {
            Path::new(template_name).to_path_buf()
        } else {
            crate_path.join(template_name)
        };

        if let Some(job) = process_readme_template(&template_path, &crate_path, &crate_name) {
            jobs.push(job);
        }
    }

    // Also handle the workspace/top-level README, if there's a Cargo.toml
    let workspace_cargo_toml = workspace_dir.join("Cargo.toml");
    if !workspace_cargo_toml.exists() {
        // No top-level Cargo.toml, skip workspace README
        return jobs;
    }

    let workspace_template_path = workspace_dir.join(template_name);

    // Get workspace name from cargo metadata so we can use the declared default member
    let workspace_name = match workspace_name_from_metadata_object(metadata) {
        Ok(name) => name,
        Err(err) => {
            // Fallback to directory name if metadata parsing fails
            let fallback = workspace_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("workspace")
                .to_string();
            warn!(
                "Failed to determine workspace name via cargo metadata: {err}, falling back to '{fallback}'"
            );
            fallback
        }
    };

    if let Some(job) =
        process_readme_template(&workspace_template_path, &workspace_dir, &workspace_name)
    {
        jobs.push(job);
    }

    jobs
}

/// Get the workspace name from cargo metadata (the root package name or first default member)
pub fn workspace_name_from_metadata_object(
    metadata: &cargo_metadata::Metadata,
) -> Result<String, String> {
    // Convert metadata to JSON for easier traversal
    let metadata_json =
        serde_json::to_value(metadata).map_err(|e| format!("Failed to serialize metadata: {e}"))?;

    if let Some(root_id) = metadata_json
        .get("resolve")
        .and_then(|resolve| resolve.get("root"))
        .and_then(|root| root.as_str())
        && let Some(name) = package_name_by_id(&metadata_json, root_id)
    {
        return Ok(name.to_string());
    }

    if let Some(default_members) = metadata_json
        .get("workspace_default_members")
        .and_then(|members| members.as_array())
    {
        for member in default_members {
            if let Some(member_id) = member.as_str()
                && let Some(name) = package_name_by_id(&metadata_json, member_id)
            {
                return Ok(name.to_string());
            }
        }
    }

    let canonical_manifest = fs::canonicalize(metadata.workspace_root.join("Cargo.toml"))
        .map_err(|e| format!("Failed to canonicalize workspace manifest: {e}"))?;

    if let Some(packages) = metadata_json
        .get("packages")
        .and_then(|packages| packages.as_array())
    {
        for pkg in packages {
            if let (Some(name), Some(manifest_path_str)) = (
                pkg.get("name").and_then(|n| n.as_str()),
                pkg.get("manifest_path").and_then(|path| path.as_str()),
            ) && let Ok(pkg_manifest_path) = fs::canonicalize(manifest_path_str)
                && pkg_manifest_path == canonical_manifest
            {
                return Ok(name.to_string());
            }
        }
    }

    Err("Unable to match workspace manifest to any package".to_string())
}

fn package_name_by_id<'a>(metadata: &'a serde_json::Value, package_id: &str) -> Option<&'a str> {
    let packages = metadata.get("packages")?.as_array()?;
    for pkg in packages {
        let id = pkg.get("id")?.as_str()?;
        if id == package_id {
            return pkg.get("name")?.as_str();
        }
    }
    None
}

/// Check if a crate has `generate-readmes = false` in its `[package.metadata.captain]`
fn crate_has_readme_disabled(cargo_toml_path: &Path) -> bool {
    let content = match fs::read_to_string(cargo_toml_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let doc = match content.parse::<toml_edit::DocumentMut>() {
        Ok(d) => d,
        Err(_) => return false,
    };
    doc.get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("captain"))
        .and_then(|f| f.get("generate-readmes"))
        .and_then(|v| v.as_bool())
        == Some(false)
}
