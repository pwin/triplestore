//! What a binary was built from, so a running server can say which one it is.
//!
//! A version number alone does not answer "which build is this?": between two releases
//! every binary built from the working tree says the same number, and the one serving a
//! store may be older than the fix that was just committed. So a binary carries three
//! things — the crate version, the commit it was built at, and whether the tree had
//! uncommitted changes — and shows them in one form everywhere: `--version`, the startup
//! banner, the HTTP `Server` header ([RFC 9110
//! §10.2.4](https://www.rfc-editor.org/rfc/rfc9110#section-10.2.4)), and `/stats`.
//!
//! Two halves, in one crate because they must agree on the names: [`stamp`] runs in a
//! binary's build script and writes the commit and the cleanliness into the environment
//! the binary is compiled with, and [`build!`] reads them back into a [`Build`] at compile
//! time. A build without a repository — a source tarball, a registry build — has no
//! commit and says only its version.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::pedantic)]

use std::fmt;
use std::process::Command;

/// The version, commit and cleanliness a binary was built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Build {
    /// The crate version, `0.13.0`.
    pub version: &'static str,
    /// The short commit hash, or empty where the build had no repository.
    pub commit: &'static str,
    /// Whether the working tree differed from the commit: a binary built from work not
    /// yet committed, which is the one a version number cannot tell from the release.
    pub modified: bool,
}

impl Build {
    /// A build from its three parts. `const`, so a binary can hold one in a constant.
    #[must_use]
    pub const fn new(version: &'static str, commit: &'static str, modified: bool) -> Self {
        Self {
            version,
            commit,
            modified,
        }
    }

    /// The parenthetical after the version: `(d4b4fdd)`, `(d4b4fdd, modified)`, or
    /// nothing when there is no commit to name.
    fn note(&self) -> String {
        match (self.commit.is_empty(), self.modified) {
            (true, _) => String::new(),
            (false, false) => format!(" ({})", self.commit),
            (false, true) => format!(" ({}, modified)", self.commit),
        }
    }

    /// The HTTP `Server` header's value: a product token and, as its comment, the build —
    /// `holos/0.13.0 (d4b4fdd)`.
    #[must_use]
    pub fn product(&self, name: &str) -> String {
        format!("{name}/{}{}", self.version, self.note())
    }

    /// The build as JSON object members, for a status document to carry:
    /// `"version":"0.13.0","commit":"d4b4fdd","modified":false`. The commit is left out
    /// rather than sent empty when there is none.
    #[must_use]
    pub fn json_members(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(r#""version":"{}""#, self.version);
        if !self.commit.is_empty() {
            let _ = write!(out, r#","commit":"{}""#, self.commit);
        }
        let _ = write!(out, r#","modified":{}"#, self.modified);
        out
    }
}

impl fmt::Display for Build {
    /// `0.13.0 (d4b4fdd, modified)`: what `--version` prints after the binary's name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.version, self.note())
    }
}

/// The calling crate's [`Build`], from its own version and the stamp its build script
/// wrote with [`stamp`].
///
/// A macro rather than a function because `env!` reads the environment of the crate being
/// compiled, and it is the binary's build script, not this crate's, that set it.
#[macro_export]
macro_rules! build {
    () => {
        $crate::Build::new(
            env!("CARGO_PKG_VERSION"),
            env!("HOLOS_GIT_COMMIT"),
            !env!("HOLOS_GIT_MODIFIED").is_empty(),
        )
    };
}

/// Stamps the crate being built with its commit and cleanliness. Called from a binary's
/// `build.rs`, and nothing else.
///
/// Writes `HOLOS_GIT_COMMIT`, the short hash or empty, and `HOLOS_GIT_MODIFIED`, `1` when
/// `git status` reports anything and empty otherwise — both for [`build!`] to read.
/// Without `git` on the path or a repository above the crate, both are empty and the
/// binary says only its version.
///
/// The script is re-run whenever anything under the workspace's `crates/` changes, or its
/// manifests, or the repository's `HEAD` and index. That is the whole point: a stamp that
/// still said "clean" after an edit and a rebuild would be the one lie this exists to
/// prevent, and the cost is a `git status` per build that changed something.
///
/// # Panics
///
/// If `CARGO_MANIFEST_DIR` is unset, which cannot happen under Cargo.
pub fn stamp() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo for a build script");
    let root = std::path::Path::new(&manifest).join("../..");
    for watched in ["crates", "Cargo.toml", "Cargo.lock", ".git/HEAD", ".git/index"] {
        println!("cargo:rerun-if-changed={}", root.join(watched).display());
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let commit = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_default();
    let modified = if commit.is_empty() {
        ""
    } else {
        match git(&["status", "--porcelain"]) {
            Some(status) if status.is_empty() => "",
            Some(_) => "1",
            None => "",
        }
    };
    println!("cargo:rustc-env=HOLOS_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=HOLOS_GIT_MODIFIED={modified}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_build_names_its_commit() {
        let build = Build::new("0.13.0", "d4b4fdd", false);
        assert_eq!(build.to_string(), "0.13.0 (d4b4fdd)");
        assert_eq!(build.product("holos"), "holos/0.13.0 (d4b4fdd)");
        assert_eq!(
            build.json_members(),
            r#""version":"0.13.0","commit":"d4b4fdd","modified":false"#
        );
    }

    #[test]
    fn a_modified_tree_says_so() {
        let build = Build::new("0.13.0", "d4b4fdd", true);
        assert_eq!(build.to_string(), "0.13.0 (d4b4fdd, modified)");
        assert_eq!(build.product("holos"), "holos/0.13.0 (d4b4fdd, modified)");
        assert!(build.json_members().ends_with(r#""modified":true"#));
    }

    #[test]
    fn a_build_without_a_repository_says_only_its_version() {
        let build = Build::new("0.13.0", "", false);
        assert_eq!(build.to_string(), "0.13.0");
        assert_eq!(build.product("holos"), "holos/0.13.0");
        assert_eq!(build.json_members(), r#""version":"0.13.0","modified":false"#);
    }

    /// This crate is built inside the repository, so the stamp would name a commit — the
    /// one check of `stamp`'s inputs that needs no second process.
    #[test]
    fn the_stamp_finds_this_repository() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let output = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--short=7", "HEAD"])
            .output();
        let Ok(output) = output else {
            eprintln!("skipping: no git on the path");
            return;
        };
        assert!(output.status.success(), "this is a git checkout");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim().len(), 7);
    }
}
