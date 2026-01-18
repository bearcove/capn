use log::{Level, LevelFilter, Log, Metadata, Record, debug, error, warn};
use owo_colors::{OwoColorize, Style};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::{
    borrow::Cow,
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use supports_color::{self, Stream as ColorStream};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

mod readme;
mod utils;

use utils::{TaskProgress, dir_size, format_size, run_command_with_spinner};

fn terminal_supports_color(stream: ColorStream) -> bool {
    supports_color::on_cached(stream).is_some()
}

fn maybe_strip_bytes<'a>(data: &'a [u8], stream: ColorStream) -> Cow<'a, [u8]> {
    if terminal_supports_color(stream) {
        Cow::Borrowed(data)
    } else {
        Cow::Owned(strip_ansi_escapes::strip(data))
    }
}

fn apply_color_env(cmd: &mut Command) {
    cmd.env("FORCE_COLOR", "1");
    cmd.env("CARGO_TERM_COLOR", "always");
}

fn command_with_color<S: AsRef<OsStr>>(program: S) -> Command {
    let mut cmd = Command::new(program);
    apply_color_env(&mut cmd);
    cmd
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

#[derive(Debug, Clone)]
struct Job {
    path: PathBuf,
    old_content: Option<Vec<u8>>,
    new_content: Vec<u8>,
    #[cfg(unix)]
    executable: bool,
}

impl Job {
    fn is_noop(&self) -> bool {
        match &self.old_content {
            Some(old) => {
                if &self.new_content != old {
                    return false;
                }
                #[cfg(unix)]
                {
                    // Check if executable bit would change
                    let current_executable = self
                        .path
                        .metadata()
                        .map(|m| m.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false);
                    current_executable == self.executable
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
            None => {
                #[cfg(unix)]
                {
                    self.new_content.is_empty() && !self.executable
                }
                #[cfg(not(unix))]
                {
                    self.new_content.is_empty()
                }
            }
        }
    }

    /// Applies the job by writing out the new_content to path and staging the file.
    fn apply(&self) -> std::io::Result<()> {
        use std::fs;

        // Create parent directories if they don't exist
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&self.path, &self.new_content)?;

        // Set executable bit if needed
        #[cfg(unix)]
        if self.executable {
            let mut perms = fs::metadata(&self.path)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&self.path, perms)?;
        }

        // Now stage it, best effort
        let _ = command_with_color("git")
            .arg("add")
            .arg(&self.path)
            .status();
        Ok(())
    }
}

fn ensure_table(item: &mut Item) -> &mut Table {
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
    item.as_table_mut().expect("item to be a table")
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

/// Check that all workspace crates use edition 2024. Bails with an error if not.
fn check_edition_2024(metadata: &cargo_metadata::Metadata) {
    use std::collections::HashSet;

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

    if !errors.is_empty() {
        error!(
            "{}",
            "You have been deemed OUTDATED - edition 2024 now or bust".red()
        );
        error!("");
        for err in &errors {
            error!("  {} {}", "fix:".yellow(), err);
        }
        error!("");
        error!("Set edition = \"2024\" in the above location(s) to proceed.");
        std::process::exit(1);
    }
}

/// Configuration read from `.config/captain/config.styx`
#[derive(Debug, facet::Facet)]
#[facet(derive(Default), traits(Default))]
#[facet(rename_all = "kebab-case")]
struct CaptainConfig {
    #[facet(default)]
    pre_commit: PreCommitConfig,

    #[facet(default)]
    pre_push: PrePushConfig,
}

#[derive(Debug, facet::Facet)]
#[facet(rename_all = "kebab-case", traits(Default), derive(Default))]
struct PreCommitConfig {
    #[facet(default = true)]
    generate_readmes: bool,
    #[facet(default = true)]
    rustfmt: bool,
    #[facet(default = true)]
    cargo_lock: bool,
    #[facet(default = true)]
    arborium: bool,
    #[facet(default = true)]
    edition_2024: bool,
}

#[derive(Debug, facet::Facet)]
#[facet(rename_all = "kebab-case", traits(Default), derive(Default))]
struct PrePushConfig {
    #[facet(default = true)]
    clippy: bool,
    /// Features to use for clippy. If None, uses --all-features.
    #[facet(default)]
    clippy_features: Option<Vec<String>>,
    #[facet(default = true)]
    nextest: bool,
    #[facet(default = false)]
    doc_tests: bool,
    /// Features to use for doc tests. If None, uses --all-features.
    #[facet(default)]
    doc_test_features: Option<Vec<String>>,
    #[facet(default = true)]
    docs: bool,
    /// Features to use for docs. If None, uses --all-features.
    #[facet(default)]
    docs_features: Option<Vec<String>>,
    #[facet(default = true)]
    cargo_shear: bool,
}

enum ConfigFormat {
    Styx,
    Yaml,
}

fn load_captain_config() -> CaptainConfig {
    let captain_dir = std::env::current_dir().unwrap().join(".config/captain");
    let styx_path = captain_dir.join("config.styx");
    let yaml_path = captain_dir.join("config.yaml");

    // Check for old KDL config and error out
    let old_kdl_path = captain_dir.join("config.kdl");
    if old_kdl_path.exists() {
        error!(
            "Found old KDL config file at {}\n\
             Captain now uses Styx configuration.\n\
             Please migrate your config.kdl to config.styx and remove the .kdl file.\n\
             See the README for the Styx syntax.",
            old_kdl_path.display()
        );
        std::process::exit(1);
    }

    // Check for config.yml (we standardize on .yaml for legacy, .styx for new)
    let yml_path = captain_dir.join("config.yml");
    if yml_path.exists() {
        error!(
            "Found config.yml at {}\n\
             Captain uses config.styx (or config.yaml for legacy).\n\
             Please rename config.yml to config.styx.",
            yml_path.display()
        );
        std::process::exit(1);
    }

    // Error if both .styx and .yaml exist
    if styx_path.exists() && yaml_path.exists() {
        error!(
            "Found both config.styx and config.yaml in {}\n\
             Please remove config.yaml and keep only config.styx.",
            captain_dir.display()
        );
        std::process::exit(1);
    }

    // Determine which config file to use
    let (config_path, format) = if styx_path.exists() {
        (styx_path, ConfigFormat::Styx)
    } else if yaml_path.exists() {
        warn!(
            "Using deprecated config.yaml at {}\n\
             Please migrate to config.styx. The YAML format will be removed in a future version.",
            yaml_path.display()
        );
        (yaml_path, ConfigFormat::Yaml)
    } else {
        debug!("No config file at {}, using defaults", styx_path.display());
        return CaptainConfig::default();
    };

    let content = match fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to read config file {}: {e}", config_path.display());
            std::process::exit(1);
        }
    };

    // Handle empty file as defaults
    if content.trim().is_empty() {
        return CaptainConfig::default();
    }

    match format {
        ConfigFormat::Styx => match facet_styx::from_str(&content) {
            Ok(config) => config,
            Err(e) => {
                error!("Failed to parse {}:\n{e}", config_path.display());
                std::process::exit(1);
            }
        },
        ConfigFormat::Yaml => match facet_yaml::from_str(&content) {
            Ok(config) => config,
            Err(e) => {
                error!("Failed to parse {}:\n{e}", config_path.display());
                std::process::exit(1);
            }
        },
    }
}

