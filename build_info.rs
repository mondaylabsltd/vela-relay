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
    // for `git describe` to see any tag.
    let release = env_var("GITHUB_REF_NAME")
        .filter(|name| name.starts_with('v'))
        .unwrap_or_else(|| describe_release(repo_root));
    println!("cargo:rustc-env=VELA_RELAY_RELEASE={release}");
}

/// How this build relates to the last release tag: `v0.9.1` when it IS that
/// release, `v0.9.1+2` two commits past it, `-dirty` appended for uncommitted
/// changes. A build past a tag can therefore never silently claim to BE it.
///
/// Deliberately NOT `git describe`'s own string. That reads `v0.9.1-2-gf54d862`
/// — the commit again, abbreviated to a different length than the `commit`
/// field beside it, behind a `g` that only means "this is a git hash". One
/// response should not state the same commit twice in two spellings; the
/// distance is the only thing here the SHA does not already say.
fn describe_release(repo_root: &str) -> String {
    // `--abbrev=0` yields the bare nearest tag with no suffix.
    let Some(tag) = git(repo_root, &["describe", "--tags", "--abbrev=0"]) else {
        // No tag reachable at all (a shallow clone, or before the first
        // release). The commit is already reported on its own.
        return "unknown".into();
    };

    // Measured against HEAD, which is the tree cargo is building. Counting
    // from `GITHUB_SHA` instead could name a commit a shallow clone does not
    // contain, and a failure there would read as distance 0 — the build
    // claiming to BE the release, which is the one answer that must never be
    // guessed. So an uncountable distance says so out loud.
    let Some(distance) = git(repo_root, &["rev-list", "--count", &format!("{tag}..HEAD")])
        .and_then(|count| count.parse::<u32>().ok())
    else {
        return format!("{tag}+unknown");
    };
    // Any tracked-file change, staged or not. Untracked files are ignored:
    // they are not in the build.
    let dirty = git(repo_root, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.is_empty());

    let mut release = tag;
    if distance > 0 {
        release.push_str(&format!("+{distance}"));
    }
    if dirty {
        release.push_str("-dirty");
    }
    release
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
