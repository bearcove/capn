//! Captain configuration types.
//!
//! This is a separate crate so build.rs can generate schemas from these types.

/// Configuration read from `.config/captain/config.styx`
#[derive(Debug, facet::Facet)]
#[facet(derive(Default), traits(Default))]
#[facet(rename_all = "kebab-case")]
pub struct CaptainConfig {
    #[facet(default)]
    pub pre_commit: PreCommitConfig,

    #[facet(default)]
    pub pre_push: PrePushConfig,
}

#[derive(Debug, facet::Facet)]
#[facet(rename_all = "kebab-case", traits(Default), derive(Default))]
pub struct PreCommitConfig {
    #[facet(default = true)]
    pub generate_readmes: bool,
    #[facet(default = true)]
    pub rustfmt: bool,
    #[facet(default = true)]
    pub cargo_lock: bool,
    #[facet(default = true)]
    pub arborium: bool,
    #[facet(default = true)]
    pub edition_2024: bool,
}

#[derive(Debug, facet::Facet)]
#[facet(rename_all = "kebab-case", traits(Default), derive(Default))]
pub struct PrePushConfig {
    #[facet(default = true)]
    pub clippy: bool,
    /// Features to use for clippy. If None, uses --all-features.
    #[facet(default)]
    pub clippy_features: Option<Vec<String>>,
    #[facet(default = true)]
    pub nextest: bool,
    #[facet(default = false)]
    pub doc_tests: bool,
    /// Features to use for doc tests. If None, uses --all-features.
    #[facet(default)]
    pub doc_test_features: Option<Vec<String>>,
    #[facet(default = true)]
    pub docs: bool,
    /// Features to use for docs. If None, uses --all-features.
    #[facet(default)]
    pub docs_features: Option<Vec<String>>,
    #[facet(default = true)]
    pub cargo_shear: bool,
}
