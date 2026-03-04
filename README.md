# capn

[![crates.io](https://img.shields.io/crates/v/capn.svg)](https://crates.io/crates/capn)
[![documentation](https://docs.rs/capn/badge.svg)](https://docs.rs/capn)
[![MIT/Apache-2.0 licensed](https://img.shields.io/crates/l/capn.svg)](./LICENSE)

**capn** is a development automation tool for Rust workspaces.
It runs as pre-commit and pre-push hooks, handling code formatting
and comprehensive validation before you push.

> This project was originally forked from [facet-dev](https://github.com/facet-rs/facet-dev).

## Features

### Pre-commit (runs on every commit)

- **Code Formatting**: Formats staged Rust files with `rustfmt` (edition 2024)
- **Cargo.lock Staging**: Automatically stages lockfile changes
- **Arborium Setup**: Configures [arborium](https://github.com/bearcove/arborium) syntax highlighting for rustdoc
- **Edition 2024 Enforcement**: Ensures all crates use Rust edition 2024
- **External Path Deps Check**: Catches path dependencies pointing outside the workspace

### Pre-push (runs before pushing)

- **Clippy**: Runs `cargo clippy` with warnings as errors
- **Tests**: Runs tests via `cargo nextest` (only affected crates)
- **Doc Tests**: Runs documentation tests (disabled by default)
- **Documentation**: Builds docs with `cargo doc -D warnings`
- **Unused Dependencies**: Checks for unused deps with `cargo-shear`
- **Target Size**: Reports target directory size at the end

All checks run in parallel with live progress spinners. If any check fails, remaining
tasks are cancelled immediately.

## Installation

### Quick Install (recommended)

On macOS and Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bearcove/capn/releases/latest/download/capn-installer.sh | sh
```

On Windows (PowerShell):

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/bearcove/capn/releases/latest/download/capn-installer.ps1 | iex"
```

### From crates.io

```bash
cargo install capn
```

This installs both `capn` and a `captain` compatibility shim that forwards to `capn`.

### From source

```bash
cargo install --git https://github.com/bearcove/capn
```

## Quick Start

Initialize capn in your project:

```bash
capn init
```

This creates:
- `hooks/pre-commit` and `hooks/pre-push` scripts
- `hooks/install.sh` to install the hooks
- `conductor.json` for [Conductor](https://www.conductor.build/) integration
- `.config/capn/config.styx` configuration file

Then install the hooks:

```bash
./hooks/install.sh
```

## Usage

### Pre-commit (default command)

```bash
capn
```

Runs all pre-commit checks, formats code, and stages changes.

### Pre-push

```bash
capn pre-push
```

Runs clippy, tests, doc builds, and cargo-shear on affected crates.

### Skip slow checks (emergency escape hatch)

```bash
HAVE_MERCY=1 git push   # Skip tests, doc-tests, docs
HAVE_MERCY=2 git push   # Also skip clippy
HAVE_MERCY=3 git push   # Skip everything
```

### Debug workspace info

```bash
capn debug-packages
```

## Configuration

Capn uses [Styx](https://github.com/bearcove/styx) configuration at `.config/capn/config.styx`:

```styx
@schema {id crate:capn-config@1, cli capn}

pre-commit {
  // `generate-readmes` defaults to false and is deprecated/ignored.
  // Enable it only to get a reminder to use cargo-reedme.
  generate-readmes false
  rustfmt true
  cargo-lock true
  arborium true
  edition-2024 true
  external-path-deps true
  internal-dev-deps-release-plz true
}

pre-push {
  clippy true
  nextest true
  doc-tests false        // Disabled by default
  docs true
  cargo-shear true

  // Optional: specify features instead of --all-features
  // clippy-features (feature1 feature2)
  // doc-test-features (feature1)
  // docs-features (feature1)
}
```

If you still have a legacy `.config/captain/`, run `capn migrate` to move it.
When both exist, `.config/capn/` takes precedence.

### Pre-commit Options

| Option | Default | Description |
|--------|---------|-------------|
| `generate-readmes` | `false` | Deprecated/ignored. If enabled, capn recommends `cargo-reedme` |
| `rustfmt` | `true` | Format staged Rust files |
| `cargo-lock` | `true` | Stage `Cargo.lock` changes |
| `arborium` | `true` | Set up arborium syntax highlighting |
| `edition-2024` | `true` | Require Rust edition 2024 |
| `external-path-deps` | `true` | Check for external path dependencies |
| `internal-dev-deps-release-plz` | `true` | Forbid internal dev-deps with `workspace = true` or `path` + `version` |

### Pre-push Options

| Option | Default | Description |
|--------|---------|-------------|
| `clippy` | `true` | Run clippy with `-D warnings` |
| `nextest` | `true` | Run tests via cargo-nextest |
| `doc-tests` | `false` | Run documentation tests |
| `docs` | `true` | Build docs with `-D warnings` |
| `cargo-shear` | `true` | Check for unused dependencies |
| `clippy-features` | - | Features for clippy (omit for `--all-features`) |
| `doc-test-features` | - | Features for doc tests |
| `docs-features` | - | Features for rustdoc |

## README Generation

Capn no longer generates `README.md` files.
If you enable `pre-commit.generate-readmes = true`, capn prints a warning and recommends using `cargo-reedme` instead.

## Logging

Set `RUST_LOG` for debug output:

```bash
RUST_LOG=capn=debug capn
RUST_LOG=capn=trace capn  # Very verbose
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](./LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
