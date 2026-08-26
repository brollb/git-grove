//! Stamps the binary with the commit it was built from.
//!
//! Only when there is one: built from a crates.io tarball there is no git
//! anything, `GROVE_COMMIT` goes unset, and `--version` falls back to the plain
//! semver from Cargo.toml.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from) else {
        return;
    };
    // HEAD moving is what changes the answer, and it is not a source file, so
    // cargo has to be told to watch it. The dirty marker rides along on source
    // changes, which cargo already rebuilds for.
    watch(&git_dir.join("HEAD"));
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
        if let Some(refname) = head.strip_prefix("ref: ") {
            watch(&git_dir.join(refname.trim()));
            // A packed ref has no file of its own; packed-refs is where it moves.
            watch(&git_dir.join("packed-refs"));
        }
    }
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");

    let Some(commit) = git(&["rev-parse", "--short=8", "HEAD"]) else {
        return; // a repo with no commits yet
    };
    // Uncommitted changes mean the binary is not the commit it names. This sees
    // what cargo re-runs the build script for — src/ and Cargo.toml, the things
    // that end up in the binary — so an uncommitted README is not "dirty".
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|out| !out.trim().is_empty());
    let suffix = if dirty { "-dirty" } else { "" };
    println!("cargo:rustc-env=GROVE_COMMIT={commit}{suffix}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn watch(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
