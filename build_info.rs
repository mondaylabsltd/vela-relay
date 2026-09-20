// Build-time identity, shared by both shells' build scripts.
//
// `include!`d rather than made a crate: a build script cannot depend on a
// workspace member, and one copy of this logic is the only way the two
// deployments can be trusted to answer the same question the same way.
//
// It emits the two facts an operator needs from a running deployment:
//
// - `VELA_RELAY_RELEASE` — which release this is.
// - `VELA_RELAY_BUILD_SHA` — exactly which commit it was built from.
//
// Both are baked in at compile time, so a response can never claim a version
// the running binary is not.

use std::{env, path::Path, process::Command};

/// `repo_root` is the repository root relative to the crate being built; the
/// two shells sit at different depths (`.` for the root package, `..` for
/// `vela-relay-cf`).
fn emit_build_info(repo_root: &str) {
    // A commit only rewrites `.git/HEAD` when the BRANCH changes, so watching
    // that file alone bakes the first build's commit into every later one —
    // exactly the staleness that makes a version endpoint worse than none.
    // `.git/logs/HEAD` is appended on every commit, checkout, merge and reset,
    // which is the event that actually invalidates this.
    for path in ["HEAD", "logs/HEAD"] {
        let path = format!("{repo_root}/.git/{path}");
        // A missing path reads as "changed" to cargo and would rebuild every
        // time, so only watch what is actually there (a source tarball has no
        // `.git` at all).
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");

    let commit = env_var("GITHUB_SHA")
        .map(|sha| sha.chars().take(12).collect::<String>())
        .or_else(|| git(repo_root, &["rev-parse", "--short=12", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=VELA_RELAY_BUILD_SHA={commit}");

    // The release tag is the version an operator deploys. `CARGO_PKG_VERSION`
    // is NOT that version and never has been: it has read 0.1.0 across every
    // release tag this repository has cut, so reporting it would answer the
    // operator's question with a number that cannot change.
    //
    // On a CI tag push `GITHUB_REF_NAME` is the tag itself, which is also the
    // only thing that works there — `actions/checkout` fetches too shallowly
    // for `git describe` to see any tag. Otherwise `git describe` names the
    // last release plus how far past it this build sits (`v0.9.1-3-gabc123`,
    // `-dirty` for uncommitted changes), so a hand-rolled build can never
    // silently claim to BE the release.
    let release = env_var("GITHUB_REF_NAME")
        .filter(|name| name.starts_with('v'))
        .or_else(|| git(repo_root, &["describe", "--tags", "--always", "--dirty"]))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=VELA_RELAY_RELEASE={release}");
}

fn env_var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn git(repo_root: &str, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}
