//! Captain configuration types.
//!
//! This is a separate crate so build.rs can generate schemas from these types.

/// Configuration read from `.config/captain/config.styx`
#[derive(Debug, Clone, facet::Facet)]
#[facet(derive(Default), traits(Default))]
#[facet(rename_all = "kebab-case")]
pub struct CaptainConfig {
    /// Configuration for pre-commit hooks.
    #[facet(default)]
    pub pre_commit: PreCommitConfig,

    /// Configuration for pre-push hooks.
    #[facet(default)]
    pub pre_push: PrePushConfig,
}

/// Configuration for pre-commit hooks.
#[derive(Debug, Clone, facet::Facet)]
#[facet(rename_all = "kebab-case", traits(Default), derive(Default))]
pub struct PreCommitConfig {
    /// Generate `README.md` files from `README.md.in` templates.
    #[facet(default = true)]
    pub generate_readmes: bool,

    /// Format staged Rust files with `rustfmt`.
    #[facet(default = true)]
    pub rustfmt: bool,

    /// Stage `Cargo.lock` changes automatically.
    #[facet(default = true)]
    pub cargo_lock: bool,

    /// Create `arborium-header.html` files for enhanced rustdoc syntax highlighting.
    #[facet(default = true)]
    pub arborium: bool,

    /// Require Rust edition 2024 in all workspace crates.
    #[facet(default = true)]
    pub edition_2024: bool,

    /// Check for path dependencies pointing outside the workspace.
    /// These are typically local development overrides that should not be committed.
    #[facet(default = true)]
    pub external_path_deps: bool,
}

/// Configuration for pre-push hooks.
#[derive(Debug, Clone, facet::Facet)]
#[facet(rename_all = "kebab-case", traits(Default), derive(Default))]
pub struct PrePushConfig {
    /// Run `cargo clippy` with warnings as errors.
    #[facet(default = true)]
    pub clippy: bool,

    /// Features to pass to clippy. If `None`, uses `--all-features`.
    #[facet(default)]
    pub clippy_features: Option<Vec<String>>,

    /// Run tests via `cargo nextest`.
    #[facet(default = true)]
    pub nextest: bool,

    /// Run documentation tests via `cargo test --doc`.
    #[facet(default = false)]
    pub doc_tests: bool,

    /// Features to pass to doc tests. If `None`, uses `--all-features`.
    #[facet(default)]
    pub doc_test_features: Option<Vec<String>>,

    /// Build documentation with `cargo doc` and treat warnings as errors.
    #[facet(default = true)]
    pub docs: bool,

    /// Features to pass to rustdoc. If `None`, uses `--all-features`.
    #[facet(default)]
    pub docs_features: Option<Vec<String>>,

    /// Check for unused dependencies with `cargo-shear`.
    #[facet(default = true)]
    pub cargo_shear: bool,
}
