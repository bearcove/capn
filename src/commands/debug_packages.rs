//! Debug command to show workspace package info.

use owo_colors::OwoColorize;
use std::collections::HashSet;
use tracing::error;

pub fn debug_packages() {
    println!("{}", "Loading workspace metadata...".cyan().bold());

    let metadata = match cargo_metadata::MetadataCommand::new().exec() {
        Ok(m) => m,
        Err(e) => {
            let err_str = e.to_string();
            // No Cargo.toml in this directory - not a Rust project
            if err_str.contains("could not find") {
                println!("{}", "No Cargo.toml found, nothing to do".yellow());
                std::process::exit(0);
            }
            // Check if this is an empty virtual workspace (no members)
            if err_str.contains("virtual manifest")
                || err_str.contains("no members")
                || err_str.contains("workspace has no members")
            {
                println!(
                    "{}",
                    "No workspace members found (empty virtual workspace)".yellow()
                );
                std::process::exit(0);
            }
            error!("Failed to get workspace metadata: {}", e);
            std::process::exit(1);
        }
    };

    // If this is a virtual workspace with no members, show that info
    if metadata.workspace_members.is_empty() {
        println!(
            "{}",
            "No workspace members found (empty virtual workspace)".yellow()
        );
        std::process::exit(0);
    }

    println!("{}", "\n📦 Workspace Members:".cyan().bold());
    for member_id in &metadata.workspace_members {
        if let Some(package) = metadata.packages.iter().find(|p| &p.id == member_id) {
            println!(
                "  ✓ {} ({})",
                package.name,
                package.manifest_path.parent().unwrap()
            );
        }
    }

    // Get the set of excluded crate names (those that are packages but not workspace members)
    let workspace_member_ids: HashSet<_> = metadata
        .workspace_members
        .iter()
        .map(|id| &id.repr)
        .collect();

    let excluded: Vec<_> = metadata
        .packages
        .iter()
        .filter(|pkg| !workspace_member_ids.contains(&pkg.id.repr))
        .collect();

    if !excluded.is_empty() {
        println!("{}", "\n🚫 Excluded Packages:".yellow().bold());
        for package in excluded {
            println!(
                "  ✗ {} ({})",
                package.name,
                package.manifest_path.parent().unwrap()
            );
        }
    } else {
        println!("{}", "\n🚫 Excluded Packages: None".yellow().bold());
    }

    println!("\n✅ Total packages: {}", metadata.packages.len());
}
