//! Job system for file modifications that get staged to git.

mod arborium;
mod cargo_lock;
mod readme;
mod rustfmt;

pub use arborium::enqueue_arborium_jobs;
pub use cargo_lock::enqueue_cargo_lock_jobs;
pub use readme::enqueue_readme_jobs;
pub use rustfmt::enqueue_rustfmt_jobs;

use crate::command_with_color;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{fs, path::PathBuf};

#[derive(Debug, Clone)]
pub struct Job {
    pub path: PathBuf,
    pub old_content: Option<Vec<u8>>,
    pub new_content: Vec<u8>,
    #[cfg(unix)]
    pub executable: bool,
}

impl Job {
    pub fn is_noop(&self) -> bool {
        match &self.old_content {
            Some(old) => {
                if &self.new_content != old {
                    return false;
                }
                #[cfg(unix)]
                {
                    let current_executable = self
                        .path
                        .metadata()
                        .map(|m| m.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false);
                    current_executable == self.executable
                }
                #[cfg(not(unix))]
                {
                    true
                }
            }
            None => {
                #[cfg(unix)]
                {
                    self.new_content.is_empty() && !self.executable
                }
                #[cfg(not(unix))]
                {
                    self.new_content.is_empty()
                }
            }
        }
    }

    pub fn apply(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(&self.path, &self.new_content)?;

        #[cfg(unix)]
        if self.executable {
            let mut perms = fs::metadata(&self.path)?.permissions();
            perms.set_mode(perms.mode() | 0o111);
            fs::set_permissions(&self.path, perms)?;
        }

        let _ = command_with_color("git")
            .arg("add")
            .arg(&self.path)
            .status();
        Ok(())
    }
}
