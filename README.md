# captain

[![crates.io](https://img.shields.io/crates/v/captain.svg)](https://crates.io/crates/captain)
[![documentation](https://docs.rs/captain/badge.svg)](https://docs.rs/captain)
[![MIT/Apache-2.0 licensed](https://img.shields.io/crates/l/captain.svg)](./LICENSE)

**captain** is a comprehensive development automation tool for Rust workspaces.
It integrates seamlessly as a pre-commit hook and handles repetitive project setup,
code generation, and CI/CD configuration automatically.

> This project was originally forked from [facet-dev](https://github.com/facet-rs/facet-dev).

## Features

captain automates the following tasks:

- **README Generation**: Generates `README.md` files from `README.md.in` templates with customizable headers and footers
- **Code Formatting**: Runs `cargo fmt` to enforce consistent code style
- **Pre-push Verification**: Validates code before pushing (via `captain pre-push`):
  - Runs clippy for linting
  - Executes tests
  - Checks documentation compilation
  - Verifies all crates in workspace
- **MSRV Consistency**: Ensures all crates have the same Minimum Supported Rust Version
- **Rust Documentation**: Auto-generates rustdoc configuration

## Installation

### Quick Install (recommended)

On macOS and Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/bearcove/captain/releases/latest/download/captain-installer.sh | sh
```

On Windows (PowerShell):

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://github.com/bearcove/captain/releases/latest/download/captain-installer.ps1 | iex"
```

### From crates.io

```bash
cargo install captain
```

### From source

```bash
cargo install --git https://github.com/bearcove/captain
```

## Usage

### Quick Start

Initialize captain in your project:

```bash
captain init
```

This interactively creates:
- `hooks/` directory with pre-commit and pre-push hooks
- `hooks/install.sh` to install the hooks
- `conductor.json` for [Conductor](https://www.conductor.build/) setup (optional)
- `.config/captain/config.styx` configuration file
- `.config/captain/readme-templates/` with empty header/footer templates
- `README.md.in` template (if not present)

### Basic Generation

Run captain in your workspace root to generate/update all project files:

```bash
captain
```

This will:
1. Generate `README.md` for the workspace and all crates
2. Format code with `cargo fmt`
3. Stage all changes with `git add`

### Pre-push Checks

Before pushing to a remote repository, run:

```bash
captain pre-push
```

This performs comprehensive checks:
- Runs `cargo clippy` on all crates
- Executes `cargo test`
- Validates documentation with `cargo doc`
- Ensures MSRV consistency

### Custom Template Directory

Specify a custom directory for looking up `README.md.in` templates:

```bash
captain --template-dir /path/to/templates
```

This searches for `{crate_name}.md.in` files in the specified directory, falling back
to the crate's own template if not found.

### Debugging

View workspace metadata and package information:

```bash
captain debug-packages
```

## README Template Configuration

### Standard Template Format

Each crate should have a `README.md.in` file in its directory. The generated `README.md`
combines three parts:

1. **Header** (with badges and links)
2. **Main Content** (from `README.md.in`)
3. **Footer** (with license information)

### Header and Footer Templates

Captain looks for header and footer templates in `.config/captain/readme-templates/`:

```bash
.config/captain/readme-templates/
├── readme-header.md
└── readme-footer.md
```

These are prepended/appended to every generated `README.md`. If not present, no header or footer is added.

**Header Template Example** (`readme-header.md`):

```markdown
# {CRATE}

[![CI](https://github.com/your-org/{CRATE}/actions/workflows/ci.yml/badge.svg)](https://github.com/your-org/{CRATE}/actions)
[![crates.io](https://img.shields.io/crates/v/{CRATE}.svg)](https://crates.io/crates/{CRATE})
```

The `{CRATE}` placeholder will be replaced with the actual crate name.

**Footer Template Example** (`readme-footer.md`):

```markdown
## License

Licensed under the MIT License.
```

Run `captain init` to create empty templates that you can customize.

### Template Priority

For `README.md.in` templates (main content):

1. Custom directory specified via `--template-dir` (if provided)
2. Crate's own `README.md.in` file (in the crate directory)
3. Workspace-level `README.md.in` (for the workspace README)

## Git Hooks Setup

After running `captain init`, install the git hooks:

```bash
./hooks/install.sh
```

This installs hooks that:
- **pre-commit**: Runs `captain` to auto-generate and stage files
- **pre-push**: Runs `captain pre-push` for comprehensive validation

The hooks are installed to the main repo and all worktrees.

## Configuration

Captain is configured via `.config/captain/config.styx`:

```styx
@schema crate:captain@1

// Captain configuration
// All options default to true. Set to false to disable.

pre-commit {
  generate-readmes false
  rustfmt false
  cargo-lock false
}

pre-push {
  clippy false
  nextest false
  doc-tests false
  docs false
  cargo-shear false

  // Feature configuration
  clippy-features (feature1 feature2)
  doc-test-features (feature1)
  docs-features (feature1)
}
```

### Available Options

All options default to `true`. Set to `false` to disable.

#### Pre-commit Options (in `pre-commit { }` block)

| Option | Description |
|--------|-------------|
| `generate-readmes` | Generate `README.md` files from templates |
| `rustfmt` | Run `cargo fmt` to format code |
| `cargo-lock` | Stage `Cargo.lock` changes |
| `arborium` | Set up arborium syntax highlighting for docs |
| `rust-version` | Enforce consistent MSRV across crates |
| `edition-2024` | Require Rust edition 2024 |

#### Pre-push Options (in `pre-push { }` block)

| Option | Description |
|--------|-------------|
| `clippy` | Run `cargo clippy` with warnings as errors |
| `nextest` | Run tests via `cargo nextest` |
| `doc-tests` | Run documentation tests |
| `docs` | Build documentation with warnings as errors |
| `cargo-shear` | Check for unused dependencies |
| `clippy-features` | Features to pass to clippy (child node with args) |
| `doc-test-features` | Features to pass to doc tests (child node with args) |
| `docs-features` | Features to pass to rustdoc (child node with args) |

## Notes

### Automatic Staging

When captain runs, it automatically stages all generated files with `git add`.
If captain is updated mid-development, the new changes might appear in an unrelated PR.
This is by design—keep captain stable during active work.

### Workspace Requirements

Your project should:
- Be a Rust workspace or single crate with `Cargo.toml`
- Have git initialized
- Have `README.md.in` template files for any crates you want documented

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](./LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