fn enqueue_readme_jobs(
    sender: std::sync::mpsc::Sender<Job>,
    template_dir: Option<&Path>,
    staged_files: &StagedFiles,
    metadata: &cargo_metadata::Metadata,
) {
    let workspace_dir = std::env::current_dir().unwrap();
    let entries = match fs_err::read_dir(&workspace_dir) {
        Ok(e) => e,
        Err(e) => {
            error!("Failed to read workspace directory ({e})");
            return;
        }
    };

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
    let process_readme_template = |template_path: &Path, output_dir: &Path, crate_name: &str| {
        if !template_path.exists() {
            error!(
                "🚫 {} Please add a README.md.in template here that describes what this crate is for:\n   {}",
                "Missing template!".red().bold(),
                template_path.display().yellow()
            );
            return;
        }

        // Read the template file
        let template_input = match fs::read_to_string(template_path) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to read template {}: {e}", template_path.display());
                return;
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

        let job = Job {
            path: readme_path,
            old_content,
            new_content: readme_content.into_bytes(),
            #[cfg(unix)]
            executable: false,
        };

        if let Err(e) = sender.send(job) {
            error!("Failed to send job: {e}");
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                warn!("Skipping entry: {e}");
                continue;
            }
        };
        let crate_path = entry.path();

        if !crate_path.is_dir()
            || crate_path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with('.') || name.starts_with('_')
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

        process_readme_template(&template_path, &crate_path, &crate_name);
    }

    // Also handle the workspace/top-level README, if there's a Cargo.toml
    let workspace_cargo_toml = workspace_dir.join("Cargo.toml");
    if !workspace_cargo_toml.exists() {
        // No top-level Cargo.toml, skip workspace README
        return;
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

    process_readme_template(&workspace_template_path, &workspace_dir, &workspace_name);
}

/// Get the workspace name from cargo metadata (the root package name or first default member)
fn workspace_name_from_metadata_object(
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

fn enqueue_rustfmt_jobs(sender: std::sync::mpsc::Sender<Job>, staged_files: &StagedFiles) {
    use log::trace;
    use std::time::Instant;

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
        let job = Job {
            path: path.clone(),
            old_content: Some(original),
            new_content: formatted,
            #[cfg(unix)]
            executable: false,
        };
        if let Err(e) = sender.send(job) {
            error!("Failed to send rustfmt job for {}: {}", path.display(), e);
        }
    }
}

fn enqueue_cargo_lock_jobs(sender: std::sync::mpsc::Sender<Job>) {
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

                let job = Job {
                    path: lock_path.to_path_buf(),
                    old_content,
                    new_content: content,
                    #[cfg(unix)]
                    executable: false,
                };

                if let Err(e) = sender.send(job) {
                    error!("Failed to send Cargo.lock job: {e}");
                }
            }
        }
    }
}

fn enqueue_arborium_jobs(
    sender: std::sync::mpsc::Sender<Job>,
    metadata: &cargo_metadata::Metadata,
) {
    use std::collections::HashSet;

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
                let job = Job {
                    path: header_path,
                    old_content,
                    new_content,
                    #[cfg(unix)]
                    executable: false,
                };
                let _ = sender.send(job);
            }

            // Also update Cargo.toml to add docsrs metadata if not present
            let cargo_toml_path = crate_dir.join("Cargo.toml");
            if cargo_toml_path.exists()
                && let Some(job) = rewrite_cargo_toml(&cargo_toml_path, ensure_docsrs_metadata)
            {
                let _ = sender.send(job);
            }
        }
    }
}

fn shell_escape(part: &str) -> String {
    if part
        .chars()
        .all(|c| !c.is_whitespace() && c != '"' && c != '\'' && c != '\\')
    {
        part.to_string()
    } else {
        format!("{:?}", part)
    }
}

