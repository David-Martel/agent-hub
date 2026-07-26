//! Embed best-effort source provenance for every runtime binary.

use std::process::Command;

fn main() {
    let describe = run_git(&["describe", "--tags", "--always", "--dirty", "--match", "v*"]);
    let short = run_git(&["rev-parse", "--short=12", "HEAD"]);
    let date = run_git(&["log", "-1", "--format=%cs"]);
    let explicit_revision = std::env::var("AGENT_BUS_BUILD_REVISION")
        .ok()
        .filter(|value| !value.trim().is_empty());

    let revision = match explicit_revision {
        Some(value) => with_date(value.trim(), date.as_deref()),
        None => match (describe, short) {
            (Some(value), _) | (_, Some(value)) if !value.is_empty() => {
                with_date(&value, date.as_deref())
            }
            _ => "unknown".to_owned(),
        },
    };
    println!("cargo:rustc-env=AGENT_BUS_CORE_GIT_VERSION={revision}");
    println!("cargo:rerun-if-env-changed=AGENT_BUS_BUILD_REVISION");

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

fn with_date(revision: &str, date: Option<&str>) -> String {
    match date {
        Some(value) if !value.is_empty() => format!("{revision} {value}"),
        _ => revision.to_owned(),
    }
}

fn run_git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}
