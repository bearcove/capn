//! Configuration loading from .config/captain/config.styx

use captain_config::CaptainConfig;
use log::{debug, error, warn};
use std::fs;

// Move from main.rs:
// - ConfigFormat enum (lines 333-336)
// - strip_schema_directive (lines 338-344)
// - load_captain_config (lines 346-434)

pub fn load_captain_config() -> CaptainConfig {
    todo!("move load_captain_config here")
}