fn format_command_line(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| shell_escape(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn cargo_subcommand_missing_message(stderr: &str, subcommand: &str) -> bool {
    let stderr_lower = stderr.to_lowercase();
    let patterns = [
        format!("no such command: `{}`", subcommand),
        format!("no such command: '{}'", subcommand),
        format!("no such subcommand: `{}`", subcommand),
        format!("no such subcommand: '{}'", subcommand),
    ];
    patterns
        .iter()
        .any(|pattern| stderr_lower.contains(&pattern.to_lowercase()))
}

fn indicates_missing_cargo_subcommand(output: &std::process::Output, subcommand: &str) -> bool {
    cargo_subcommand_missing_message(&String::from_utf8_lossy(&output.stderr), subcommand)
}

fn print_clippy_fix_hint(command: &[String]) {
    let mut fix_command = Vec::with_capacity(command.len() + 2);
    let mut inserted = false;

    for part in command {
        if !inserted && part == "--" {
            fix_command.push("--allow-dirty".to_string());
            fix_command.push("--fix".to_string());
            inserted = true;
        }
        fix_command.push(part.clone());
    }

    if !inserted {
        fix_command.push("--allow-dirty".to_string());
        fix_command.push("--fix".to_string());
    }

    println!(
        "    {} Try auto-fixing with:\n        {}\n        git commit --amend --no-edit",
        "💡".cyan(),
        format_command_line(&fix_command)
    );
}

fn print_shear_fix_hint() {
    println!(
        "    {} Try cleaning unused dependencies with:\n        cargo shear --fix",
        "💡".cyan()
    );
}

fn print_stream(label: &str, data: &[u8], stream: ColorStream) {
    if data.is_empty() {
        println!("    {}: <empty>", label);
    } else {
        let cleaned = maybe_strip_bytes(data, stream);
        let text = String::from_utf8_lossy(&cleaned);
        println!("    {}:\n{}", label, text.trim_end());
    }
}

fn print_env_vars(envs: &[(&str, &str)]) {
    for (key, value) in envs {
        println!("    env: {}={}", key, value);
    }
}

fn exit_with_command_failure(
    command: &[String],
    envs: &[(&str, &str)],
    output: std::process::Output,
    hint: Option<Box<dyn FnOnce()>>,
) -> ! {
    println!("    command: {}", format_command_line(command));
    if !envs.is_empty() {
        print_env_vars(envs);
    }
    match output.status.code() {
        Some(code) => println!("    exit code: {}", code),
        None => println!("    exit code: terminated by signal"),
    }
    print_stream("stdout", &output.stdout, ColorStream::Stdout);
    print_stream("stderr", &output.stderr, ColorStream::Stderr);
    if let Some(action) = hint {
        action();
    }
    std::process::exit(1);
}

fn exit_with_command_error(
    command: &[String],
    envs: &[(&str, &str)],
    error: std::io::Error,
    hint: Option<Box<dyn FnOnce()>>,
) -> ! {
    println!("    command: {}", format_command_line(command));
    if !envs.is_empty() {
        print_env_vars(envs);
    }
    println!("    error: {}", error);
    if let Some(action) = hint {
        action();
    }
    std::process::exit(1);
}

fn should_skip_doc_tests(output: &std::process::Output) -> bool {
    if output.status.code() != Some(101) {
        return false;
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.contains("there is nothing to test")
        || stderr.contains("found no library targets to test")
        || stderr.contains("found no binaries to test")
        || stderr.contains("no library targets found")
}

fn debug_packages() {
    use std::collections::HashSet;

    println!("{}", "Loading workspace metadata...".cyan().bold());

    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => m,
        Err(e) => {
            let err_str = e.to_string();
            // No Cargo.toml in this directory - not a Rust project
            if err_str.contains("could not find") {
                println!("{}", "No Cargo.toml found, nothing to do".yellow());
                std::process::exit(0);
            }
            // Check if this is an empty virtual workspace (no members)
            if err_str.contains("virtual manifest")
                || err_str.contains("no members")
                || err_str.contains("workspace has no members")
            {
                println!(
                    "{}",
                    "No workspace members found (empty virtual workspace)".yellow()
                );
                std::process::exit(0);
            }
            error!("Failed to get workspace metadata: {}", e);
            std::process::exit(1);
        }
    };

    // If this is a virtual workspace with no members, show that info
    if metadata.workspace_members.is_empty() {
        println!(
            "{}",
            "No workspace members found (empty virtual workspace)".yellow()
        );
        std::process::exit(0);
    }

    println!("{}", "\n📦 Workspace Members:".cyan().bold());
    for member_id in &metadata.workspace_members {
        if let Some(package) = metadata.packages.iter().find(|p| &p.id == member_id) {
            println!(
                "  ✓ {} ({})",
                package.name,
                package.manifest_path.parent().unwrap()
            );
        }
    }

    // Get the set of excluded crate names (those that are packages but not workspace members)
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    let excluded: Vec<_> = metadata
        .packages
        .iter()
        .filter(|pkg| !workspace_member_ids.contains(&pkg.id.repr))
        .collect();

    if !excluded.is_empty() {
        println!("{}", "\n🚫 Excluded Packages:".yellow().bold());
        for package in excluded {
            println!(
                "  ✗ {} ({})",
                package.name,
                package.manifest_path.parent().unwrap()
            );
        }
    } else {
        println!("{}", "\n🚫 Excluded Packages: None".yellow().bold());
    }

    println!("\n✅ Total packages: {}", metadata.packages.len());
}

/// Get the shared target directory for pre-push checks (~/.captain/target)
fn get_shared_target_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".captain").join("target"))
}

