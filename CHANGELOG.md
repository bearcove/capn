# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Pre-push checks now use each project's regular `target` directory instead
  of a shared `~/.capn/target` directory. `CARGO_TARGET_DIR` is still respected
  if set.
- Removed the `capn clean` subcommand — use `cargo clean` instead, which now
  operates on the same target directory the checks use.
- Removed the target directory size report from pre-push output. It existed only
  to surface growth of the hidden shared directory and is no longer relevant.
- Forked from [facet-dev](https://github.com/facet-rs/facet-dev) and renamed to capn
- Updated all configuration keys from `facet-dev` to `capn`
- Removed facet-specific branding and templates
- Made the tool more generic for use with any Rust workspace
