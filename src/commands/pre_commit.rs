//! Pre-commit hook implementation.

use std::path::PathBuf;

// Move from main.rs:
// - The pre-commit logic currently in main() (lines 2430-2570)

pub fn run_pre_commit(_template_dir: Option<PathBuf>) {
    todo!("move pre-commit logic here from main()")
}

fn show_and_apply_jobs(jobs: &mut [Job]) {
    jobs.sort_by_key(|job| job.path.clone());

    if jobs.is_empty() {
        println!("{}", "All generated files are up-to-date".green().bold());
        return;
    }

    // Apply all jobs first
    for job in jobs.iter() {
        if let Err(e) = job.apply() {
            eprintln!("Failed to apply {}: {e}", job.path.display());
            std::process::exit(1);
        }
    }

    // Print clean summary
    println!(
        "\n{}",
        "These files have been automatically formatted and staged:".green()
    );
    for job in jobs.iter() {
        let ext = job.path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let icon = icon_for_extension(ext);
        println!("  {} {}", icon.cyan(), job.path.display());
    }
    println!(
        "\n{}",
        "The commit is ready to push - no 'git amend' is necessary.".green()
    );
    std::process::exit(0);
}
/// Returns a Nerd Font icon for the given file extension
fn icon_for_extension(ext: &str) -> &'static str {
    match ext {
        // Languages
        "rs" => "\u{e7a8}",                         //  Rust
        "js" => "\u{e74e}",                         //  JavaScript
        "ts" => "\u{e628}",                         //  TypeScript
        "jsx" | "tsx" => "\u{e7ba}",                //  React
        "py" => "\u{e73c}",                         //  Python
        "rb" => "\u{e791}",                         //  Ruby
        "go" => "\u{e626}",                         //  Go
        "java" => "\u{e738}",                       //  Java
        "c" | "h" => "\u{e61e}",                    //  C
        "cpp" | "cc" | "cxx" | "hpp" => "\u{e61d}", //  C++
        "cs" => "\u{f031b}",                        // 󰌛 C#
        "swift" => "\u{e755}",                      //  Swift
        "kt" | "kts" => "\u{e634}",                 //  Kotlin
        "php" => "\u{e73d}",                        //  PHP
        "lua" => "\u{e620}",                        //  Lua
        "zig" => "\u{e6a9}",                        //  Zig
        "hs" => "\u{e777}",                         //  Haskell
        "ex" | "exs" => "\u{e62d}",                 //  Elixir
        "erl" => "\u{e7b1}",                        //  Erlang
        "scala" => "\u{e737}",                      //  Scala
        "clj" | "cljs" => "\u{e768}",               //  Clojure
        "r" => "\u{f07d4}",                         // 󰟔 R
        "jl" => "\u{e624}",                         //  Julia
        "pl" | "pm" => "\u{e769}",                  //  Perl
        "sh" | "bash" | "zsh" => "\u{e795}",        //  Shell
        "fish" => "\u{f489}",                       //  Fish
        "ps1" => "\u{e70f}",                        //  PowerShell
        "vim" => "\u{e62b}",                        //  Vim
        "el" => "\u{e779}",                         //  Emacs Lisp

        // Web
        "html" | "htm" => "\u{e736}",  //  HTML
        "css" => "\u{e749}",           //  CSS
        "scss" | "sass" => "\u{e74b}", //  Sass
        "less" => "\u{e758}",          //  Less
        "vue" => "\u{e6a0}",           //  Vue
        "svelte" => "\u{e697}",        //  Svelte
        "astro" => "\u{e6b3}",         //  Astro
        "wasm" => "\u{e6a1}",          //  WebAssembly

        // Data/Config
        "json" => "\u{e60b}",            //  JSON
        "yaml" | "yml" => "\u{e6a8}",    //  YAML
        "toml" => "\u{e6b2}",            //  TOML
        "xml" => "\u{f05c0}",            // 󰗀 XML
        "csv" => "\u{f0219}",            // 󰈙 CSV
        "sql" => "\u{e706}",             //  SQL
        "graphql" | "gql" => "\u{e662}", //  GraphQL
        "proto" => "\u{e6a5}",           //  Protobuf

        // Documentation
        "md" | "markdown" => "\u{e73e}", //  Markdown
        "txt" => "\u{f0219}",            // 󰈙 Text
        "pdf" => "\u{f0226}",            // 󰈦 PDF
        "doc" | "docx" => "\u{f0219}",   // 󰈙 Word
        "rst" => "\u{f0219}",            // 󰈙 reStructuredText

        // Build/Package
        "lock" => "\u{f023}",       //  Lock file
        "dockerfile" => "\u{e7b0}", //  Docker
        "nix" => "\u{f313}",        //  Nix
        "cmake" => "\u{e615}",      //  CMake

        // Images
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "webp" => "\u{f03e}", //  Image
        "svg" => "\u{f0721}",                                                  // 󰜡 SVG

        // Git
        "gitignore" | "gitattributes" | "gitmodules" => "\u{e702}", //  Git

        // Default
        _ => "\u{f15b}", //  Generic file
    }
}
