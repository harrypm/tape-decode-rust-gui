use std::env;
use std::path::Path;
use std::process::Command;

// Embed the build's git branch and commit into the binary so the decode JSON
// sidecar (`videoParameters.gitBranch` / `gitCommit`) carries a clear
// provenance chain. Falls back to "UNKNOWN" when git is missing or this is not
// a git checkout (e.g. a tarball build).
fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"]);
    let commit = git_output(&["rev-parse", "HEAD"]);
    let release = git_release();
    println!("cargo:rustc-env=GIT_BRANCH={branch}");
    println!("cargo:rustc-env=GIT_COMMIT={commit}");
    println!("cargo:rustc-env=GIT_RELEASE={release}");

    // Re-run when the git state changes so the embedded branch/commit/release
    // track new commits without a clean rebuild. `.git` lives at the workspace
    // root, two levels above this package's manifest dir.
    if let Ok(manifest) = env::var("CARGO_MANIFEST_DIR") {
        let git_head = Path::new(&manifest).join("../../.git/HEAD");
        if git_head.exists() {
            println!("cargo:rerun-if-changed={}", git_head.display());
        }
    }
}

fn git_output(args: &[&str]) -> String {
    match Command::new("git").args(args).output() {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => "UNKNOWN".to_string(),
    }
}

/// Resolve the release tag version, mirroring `scripts/ci/git-version.sh` so the
/// embedded `gitRelease` matches the release artifact version: honor the
/// `DECODE_LIGHT_VERSION(_OVERRIDE)` env vars, else `git describe --tags --dirty`
/// against the project tag patterns, else a `dev-<sha>[-dirty]` fallback. The
/// `decode-light-` / `decode-rust-gui-` tag prefixes are stripped.
fn git_release() -> String {
    for key in &["DECODE_LIGHT_VERSION_OVERRIDE", "DECODE_LIGHT_VERSION"] {
        if let Ok(value) = env::var(key) {
            let value = value.trim();
            if !value.is_empty() {
                return strip_tag_prefix(value);
            }
        }
    }
    let described = Command::new("git")
        .args([
            "describe",
            "--tags",
            "--dirty",
            "--match",
            "v*",
            "--match",
            "decode-light-*",
            "--match",
            "decode-rust-gui-*",
        ])
        .output();
    if let Ok(out) = described {
        if out.status.success() {
            let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !value.is_empty() {
                return strip_tag_prefix(&value);
            }
        }
    }
    // Fallback: dev-<short-sha>[-dirty], matching scripts/ci/git-version.sh.
    let sha = match Command::new("git").args(["rev-parse", "--short", "HEAD"]).output() {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => "nogit".to_string(),
    };
    let dirty = match Command::new("git")
        .args(["diff", "--quiet", "--ignore-submodules", "--"])
        .output()
    {
        Ok(out) if out.status.success() => String::new(),
        Ok(_) => "-dirty".to_string(),
        Err(_) => String::new(),
    };
    format!("dev-{sha}{dirty}")
}

fn strip_tag_prefix(value: &str) -> String {
    let value = value.trim();
    for prefix in &["decode-light-", "decode-rust-gui-"] {
        if let Some(rest) = value.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    value.to_string()
}
