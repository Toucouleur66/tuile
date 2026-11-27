// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Which build of tuile this is, as the build script decides it: the choice
//! between what git says and what the environment says, kept here as plain
//! functions so that it is tested like the rest. `build.rs` includes this
//! file; the library only compiles it for its tests.
//!
//! - In a checkout of tuile, git answers: `git rev-parse --short=12 HEAD`
//!   for the revision, `git describe --tags --always --dirty` for the name.
//! - Otherwise — a tarball, a container build without `.git`, a package
//!   from a registry — `TUILE_GIT_COMMIT` and `TUILE_GIT_DESCRIBE`, which a
//!   CI or release build sets.
//! - Otherwise nothing: the attribute is absent, never made up.

use std::path::Path;

/// The variable a build without git names its commit in.
pub const COMMIT_VAR: &str = "TUILE_GIT_COMMIT";
/// The variable a build without git names its tag (a `git describe`) in.
pub const DESCRIBE_VAR: &str = "TUILE_GIT_DESCRIBE";

/// The library's own names for what the build script found.
pub const REVISION_ENV: &str = "TUILE_BUILD_REVISION";
pub const REF_NAME_ENV: &str = "TUILE_BUILD_REF_NAME";

/// One line of a command's output or a variable's value, trimmed; nothing
/// when empty or when it holds anything but printable ASCII (a value that
/// cannot be a commit or a tag is no value).
pub fn clean(raw: &str) -> Option<String> {
    let line = raw.trim();
    (!line.is_empty() && line.chars().all(|c| c.is_ascii_graphic())).then(|| line.to_owned())
}

/// What git says if it says anything usable, else what the environment
/// says, else nothing.
pub fn choose(from_git: Option<&str>, from_env: Option<&str>) -> Option<String> {
    from_git
        .and_then(clean)
        .or_else(|| from_env.and_then(clean))
}

/// Whether the repository whose top is `toplevel` is tuile's own checkout
/// holding this crate at `manifest_dir` — not some other project's
/// repository that happens to contain a vendored copy, whose commit says
/// nothing about tuile.
pub fn own_checkout(toplevel: &Path, manifest_dir: &Path) -> bool {
    toplevel.join("crates").join("tuile-telemetry") == manifest_dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_comes_first_then_the_environment_then_nothing() {
        assert_eq!(
            choose(Some("abc123def456\n"), Some("fedcba987654")).as_deref(),
            Some("abc123def456")
        );
        assert_eq!(
            choose(None, Some(" v0.17.1-12-gabc1234 ")).as_deref(),
            Some("v0.17.1-12-gabc1234")
        );
        assert_eq!(
            choose(Some("  \n"), Some("v0.17.1")).as_deref(),
            Some("v0.17.1")
        );
        assert_eq!(choose(None, None), None);
        assert_eq!(choose(Some(""), Some("")), None);
    }

    #[test]
    fn only_a_plausible_value_is_kept() {
        assert_eq!(
            clean("v0.17.1-12-gabc1234-dirty\n").as_deref(),
            Some("v0.17.1-12-gabc1234-dirty")
        );
        assert_eq!(clean("fatal: not a git repository"), None);
        assert_eq!(clean("abc\u{7}"), None);
        assert_eq!(clean(""), None);
    }

    #[test]
    fn only_tuile_s_own_checkout_is_asked() {
        let top = Path::new("/src/tuile");
        assert!(own_checkout(
            top,
            Path::new("/src/tuile/crates/tuile-telemetry")
        ));
        // A host's repository with tuile vendored somewhere inside.
        assert!(!own_checkout(
            Path::new("/src/game"),
            Path::new("/src/game/vendor/tuile-telemetry")
        ));
        assert!(!own_checkout(
            Path::new("/src/game"),
            Path::new("/src/game/vendor/tuile/crates/tuile-telemetry")
        ));
    }
}
