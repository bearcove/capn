//! Pre-commit and pre-push validation checks.
//!
//! These checks return errors instead of printing/exiting directly,
//! so callers can properly integrate with the spinner infrastructure.

use std::fmt::Write;

/// Error from a validation check, with formatted details for display.
pub struct CheckError {
    pub summary: String,
    pub details: String,
}

impl CheckError {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            details: String::new(),
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = details.into();
        self
    }
}

// Move from main.rs:
// - check_edition_2024 (lines 225-283) - change to return Result<(), CheckError>
// - check_external_path_deps (lines 289-330) - change to return Result<(), CheckError>

pub fn check_edition_2024(_metadata: &cargo_metadata::Metadata) -> Result<(), CheckError> {
    todo!("move check_edition_2024 here, return Result instead of exit")
}

pub fn check_external_path_deps(_metadata: &cargo_metadata::Metadata) -> Result<(), CheckError> {
    todo!("move check_external_path_deps here, return Result instead of exit")
}
