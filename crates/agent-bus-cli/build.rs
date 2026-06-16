//! Build script: embed best-effort git metadata into the binary so
//! `agent-bus --version` reports the exact commit it was built from. This
//! enables cross-machine version/commit parity checks across the fleet.
//!
//! Fully optional: if `git` is unavailable or this is not a checkout (e.g. a
//! packaged crate or CI tarball), the embedded value falls back to `unknown`
//! and the build still succeeds. There is no runtime dependency on git.

use std::process::Command;

fn main() {
    let describe = run_git(&["describe", "--tags", "--always", "--dirty", "--match", "v*"]);
    let short = run_git(&["rev-parse", "--short=12", "HEAD"]);
    let date = run_git(&["log", "-1", "--format=%cs"]); // committer date, YYYY-MM-DD

    let version = match (describe, short) {
        (Some(d), _) if !d.is_empty() => with_date(&d, date.as_deref()),
        (_, Some(h)) if !h.is_empty() => with_date(&h, date.as_deref()),
        _ => "unknown".to_string(),
    };
    println!("cargo:rustc-env=AGENT_BUS_GIT_VERSION={version}");

    // Rebuild when the checked-out commit moves.
    if let Some(git_dir) = run_git(&["rev-parse", "--git-dir"]) {
        let head = std::path::Path::new(&git_dir).join("HEAD");
        if head.exists() {
            println!("cargo:rerun-if-changed={}", head.display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}

fn with_date(rev: &str, date: Option<&str>) -> String {
    match date {
        Some(d) if !d.is_empty() => format!("{rev} {d}"),
        _ => rev.to_string(),
    }
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}
