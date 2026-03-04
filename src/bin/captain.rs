use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::process::Command;

fn capn_path() -> PathBuf {
    #[cfg(windows)]
    let exe_name = "capn.exe";
    #[cfg(not(windows))]
    let exe_name = "capn";

    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join(exe_name)))
        .unwrap_or_else(|| PathBuf::from(exe_name))
}

fn main() {
    eprintln!("`captain` is deprecated. Forwarding to `capn`...");

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let status = match Command::new(capn_path()).args(&args).status() {
        Ok(status) => status,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            match Command::new("capn").args(&args).status() {
                Ok(status) => status,
                Err(path_err) if path_err.kind() == io::ErrorKind::NotFound => {
                    match Command::new("cargo")
                        .arg("run")
                        .arg("--quiet")
                        .arg("--bin")
                        .arg("capn")
                        .arg("--")
                        .args(&args)
                        .status()
                    {
                        Ok(status) => status,
                        Err(err) => {
                            eprintln!(
                                "Failed to execute `capn` (direct, PATH, cargo fallback): {err}"
                            );
                            std::process::exit(1);
                        }
                    }
                }
                Err(err) => {
                    eprintln!("Failed to execute `capn`: {err}");
                    std::process::exit(1);
                }
            }
        }
        Err(err) => {
            eprintln!("Failed to execute `capn`: {err}");
            std::process::exit(1);
        }
    };
    std::process::exit(status.code().unwrap_or(1));
}
