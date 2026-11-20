// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Names the build of tuile this crate is compiled from — its commit and its
//! `git describe` — for the telemetry's `vcs.ref.head.revision` and
//! `vcs.ref.head.name`, with no variable to set by hand. The choice is
//! `src/vcs.rs`'s; this only asks git and the environment, and tells cargo
//! when to ask again: when `HEAD` moves, when the branch it is on moves,
//! when a tag is added.

#[path = "src/vcs.rs"]
mod vcs;

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/vcs.rs");
    println!("cargo:rerun-if-env-changed={}", vcs::COMMIT_VAR);
    println!("cargo:rerun-if-env-changed={}", vcs::DESCRIBE_VAR);

    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let checkout = git(&manifest_dir, &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .filter(|top| {
            let (Ok(top), Ok(manifest)) = (top.canonicalize(), manifest_dir.canonicalize()) else {
                return false;
            };
            vcs::own_checkout(&top, &manifest)
        });

    let (revision, describe) = match &checkout {
        Some(_) => {
            watch(&manifest_dir);
            (
                git(&manifest_dir, &["rev-parse", "--short=12", "HEAD"]),
                git(&manifest_dir, &["describe", "--tags", "--always", "--dirty"]),
            )
        }
        None => (None, None),
    };
    let env = |var: &str| std::env::var(var).ok();
    let revision = vcs::choose(revision.as_deref(), env(vcs::COMMIT_VAR).as_deref());
    let describe = vcs::choose(describe.as_deref(), env(vcs::DESCRIBE_VAR).as_deref());
    if let Some(revision) = revision {
        println!("cargo:rustc-env={}={revision}", vcs::REVISION_ENV);
    }
    if let Some(describe) = describe {
        println!("cargo:rustc-env={}={describe}", vcs::REF_NAME_ENV);
    }
}

/// One command's output, when git is there and it succeeds.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    vcs::clean(&String::from_utf8_lossy(&out.stdout))
}

/// The files whose change means another commit or another tag: `HEAD` (the
/// worktree's own), the ref it points at, the packed refs and the tags.
/// Only files that exist: cargo reruns at every build for a missing one.
fn watch(dir: &Path) {
    let absolute = |p: String| {
        let p = PathBuf::from(p);
        if p.is_absolute() {
            p
        } else {
            dir.join(p)
        }
    };
    let Some(git_dir) = git(dir, &["rev-parse", "--absolute-git-dir"]).map(absolute) else {
        return;
    };
    let common = git(dir, &["rev-parse", "--git-common-dir"])
        .map(absolute)
        .unwrap_or_else(|| git_dir.clone());
    let mut paths = vec![
        git_dir.join("HEAD"),
        common.join("packed-refs"),
        common.join("refs").join("tags"),
    ];
    if let Some(branch) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        paths.push(common.join(branch));
    }
    for path in paths.into_iter().filter(|p| p.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
