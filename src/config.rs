//! Configuration loading from .config/captain/config.styx

use captain_config::CaptainConfig;
use log::{debug, error};
use std::fs;

pub fn load_captain_config() -> CaptainConfig {
    let captain_dir = std::env::current_dir().unwrap().join(".config/captain");
    let styx_path = captain_dir.join("config.styx");

    if !styx_path.exists() {
        debug!("No config file at {}, using defaults", styx_path.display());
        return CaptainConfig::default();
    }

    let content = match fs::read_to_string(&styx_path) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to read config file {}: {e}", styx_path.display());
            std::process::exit(1);
        }
    };

    // Handle empty file as defaults
    if content.trim().is_empty() {
        return CaptainConfig::default();
    }

    // Strip @schema directive (metadata for editors, not data)
    let content = strip_schema_directive(&content);

    match facet_styx::from_str(&content) {
        Ok(config) => config,
        Err(e) => {
            error!("Failed to parse {}:\n{e}", styx_path.display());
            std::process::exit(1);
        }
    }
}

/// Strip @schema directive from config content.
/// The @schema directive is metadata for editors/LSPs, not actual config data.
fn strip_schema_directive(content: &str) -> String {
    content
        .lines()
        .filter(|line| !line.trim_start().starts_with("@schema"))
        .collect::<Vec<_>>()
        .join("\n")
}
