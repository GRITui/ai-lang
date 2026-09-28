//! Bake build provenance into the `ainl` binary so `ainl --version` can report
//! exactly which artifact you are running.
//!
//! Three facts, all compile-time:
//!   * `AINL_TARGET` — the Rust target triple this binary was *built for*.
//!     `std::env::consts::OS/ARCH` describes the host it is *running on*; for
//!     a release artifact the two differ (a Linux x86_64 tarball inspected on
//!     an M-series Mac), and the build target is the one a bug report needs.
//!   * `AINL_GIT_COMMIT` — the source revision, for the same reason.
//!   * `AINL_GIT_DIRTY` — whether the tree had uncommitted changes, so a
//!     reported commit can be qualified honestly.
//!
//! This shells out to `git`; it does not depend on it. A source tarball, a
//! vendored build, or a host with no git all fall back to `unknown`, which
//! `ainl --version` prints rather than omitting — provenance that is silently
//! missing is worse than provenance that says it is missing.
//!
//! `AINL_GIT_COMMIT` in the environment wins over the git probe, so a vendor or
//! a reproducible-build script can stamp the revision itself.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Emit a `rerun-if-changed` for everything that can change the answer, then
/// let cargo cache normally. Without a directive cargo re-runs this script on
/// any package file change, which is broader than needed but never stale;
/// listing the git paths narrows it to the case that actually matters — a new
/// commit with an unchanged tree would otherwise keep a stale commit hash.
fn track_git_paths(git_dir: &Path) {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AINL_GIT_COMMIT");
    println!("cargo:rerun-if-env-changed=AINL_GIT_DIRTY");
    let head = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head.display());
    // A packed ref changes the answer without touching the loose ref file, so
    // the packed-refs index has to be tracked too.
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );
    // `git rev-parse --git-dir` gives us this; resolved here only to add the
    // loose ref for the branch HEAD points at, if it is a symbolic ref.
    if let Ok(branch) = std::fs::read_to_string(&head) {
        if let Some(name) = branch.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed={}", git_dir.join(name).display());
        }
    }
}

/// Locate the `.git` directory for the package, walking up as git's own
/// discovery rules do. Returns `None` outside a checkout (a crates.io tarball,
/// a vendored copy).
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = dir.join(".git");
        // A worktree or submodule checkout puts a *file* at .git pointing at
        // the real directory ("gitdir: <path>"); the plain directory is the
        // common case and is enough to answer the question here.
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn git(git_dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(git_dir.parent()?)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let t = s.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn main() {
    // Cargo always sets TARGET for a build script.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=AINL_TARGET={target}");

    let (commit, dirty) = match std::env::var("AINL_GIT_COMMIT") {
        Ok(c) if !c.trim().is_empty() => (
            c.trim().to_string(),
            std::env::var("AINL_GIT_DIRTY").unwrap_or_else(|_| "unknown".into()),
        ),
        _ => match find_git_dir(&std::env::current_dir().unwrap_or_default()) {
            Some(git_dir) => {
                track_git_paths(&git_dir);
                let commit = git(&git_dir, &["rev-parse", "--short=12", "HEAD"])
                    .unwrap_or_else(|| "unknown".to_string());
                // `--porcelain` is non-empty exactly when the worktree differs
                // from HEAD (untracked files count: an untracked source file
                // changes what you built, so it must count as dirty).
                let dirty = git(&git_dir, &["status", "--porcelain"])
                    .map(|s| (!s.is_empty()).to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                (commit, dirty)
            }
            None => ("unknown".to_string(), "unknown".to_string()),
        },
    };
    println!("cargo:rustc-env=AINL_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=AINL_GIT_DIRTY={dirty}");
}
