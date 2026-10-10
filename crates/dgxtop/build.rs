//! Embeds build provenance for `dgxtop --version --verbose`.
use std::path::Path;
use std::process::Command;

#[path = "src/provenance.rs"]
mod provenance;

/// Trimmed stdout of a successful `git` command.
fn git(arguments: &[&str]) -> Option<String> {
    Command::new("git")
        .args(arguments)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
}

fn main() {
    let commit = git(&["rev-parse", "HEAD"])
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    let dirty = git(&provenance::STATUS_ARGS).is_some_and(|status| provenance::is_dirty(&status));
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let rustc_version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned());
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    println!(
        "cargo:rustc-env=DGXTOP_BUILD_COMMIT={commit}{}",
        if dirty { "-dirty" } else { "" }
    );
    println!("cargo:rustc-env=DGXTOP_BUILD_RUSTC={rustc_version}");
    println!("cargo:rustc-env=DGXTOP_BUILD_TARGET={target}");
    println!("cargo:rustc-env=DGXTOP_BUILD_PROFILE={profile}");
    let git_dir = git(&["rev-parse", "--absolute-git-dir"]);
    let head_ref = git(&["symbolic-ref", "-q", "HEAD"]);
    let watched = provenance::rerun_paths(git_dir.as_deref(), head_ref.as_deref(), |path| {
        Path::new(path).exists()
    });
    for path in watched {
        println!("cargo:rerun-if-changed={path}");
    }
}
