use capn_config::CapnConfig;
use facet::Facet;
use facet_styx::StyxFormat;
use figue::{self as args, Driver};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::process::Command;
use supports_color::{self, Stream as ColorStream};
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod checks;
mod commands;
mod jobs;
mod task;
mod utils;

// Embed schema for zero-execution discovery by Styx tooling
styx_embed::embed_outdir_file!("schema.styx");

pub use commands::{StagedFiles, debug_packages};
use commands::{run_clean, run_init, run_migrate, run_pre_commit, run_pre_push};

/// Git pre-commit and pre-push hooks for Rust projects.
#[derive(Facet, Debug)]
struct Args {
    /// Standard CLI options (--help, --version, --completions)
    #[facet(flatten)]
    builtins: args::FigueBuiltins,

    /// Command to run (defaults to pre-commit)
    #[facet(default, args::subcommand)]
    command: Option<Commands>,

    /// Configuration (from .config/capn/config.styx)
    #[facet(args::config)]
    config: CapnConfig,
}

/// Available commands
#[derive(Facet, Debug)]
#[repr(u8)]
enum Commands {
    /// Run pre-commit checks (default when no command specified)
    PreCommit,
    /// Run pre-push checks
    PrePush,
    /// Initialize capn hooks in the repository
    Init,
    /// Migrate legacy `.config/captain` to `.config/capn`
    Migrate,
    /// Debug package detection
    DebugPackages,
    /// Clean capn's shared target directory
    Clean,
}

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

fn main() {
    // Set up tracing with env filter (RUST_LOG)
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("capn=info"));
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(filter)
        .init();

    // Parse config from CLI args and config file using figue
    let figue_config = args::builder::<Args>()
        .expect("failed to create figue builder")
        .cli(|c| c.args(std::env::args().skip(1)))
        .file(|f| {
            f.format(StyxFormat)
                .default_paths([".config/capn/config.styx", ".config/captain/config.styx"])
        })
        .help(|h| {
            h.program_name("capn")
                .description("Git pre-commit and pre-push hooks for Rust projects")
                .version(env!("CARGO_PKG_VERSION"))
        })
        .build();

    let args: Args = Driver::new(figue_config).run().unwrap();

    // Dispatch to the appropriate command
    match args.command {
        Some(Commands::PrePush) => {
            run_pre_push(args.config);
        }
        Some(Commands::Init) => {
            run_init();
        }
        Some(Commands::Migrate) => {
            run_migrate();
        }
        Some(Commands::DebugPackages) => {
            debug_packages();
        }
        Some(Commands::Clean) => {
            run_clean();
        }
        Some(Commands::PreCommit) => {
            run_pre_commit(args.config);
        }
        None => {
            run_pre_commit(args.config);
        }
    }
}