fn run_pre_push() {
    use std::collections::{BTreeSet, HashSet};

    let mut config = load_captain_config();

    // HAVE_MERCY levels:
    // 1 (or just set) = skip slow checks (tests, doc tests, docs)
    // 2 = also skip clippy (just cargo-shear)
    // 3 = skip everything, just check formatting basically
    if let Ok(mercy) = std::env::var("HAVE_MERCY") {
        let level: u8 = mercy.parse().unwrap_or(1);
        let mut skipped = Vec::new();

        if level >= 1 {
            config.pre_push.nextest = false;
            config.pre_push.doc_tests = false;
            config.pre_push.docs = false;
            skipped.extend(["nextest", "doc-tests", "docs"]);
        }
        if level >= 2 {
            config.pre_push.clippy = false;
            skipped.push("clippy");
        }
        if level >= 3 {
            config.pre_push.cargo_shear = false;
            skipped.push("cargo-shear");
        }

        println!(
            "{}",
            format!("🙏 HAVE_MERCY={}: skipping {}", level, skipped.join(", "))
                .yellow()
                .bold()
        );
    }

    // Show what's disabled via config (if anything)
    let mut config_disabled = Vec::new();
    if !config.pre_push.clippy {
        config_disabled.push("clippy");
    }
    if !config.pre_push.nextest {
        config_disabled.push("nextest");
    }
    if !config.pre_push.doc_tests {
        config_disabled.push("doc-tests");
    }
    if !config.pre_push.docs {
        config_disabled.push("docs");
    }
    if !config.pre_push.cargo_shear {
        config_disabled.push("cargo-shear");
    }
    if !config_disabled.is_empty() && std::env::var("HAVE_MERCY").is_err() {
        println!(
            "{}",
            format!("⏭️  Disabled via config: {}", config_disabled.join(", ")).dimmed()
        );
    }

    // Set up shared target directory
    let shared_target_dir = get_shared_target_dir();

    // Use a channel so we can do non-blocking receive for dir_size
    let (dir_size_tx, dir_size_rx) = mpsc::channel::<(PathBuf, u64)>();
    if let Some(ref target_dir) = shared_target_dir {
        // Create the directory if it doesn't exist
        let _ = fs::create_dir_all(target_dir);

        // Set CARGO_TARGET_DIR for all subsequent cargo commands
        // SAFETY: We're single-threaded at this point, before spawning any cargo commands
        unsafe { std::env::set_var("CARGO_TARGET_DIR", target_dir) };

        // Calculate dir size in background (it's just informational)
        let target_dir_clone = target_dir.clone();
        std::thread::spawn(move || {
            let size = dir_size(&target_dir_clone);
            let _ = dir_size_tx.send((target_dir_clone, size));
        });
    }

    // Create progress tracker for setup phase - must be before any spawned tasks
    let progress = TaskProgress::new();

    // Spawn git fetch in background - we'll check the result at the end
    let fetch_spinner = progress.add_task("fetch");
    let fetch_start = std::time::Instant::now();
    let fetch_handle = std::thread::spawn(|| {
        Command::new("git")
            .args(["fetch", "origin", "main"])
            .output()
    });

    // Load workspace metadata
    let metadata_spinner = progress.add_task("metadata");
    let metadata_start = std::time::Instant::now();
    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => {
            metadata_spinner.succeed(metadata_start.elapsed().as_secs_f32());
            m
        }
        Err(e) => {
            metadata_spinner.fail(metadata_start.elapsed().as_secs_f32());
            let err_str = e.to_string();
            // No Cargo.toml in this directory - not a Rust project
            if err_str.contains("could not find") {
                println!(
                    "{}",
                    "No Cargo.toml found, skipping pre-push checks".yellow()
                );
                std::process::exit(0);
            }
            // Check if this is an empty virtual workspace (no members)
            if err_str.contains("virtual manifest")
                || err_str.contains("no members")
                || err_str.contains("workspace has no members")
            {
                println!(
                    "{}",
                    "No workspace members found, skipping pre-push checks".yellow()
                );
                std::process::exit(0);
            }
            error!("Failed to get workspace metadata: {}", e);
            std::process::exit(1);
        }
    };

    // If this is a virtual workspace with no members, skip checks
    if metadata.workspace_members.is_empty() {
        println!(
            "{}",
            "No workspace members found, skipping pre-push checks".yellow()
        );
        std::process::exit(0);
    }

    let workspace_root = metadata.workspace_root.clone().into_std_path_buf();

    // Type alias for background task results
    type CommandResult = (
        Vec<String>,
        Result<std::process::Output, std::io::Error>,
        Duration,
    );

    // Start workspace-wide checks immediately (don't wait for git fetch/diff)
    // These don't need to know which specific crates changed

    // 1. Run clippy on entire workspace
    let clippy_spinner = if config.pre_push.clippy {
        Some(progress.add_task("clippy"))
    } else {
        None
    };

    if let Some(ref spinner) = clippy_spinner {
        let start = std::time::Instant::now();
        let mut clippy_command = vec!["cargo".to_string(), "clippy".to_string()];
        clippy_command.push("--workspace".to_string());
        clippy_command.push("--all-targets".to_string());
        // Use configured features, or --all-features if not specified
        match &config.pre_push.clippy_features {
            None => {
                clippy_command.push("--all-features".to_string());
            }
            Some(features) if !features.is_empty() => {
                clippy_command.push("--features".to_string());
                clippy_command.push(features.join(","));
            }
            Some(_) => {
                // Empty features list means no extra features
            }
        }
        clippy_command.extend(vec![
            "--".to_string(),
            "-D".to_string(),
            "warnings".to_string(),
        ]);

        let clippy_output = run_command_with_spinner(&clippy_command, &[], spinner);
        let elapsed = start.elapsed();

        match clippy_output {
            Ok(output) if output.status.success() => {
                spinner.succeed(elapsed.as_secs_f32());
            }
            Ok(output) => {
                spinner.fail(elapsed.as_secs_f32());
                let hint_command = clippy_command.clone();
                exit_with_command_failure(
                    &clippy_command,
                    &[],
                    output,
                    Some(Box::new(move || print_clippy_fix_hint(&hint_command))),
                );
            }
            Err(e) => {
                spinner.fail(elapsed.as_secs_f32());
                let hint_command = clippy_command.clone();
                exit_with_command_error(
                    &clippy_command,
                    &[],
                    e,
                    Some(Box::new(move || print_clippy_fix_hint(&hint_command))),
                );
            }
        }
    }

    // 2. Spawn cargo-shear in background (completely independent)
    let shear_spinner = if config.pre_push.cargo_shear {
        Some(progress.add_task("cargo-shear"))
    } else {
        None
    };

    let shear_handle: Option<std::thread::JoinHandle<CommandResult>> =
        if config.pre_push.cargo_shear {
            let handle = std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let shear_command = vec!["cargo".to_string(), "shear".to_string()];
                let mut cmd = command_with_color(&shear_command[0]);
                for arg in &shear_command[1..] {
                    cmd.arg(arg);
                }
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());
                let output = cmd.output();
                let elapsed = start.elapsed();
                (shear_command, output, elapsed)
            });
            Some(handle)
        } else {
            None
        };

    // Get the set of workspace member crate IDs
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| id.repr.clone())
        .collect();

    // Get the set of excluded crate names (those that are packages but not workspace members)
    let excluded_crates: HashSet<String> = metadata
        .packages
        .iter()
        .filter(|pkg| !workspace_member_ids.contains(&pkg.id.repr))
        .map(|pkg| pkg.name.to_string())
        .collect();

    // Wait for git fetch to complete before checking changed files
    // This ensures origin/main is up-to-date for an accurate diff
    let fetch_result = fetch_handle.join();
    let fetch_elapsed = fetch_start.elapsed().as_secs_f32();
    let fetch_failed = match &fetch_result {
        Ok(Ok(output)) if !output.status.success() => {
            fetch_spinner.fail(fetch_elapsed);
            warn!(
                "Failed to fetch from origin: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            true
        }
        Ok(Err(e)) => {
            fetch_spinner.fail(fetch_elapsed);
            warn!("Failed to run git fetch: {}", e);
            true
        }
        Err(_) => {
            fetch_spinner.fail(fetch_elapsed);
            warn!("git fetch thread panicked");
            true
        }
        Ok(Ok(_)) => false,
    };

    // Get commit range info for display
    let origin_main_sha = Command::new("git")
        .args(["rev-parse", "--short", "origin/main"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "origin/main".to_string());

    let head_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "HEAD".to_string());

    let commit_count = Command::new("git")
        .args(["rev-list", "--count", "origin/main..HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok()
        })
        .unwrap_or(0);

    // Update fetch spinner with commit range info
    let commit_label = if commit_count == 1 {
        "commit"
    } else {
        "commits"
    };
    fetch_spinner.succeed_with_message(format!(
        "  {} {:<14} {} {}..{} ({} {})",
        "✓".green(),
        "fetch",
        format!("{:.1}s", fetch_elapsed).dimmed(),
        origin_main_sha,
        head_sha,
        commit_count,
        commit_label
    ));

    // Get the list of changed files between origin/main and HEAD
    let diff_spinner = progress.add_task("diff");
    let diff_start = std::time::Instant::now();
    let mut changed_files: std::collections::BTreeSet<String> = BTreeSet::new();

    let diff_output = command_with_color("git")
        .args(["diff", "--name-only", "origin/main", "HEAD"])
        .output();

    match diff_output {
        Ok(output) if output.status.success() => {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                changed_files.insert(line.to_string());
            }
            diff_spinner.succeed(diff_start.elapsed().as_secs_f32());
        }
        Err(e) => {
            diff_spinner.fail(diff_start.elapsed().as_secs_f32());
            error!("Failed to get changed files: {}", e);
            std::process::exit(1);
        }
        Ok(output) => {
            diff_spinner.fail(diff_start.elapsed().as_secs_f32());
            error!(
                "git diff failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::process::exit(1);
        }
    };

    let changed_files: Vec<_> = changed_files.into_iter().collect();

    if changed_files.is_empty() {
        println!("{}", "No changes detected".green().bold());
        std::process::exit(0);
    }

    // Build a map from directory to crate name using workspace packages
    let mut dir_to_crate: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for package in &metadata.packages {
        if let Some(parent) = package.manifest_path.parent() {
            dir_to_crate.insert(parent.to_string(), package.name.to_string());
        }
    }

    // Find which crates are affected and track which files triggered each
    let mut crate_to_files: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for file in &changed_files {
        let initial_path = Path::new(file);
        let mut current_path = if initial_path.is_absolute() {
            PathBuf::from(initial_path)
        } else {
            workspace_root.join(initial_path)
        };

        // Find the crate directory by walking up the path
        loop {
            let current_str = current_path.to_string_lossy().to_string();
            if let Some(crate_name) = dir_to_crate.get(&current_str) {
                crate_to_files
                    .entry(crate_name.clone())
                    .or_default()
                    .push(file.clone());
                break;
            }

            if !current_path.pop() {
                break;
            }
        }
    }

    if crate_to_files.is_empty() {
        println!("{}", "No crates affected by changes".yellow());
        std::process::exit(0);
    }

    // Filter affected crates to exclude those in the excluded list
    crate_to_files.retain(|crate_name, _| !excluded_crates.contains(crate_name));

    if crate_to_files.is_empty() {
        println!("{}", "No publishable crates affected by changes".yellow());
        std::process::exit(0);
    }

    // Sort for consistent output
    let affected_crates: BTreeSet<_> = crate_to_files.keys().cloned().collect();

    // Create spinners for crate-specific tasks
    let build_spinner = if config.pre_push.nextest {
        Some(progress.add_task("build tests"))
    } else {
        None
    };
    let test_spinner = if config.pre_push.nextest {
        Some(progress.add_task("run tests"))
    } else {
        None
    };
    let doctest_spinner = if config.pre_push.doc_tests {
        Some(progress.add_task("doc tests"))
    } else {
        None
    };
    let docs_spinner = if config.pre_push.docs {
        Some(progress.add_task("docs"))
    } else {
        None
    };

    // 1. Build nextest tests
    let test_handle: Option<std::thread::JoinHandle<CommandResult>> = if config.pre_push.nextest {
        let build_spinner = build_spinner.as_ref().unwrap();
        let start = std::time::Instant::now();
        let mut build_command = vec![
            "cargo".to_string(),
            "nextest".to_string(),
            "run".to_string(),
            "--no-run".to_string(),
        ];
        for crate_name in &affected_crates {
            build_command.push("-p".to_string());
            build_command.push(crate_name.to_string());
        }

        let build_output = run_command_with_spinner(&build_command, &[], build_spinner);
        let elapsed = start.elapsed();

        match build_output {
            Ok(output) if output.status.success() => {
                build_spinner.succeed(elapsed.as_secs_f32());
            }
            Ok(output) => {
                build_spinner.fail(elapsed.as_secs_f32());
                exit_with_command_failure(&build_command, &[], output, None);
            }
            Err(e) => {
                build_spinner.fail(elapsed.as_secs_f32());
                exit_with_command_error(&build_command, &[], e, None);
            }
        }

        // Spawn test runner in background
        let mut run_command = vec![
            "cargo".to_string(),
            "nextest".to_string(),
            "run".to_string(),
        ];
        for crate_name in &affected_crates {
            run_command.push("-p".to_string());
            run_command.push(crate_name.to_string());
        }
        run_command.push("--no-tests=pass".to_string());

        let handle = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut cmd = command_with_color(&run_command[0]);
            for arg in &run_command[1..] {
                cmd.arg(arg);
            }
            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());
            let output = cmd.output();
            let elapsed = start.elapsed();
            (run_command, output, elapsed)
        });
        Some(handle)
    } else {
        None
    };

    // 2. Spawn doc tests in background (independent of other tasks)
    let doctest_handle: Option<std::thread::JoinHandle<CommandResult>> =
        if doctest_spinner.is_some() {
            let affected_crates_clone = affected_crates.clone();
            let doc_test_features = config.pre_push.doc_test_features.clone();
            let handle = std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let mut doctest_command =
                    vec!["cargo".to_string(), "test".to_string(), "--doc".to_string()];
                for crate_name in &affected_crates_clone {
                    doctest_command.push("-p".to_string());
                    doctest_command.push(crate_name.to_string());
                }
                // Use configured features, or --all-features if not specified
                match &doc_test_features {
                    None => {
                        doctest_command.push("--all-features".to_string());
                    }
                    Some(features) if !features.is_empty() => {
                        doctest_command.push("--features".to_string());
                        doctest_command.push(features.join(","));
                    }
                    Some(_) => {
                        // Empty features list means no extra features
                    }
                }

                let mut cmd = command_with_color(&doctest_command[0]);
                for arg in &doctest_command[1..] {
                    cmd.arg(arg);
                }
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());
                let output = cmd.output();
                let elapsed = start.elapsed();
                (doctest_command, output, elapsed)
            });
            Some(handle)
        } else {
            None
        };

    // 3. Spawn docs build in background (independent of other tasks)
    let docs_handle: Option<std::thread::JoinHandle<CommandResult>> = if docs_spinner.is_some() {
        let affected_crates_clone = affected_crates.clone();
        let docs_features = config.pre_push.docs_features.clone();
        let handle = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut doc_command = vec![
                "cargo".to_string(),
                "doc".to_string(),
                "--no-deps".to_string(),
            ];
            for crate_name in &affected_crates_clone {
                doc_command.push("-p".to_string());
                doc_command.push(crate_name.to_string());
            }
            // Use configured features, or --all-features if not specified
            match &docs_features {
                None => {
                    doc_command.push("--all-features".to_string());
                }
                Some(features) if !features.is_empty() => {
                    doc_command.push("--features".to_string());
                    doc_command.push(features.join(","));
                }
                Some(_) => {
                    // Empty features list means no extra features
                }
            }
            let mut doc_cmd = command_with_color(&doc_command[0]);
            for arg in &doc_command[1..] {
                doc_cmd.arg(arg);
            }
            doc_cmd.env("RUSTDOCFLAGS", "-D warnings");
            doc_cmd.stdout(Stdio::piped());
            doc_cmd.stderr(Stdio::piped());
            let output = doc_cmd.output();
            let elapsed = start.elapsed();
            (doc_command, output, elapsed)
        });
        Some(handle)
    } else {
        None
    };

    // 4. Wait for cargo-shear background task
    if let Some(handle) = shear_handle {
        let spinner = shear_spinner.as_ref().unwrap();

        match handle.join() {
            Ok((shear_command, output_result, elapsed)) => match output_result {
                Ok(output) if output.status.success() => {
                    spinner.succeed(elapsed.as_secs_f32());
                }
                Ok(output) if indicates_missing_cargo_subcommand(&output, "shear") => {
                    spinner.skip("not installed");
                }
                Ok(output) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_failure(
                        &shear_command,
                        &[],
                        output,
                        Some(Box::new(print_shear_fix_hint)),
                    );
                }
                Err(e) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_error(&shear_command, &[], e, None);
                }
            },
            Err(_) => {
                spinner.fail(0.0);
                error!("cargo-shear thread panicked");
                std::process::exit(1);
            }
        }
    }

    // 5. Wait for doc tests
    if let Some(handle) = doctest_handle {
        let spinner = doctest_spinner.as_ref().unwrap();

        match handle.join() {
            Ok((doctest_command, output_result, elapsed)) => match output_result {
                Ok(output) if output.status.success() => {
                    spinner.succeed(elapsed.as_secs_f32());
                }
                Ok(output) if should_skip_doc_tests(&output) => {
                    // No lib to test - just hide this task
                    spinner.clear();
                }
                Ok(output) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_failure(&doctest_command, &[], output, None);
                }
                Err(e) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_error(&doctest_command, &[], e, None);
                }
            },
            Err(_) => {
                spinner.fail(0.0);
                error!("doc tests thread panicked");
                std::process::exit(1);
            }
        }
    }

    // 6. Wait for docs build
    if let Some(handle) = docs_handle {
        let spinner = docs_spinner.as_ref().unwrap();

        match handle.join() {
            Ok((doc_command, output_result, elapsed)) => match output_result {
                Ok(output) if output.status.success() => {
                    spinner.succeed(elapsed.as_secs_f32());
                }
                Ok(output) => {
                    spinner.fail(elapsed.as_secs_f32());
                    let doc_env = [("RUSTDOCFLAGS", "-D warnings")];
                    exit_with_command_failure(&doc_command, &doc_env, output, None);
                }
                Err(e) => {
                    spinner.fail(elapsed.as_secs_f32());
                    let doc_env = [("RUSTDOCFLAGS", "-D warnings")];
                    exit_with_command_error(&doc_command, &doc_env, e, None);
                }
            },
            Err(_) => {
                spinner.fail(0.0);
                error!("docs build thread panicked");
                std::process::exit(1);
            }
        }
    }

    // 7. Wait for test results
    if let Some(handle) = test_handle {
        let spinner = test_spinner.as_ref().unwrap();

        match handle.join() {
            Ok((run_command, output_result, elapsed)) => match output_result {
                Ok(output) if output.status.success() => {
                    spinner.succeed(elapsed.as_secs_f32());
                }
                Ok(output) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_failure(&run_command, &[], output, None);
                }
                Err(e) => {
                    spinner.fail(elapsed.as_secs_f32());
                    exit_with_command_error(&run_command, &[], e, None);
                }
            },
            Err(_) => {
                spinner.fail(0.0);
                error!("test runner thread panicked");
                std::process::exit(1);
            }
        }
    }

    println!();
    println!("{} {}", "✅".green(), "All checks passed!".green().bold());

    // Print affected crates summary
    println!();
    println!("{}", "Dirty crates:".cyan().bold());
    for crate_name in &affected_crates {
        if let Some(files) = crate_to_files.get(crate_name) {
            let file_list = if files.len() <= 3 {
                files.join(", ")
            } else {
                format!("{}, ... (+{} more)", files[..3].join(", "), files.len() - 3)
            };
            println!(
                "  {} {}",
                format!("{}:", crate_name).yellow(),
                file_list.dimmed()
            );
        }
    }

    // Print shared target dir size (non-blocking check)
    if shared_target_dir.is_some() {
        // Try to receive with a short timeout - don't block if still calculating
        if let Ok((target_dir, size)) = dir_size_rx.recv_timeout(Duration::from_millis(100)) {
            println!(
                "   {} {} ({})",
                "Target:".dimmed(),
                target_dir.display().to_string().blue(),
                format_size(size).dimmed()
            );
        }
    }

    // Check if fetch failed earlier (we already waited for it before diffing)
    print!("   {} ", "Branch:".dimmed());
    io::stdout().flush().unwrap();

    if fetch_failed {
        println!("{}", "fetch failed".red());
        std::process::exit(1);
    }

    // Check if current branch is fast-forward to origin/main
    let merge_base_output = command_with_color("git")
        .args(["merge-base", "HEAD", "origin/main"])
        .output();

    let merge_base = match merge_base_output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => {
            println!("{}", "failed".red());
            error!("Failed to find merge base with origin/main");
            std::process::exit(1);
        }
    };

    // Get origin/main rev
    let origin_main_rev = match command_with_color("git")
        .args(["rev-parse", "origin/main"])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => {
            println!("{}", "failed".red());
            error!("Failed to get origin/main revision");
            std::process::exit(1);
        }
    };

    // Check if origin/main is ahead of merge_base (meaning we need to rebase)
    if origin_main_rev != merge_base {
        println!("{}", "rebase needed".yellow());
        println!();
        println!(
            "{} {}",
            "⚠️".yellow(),
            "Your branch has diverged from origin/main".yellow().bold()
        );
        println!("  Please rebase your changes and push again:");
        println!("    {}", "git rebase origin/main".cyan());
        std::process::exit(1);
    }

    println!("{}", "up to date with origin/main".dimmed());
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

