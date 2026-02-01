use cargo_metadata::camino::Utf8PathBuf;
use std::path::Path;

/// Finds external path dependencies in a workspace.
/// Returns a list of (package_name, manifest_path) for packages that are path dependencies
/// located outside the workspace root.
fn find_external_path_deps(metadata: &cargo_metadata::Metadata) -> Vec<(String, Utf8PathBuf)> {
    let workspace_root = &metadata.workspace_root;

    metadata
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
        .map(|pkg| (pkg.name.to_string(), pkg.manifest_path.clone()))
        .collect()
}

fn get_metadata_for_fixture(fixture_name: &str) -> cargo_metadata::Metadata {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = manifest_dir.join("tests/fixtures").join(fixture_name);

    cargo_metadata::MetadataCommand::new()
        .current_dir(&fixture_path)
        .exec()
        .unwrap_or_else(|e| panic!("Failed to get metadata for fixture {}: {}", fixture_name, e))
}

#[test]
fn valid_workspace_has_no_external_deps() {
    let metadata = get_metadata_for_fixture("valid_workspace");
    let external = find_external_path_deps(&metadata);
    assert!(
        external.is_empty(),
        "Expected no external deps, found: {:?}",
        external
    );
}

#[test]
fn detects_external_path_dependency() {
    let metadata = get_metadata_for_fixture("workspace_with_external_dep");
    let external = find_external_path_deps(&metadata);

    assert_eq!(
        external.len(),
        1,
        "Expected 1 external dep, found: {:?}",
        external
    );
    assert_eq!(external[0].0, "external-crate");
    assert!(
        external[0].1.as_str().contains("external_crate"),
        "Expected path to contain 'external_crate', got: {}",
        external[0].1
    );
}
