use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("install") => install(),
        Some(cmd) => {
            eprintln!("Unknown command: {}", cmd);
            eprintln!("Available commands: install");
            std::process::exit(1);
        }
        None => {
            eprintln!("Usage: cargo xtask <command>");
            eprintln!("Available commands: install");
            std::process::exit(1);
        }
    }
}

fn install() {
    // Build release binaries
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "capn"])
        .status()
        .expect("Failed to run cargo build");

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    let home = std::env::var("HOME").expect("HOME not set");
    let capn_src = "target/release/capn";
    let capn_dst = format!("{}/.cargo/bin/capn", home);
    std::fs::copy(capn_src, &capn_dst).expect("Failed to copy capn binary");

    let captain_src = "target/release/captain";
    let captain_dst = format!("{}/.cargo/bin/captain", home);
    std::fs::copy(captain_src, &captain_dst).expect("Failed to copy captain binary");

    // On macOS, codesign the installed binary to avoid AMFI issues
    // (signing must happen AFTER copy, not before)
    #[cfg(target_os = "macos")]
    {
        println!("Signing installed binaries...");
        let status = Command::new("codesign")
            .args(["--sign", "-", "--force", &capn_dst])
            .status()
            .expect("Failed to run codesign for capn");
        if !status.success() {
            eprintln!("Warning: codesign failed for capn, continuing anyway");
        }

        let status = Command::new("codesign")
            .args(["--sign", "-", "--force", &captain_dst])
            .status()
            .expect("Failed to run codesign for captain");
        if !status.success() {
            eprintln!("Warning: codesign failed for captain, continuing anyway");
        }
    }

    println!("Installed capn to {}", capn_dst);
    println!("Installed captain (compat shim) to {}", captain_dst);
}
