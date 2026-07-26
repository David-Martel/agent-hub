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
    let explicit_revision = std::env::var("AGENT_BUS_BUILD_REVISION")
        .ok()
        .filter(|value| !value.trim().is_empty());

    let version = match explicit_revision {
        Some(value) => with_date(value.trim(), date.as_deref()),
        None => match (describe, short) {
            (Some(d), _) if !d.is_empty() => with_date(&d, date.as_deref()),
            (_, Some(h)) if !h.is_empty() => with_date(&h, date.as_deref()),
            _ => "unknown".to_string(),
        },
    };
    println!("cargo:rustc-env=AGENT_BUS_GIT_VERSION={version}");
    println!("cargo:rerun-if-env-changed=AGENT_BUS_BUILD_REVISION");

    // A symbolic HEAD file does not change when its branch fast-forwards.
    // Watch the resolved ref and packed refs as well so cached release builds
    // cannot retain provenance from the previous deployed commit.
    if let Some(head) = run_git(&["rev-parse", "--git-path", "HEAD"]) {
        watch_if_present(&head);
    }
    if let Some(reference) = run_git(&["symbolic-ref", "-q", "HEAD"])
        && let Some(reference_path) = run_git(&["rev-parse", "--git-path", &reference])
    {
        watch_if_present(&reference_path);
    }
    if let Some(packed_refs) = run_git(&["rev-parse", "--git-path", "packed-refs"]) {
        watch_if_present(&packed_refs);
    }
    println!("cargo:rerun-if-changed=build.rs");
}

fn watch_if_present(path: &str) {
    let path = std::path::Path::new(path);
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
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
