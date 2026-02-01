use captain_config::CaptainConfig;
use facet::Facet;
use facet_styx::StyxFormat;
use figue::{self as args, Driver};
use log::{Level, LevelFilter, Log, Metadata, Record};
use owo_colors::{OwoColorize, Style};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use supports_color::{self, Stream as ColorStream};

mod checks;
mod commands;
mod jobs;
mod readme;
mod utils;

// Embed schema for zero-execution discovery by Styx tooling
styx_embed::embed_outdir_file!("schema.styx");

pub use commands::{StagedFiles, debug_packages};
use commands::{run_init, run_pre_commit, run_pre_push};

/// Git pre-commit and pre-push hooks for Rust projects.
#[derive(Facet, Debug)]
struct Args {
    /// Standard CLI options (--help, --version, --completions)
    #[facet(flatten)]
    builtins: args::FigueBuiltins,

    /// Command to run (defaults to pre-commit)
    #[facet(default, args::subcommand)]
    command: Option<Commands>,

    /// Configuration (from .config/captain/config.styx)
    #[facet(args::config)]
    config: CaptainConfig,
}

/// Available commands
#[derive(Facet, Debug)]
#[repr(u8)]
enum Commands {
    /// Run pre-commit checks (default when no command specified)
    PreCommit {
        /// Template directory for README generation
        #[facet(default, args::named)]
        template_dir: Option<PathBuf>,
    },
    /// Run pre-push checks
    PrePush,
    /// Initialize captain hooks in the repository
    Init,
    /// Debug package detection
    DebugPackages,
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
    setup_logger();

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

    // Parse config from CLI args and config file using figue
    let figue_config = args::builder::<Args>()
        .expect("failed to create figue builder")
        .cli(|c| c.args(std::env::args().skip(1)))
        .file(|f| {
            f.format(StyxFormat)
                .default_paths([".config/captain/config.styx"])
        })
        .help(|h| {
            h.program_name("captain")
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
        Some(Commands::DebugPackages) => {
            debug_packages();
        }
        Some(Commands::PreCommit { template_dir }) => {
            run_pre_commit(args.config, template_dir);
        }
        None => {
            run_pre_commit(args.config, None);
        }
    }
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