/// Initialize captain in the current repository
fn run_init() {
    println!("{}", "Captain initialization".cyan().bold());
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
captain
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
captain pre-push
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

    // 3. Create .config/captain/ directory with config.styx and templates
    println!();
    let captain_dir = workspace_dir.join(".config/captain");
    let config_path = captain_dir.join("config.styx");
    let templates_dir = captain_dir.join("readme-templates");

    if !captain_dir.exists() {
        if prompt_yes_no(
            "Create .config/captain/ with config.styx and readme templates?",
            true,
        ) {
            fs::create_dir_all(&templates_dir).expect("Failed to create captain config directory");

            // Create default config.styx
            let config_content = r#"@schema crate:captain@1

// Captain configuration
// All options default to true. Set to false to disable.

pre-commit {
  // generate-readmes false
  // rustfmt false
  // cargo-lock false
  // arborium false
  // edition-2024 false
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
            println!("  {} Created .config/captain/config.styx", "✔".green());

            // Create empty header/footer templates
            let header_path = templates_dir.join("readme-header.md");
            let footer_path = templates_dir.join("readme-footer.md");

            fs::write(&header_path, "").expect("Failed to write readme-header.md");
            fs::write(&footer_path, "").expect("Failed to write readme-footer.md");

            files_created.push(header_path);
            files_created.push(footer_path);

            println!(
                "  {} Created .config/captain/readme-templates/readme-header.md",
                "✔".green()
            );
            println!(
                "  {} Created .config/captain/readme-templates/readme-footer.md",
                "✔".green()
            );
        }
    } else {
        println!("  {} .config/captain/ already exists, skipping", "ℹ".blue());
    }

    // 4. Create README.md.in template
    println!();
    let readme_in_path = workspace_dir.join("README.md.in");
    if !readme_in_path.exists() {
        if prompt_yes_no("Create README.md.in template?", true) {
            // Try to get the package/workspace name
            let name = if let Ok(metadata) = cargo_metadata::MetadataCommand::new().exec() {
                workspace_name_from_metadata_object(&metadata).unwrap_or_else(|_| {
                    workspace_dir
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("my-project")
                        .to_string()
                })
            } else {
                workspace_dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("my-project")
                    .to_string()
            };

            let readme_content = format!(
                r#"# {name}

A Rust project.

## Features

- Feature 1
- Feature 2

## Installation

```bash
cargo install {name}
```

## Usage

```bash
{name} --help
```
"#
            );
            fs::write(&readme_in_path, readme_content).expect("Failed to write README.md.in");
            files_created.push(readme_in_path);

            println!("  {} Created README.md.in", "✔".green());
        }
    } else {
        println!("  {} README.md.in already exists, skipping", "ℹ".blue());
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
        println!("  2. Run {} to generate README.md", "captain".cyan());
        println!("  3. Commit the new files");
    }
}

fn show_and_apply_jobs(jobs: &mut [Job]) {
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

fn main() {
    setup_logger();

    miette::set_hook(Box::new(|_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .terminal_links(true)
                .unicode(true)
                .context_lines(3)
                .tab_width(4)
                .build(),
        )
    }))
    .expect("Failed to set miette hook");

    // Accept allowed log levels: trace, debug, error, warn, info
    log::set_max_level(LevelFilter::Info);
    if let Ok(log_level) = std::env::var("RUST_LOG") {
        let allowed = ["trace", "debug", "error", "warn", "info"];
        let log_level_lc = log_level.to_lowercase();
        if allowed.contains(&log_level_lc.as_str()) {
            let level = match log_level_lc.as_str() {
                "trace" => LevelFilter::Trace,
                "debug" => LevelFilter::Debug,
                "info" => LevelFilter::Info,
                "warn" => LevelFilter::Warn,
                "error" => LevelFilter::Error,
                _ => LevelFilter::Info,
            };
            log::set_max_level(level);
        }
    }

    // Parse CLI arguments
    let args: Vec<String> = std::env::args().collect();

    // Handle @dump-styx-schema for schema discovery
    if args.len() > 1 && args[1] == "@dump-styx-schema" {
        print!(
            "{}",
            styx_schema::generate::to_styx_schema::<CaptainConfig>()
        );
        return;
    }

    if args.len() > 1 && args[1] == "pre-push" {
        run_pre_push();
        return;
    }
    if args.len() > 1 && args[1] == "init" {
        run_init();
        return;
    }
    if args.len() > 1 && args[1] == "debug-packages" {
        debug_packages();
        return;
    }

    // Parse --template-dir argument
    let mut template_dir: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--template-dir" && i + 1 < args.len() {
            template_dir = Some(PathBuf::from(&args[i + 1]));
            i += 2;
        } else {
            i += 1;
        }
    }

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

    // Load captain config
    let config = load_captain_config();

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
    if config.pre_commit.edition_2024 {
        check_edition_2024(&metadata);
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
struct StagedFiles {
    /// Files that are staged (in the index) and not dirty (working tree matches index).
    clean: Vec<PathBuf>,
}

fn collect_staged_files() -> io::Result<StagedFiles> {
    let output = command_with_color("git")
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

struct SimpleLogger;

impl Log for SimpleLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        // Create style based on log level
        let level_style = match record.level() {
            Level::Error => Style::new().fg_rgb::<243, 139, 168>(), // Catppuccin red (Maroon)
            Level::Warn => Style::new().fg_rgb::<249, 226, 175>(),  // Catppuccin yellow (Peach)
            Level::Info => Style::new().fg_rgb::<166, 227, 161>(),  // Catppuccin green (Green)
            Level::Debug => Style::new().fg_rgb::<137, 180, 250>(), // Catppuccin blue (Blue)
            Level::Trace => Style::new().fg_rgb::<148, 226, 213>(), // Catppuccin teal (Teal)
        };

        // Convert level to styled display
        eprintln!(
            "{} - {}: {}",
            record.level().style(level_style),
            record
                .target()
                .style(Style::new().fg_rgb::<137, 180, 250>()), // Blue for the target
            record.args()
        );
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}

/// Set up a simple logger.
fn setup_logger() {
    let logger = Box::new(SimpleLogger);
    log::set_boxed_logger(logger).unwrap();
    log::set_max_level(LevelFilter::Trace);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_config(yaml: &str) -> CaptainConfig {
        facet_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn empty_config_uses_defaults() {
        // Empty YAML document (just empty object)
        let config: CaptainConfig = parse_config("{}");
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt);
        assert!(config.pre_commit.cargo_lock);
        assert!(config.pre_commit.arborium);
        assert!(config.pre_commit.edition_2024);
        assert!(config.pre_push.clippy);
        assert!(config.pre_push.nextest);
        assert!(!config.pre_push.doc_tests);
        assert!(config.pre_push.docs);
        assert!(config.pre_push.cargo_shear);
        assert!(config.pre_push.clippy_features.is_none());
        assert!(config.pre_push.doc_test_features.is_none());
        assert!(config.pre_push.docs_features.is_none());
    }

    #[test]
    fn empty_blocks_use_defaults() {
        let yaml = r#"
pre-commit: {}
pre-push: {}
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_push.clippy);
    }

    #[test]
    fn disable_pre_commit_options() {
        let yaml = r#"
pre-commit:
  generate-readmes: false
  rustfmt: false
  cargo-lock: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(!config.pre_commit.generate_readmes);
        assert!(!config.pre_commit.rustfmt);
        assert!(!config.pre_commit.cargo_lock);
        // Others still default to true
        assert!(config.pre_commit.arborium);
        assert!(config.pre_commit.edition_2024);
    }

    #[test]
    fn disable_pre_push_options() {
        let yaml = r#"
pre-push:
  clippy: false
  nextest: false
  doc-tests: false
  docs: false
  cargo-shear: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(!config.pre_push.clippy);
        assert!(!config.pre_push.nextest);
        assert!(!config.pre_push.doc_tests);
        assert!(!config.pre_push.docs);
        assert!(!config.pre_push.cargo_shear);
    }

    #[test]
    fn feature_lists() {
        let yaml = r#"
pre-push:
  clippy-features:
    - serde
    - async
  doc-test-features:
    - full
  docs-features:
    - all-features
    - experimental
"#;
        let config: CaptainConfig = parse_config(yaml);

        let clippy_features = config.pre_push.clippy_features.unwrap();
        assert_eq!(clippy_features, vec!["serde", "async"]);

        let doc_test_features = config.pre_push.doc_test_features.unwrap();
        assert_eq!(doc_test_features, vec!["full"]);

        let docs_features = config.pre_push.docs_features.unwrap();
        assert_eq!(docs_features, vec!["all-features", "experimental"]);
    }

    #[test]
    fn mixed_config() {
        let yaml = r#"
pre-commit:
  generate-readmes: false
  arborium: false
pre-push:
  nextest: false
  clippy-features:
    - serde
"#;
        let config: CaptainConfig = parse_config(yaml);

        assert!(!config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt); // default
        assert!(!config.pre_commit.arborium);

        assert!(config.pre_push.clippy); // default
        assert!(!config.pre_push.nextest);

        let clippy_features = config.pre_push.clippy_features.unwrap();
        assert_eq!(clippy_features, vec!["serde"]);
    }

    #[test]
    fn only_pre_commit_block() {
        let yaml = r#"
pre-commit:
  rustfmt: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(!config.pre_commit.rustfmt);
        // pre-push defaults
        assert!(config.pre_push.clippy);
        assert!(config.pre_push.nextest);
    }

    #[test]
    fn only_pre_push_block() {
        let yaml = r#"
pre-push:
  clippy: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        // pre-commit defaults
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt);
        // pre-push override
        assert!(!config.pre_push.clippy);
    }

    #[test]
    fn comment_only_blocks_use_defaults() {
        // This is what users get when they have a config with only comments under a key
        // YAML parses `pre-commit:` with only comments as null
        let yaml = r#"
# Captain configuration
# All options default to true unless noted. Set to false to disable.

pre-commit:
  # generate-readmes: false
  # rustfmt: false
  # cargo-lock: false
  # arborium: false
  # edition-2024: false

pre-push:
  # clippy: false
  # nextest: false
  # doc-tests: false
  # docs: false
  # cargo-shear: false
"#;
        let config: CaptainConfig = parse_config(yaml);
        // All defaults should apply
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt);
        assert!(config.pre_commit.cargo_lock);
        assert!(config.pre_commit.arborium);
        assert!(config.pre_commit.edition_2024);
        assert!(config.pre_push.clippy);
        assert!(config.pre_push.nextest);
        assert!(!config.pre_push.doc_tests);
        assert!(config.pre_push.docs);
        assert!(config.pre_push.cargo_shear);
    }

    #[test]
    fn top_level_comments_only() {
        // File with only comments should use all defaults
        let yaml = r#"
# Captain configuration
# This file is intentionally empty - all defaults apply
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(config.pre_commit.generate_readmes);
        assert!(config.pre_push.clippy);
    }

    #[test]
    fn mixed_comments_and_values() {
        let yaml = r#"
pre-commit:
  # Keep rustfmt enabled (default)
  generate-readmes: false
  # cargo-lock: false

pre-push:
  clippy: false
  # nextest stays enabled
  clippy-features:
    - serde
    # - disabled-feature
    - tokio
"#;
        let config: CaptainConfig = parse_config(yaml);
        assert!(!config.pre_commit.generate_readmes);
        assert!(config.pre_commit.rustfmt); // default
        assert!(config.pre_commit.cargo_lock); // default (commented out)
        assert!(!config.pre_push.clippy);
        assert!(config.pre_push.nextest); // default
        let features = config.pre_push.clippy_features.unwrap();
        assert_eq!(features, vec!["serde", "tokio"]);
    }
}
