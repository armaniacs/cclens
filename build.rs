//! Stamp the binary with the release it was built from.
//!
//! The committed `Cargo.toml` keeps `version = "0.0.0"` as a placeholder — the
//! tags are the record — so a version baked from the package metadata alone
//! would print `0.0.0` for everyone building from source. Instead this script
//! asks git what the checkout is: `git describe --tags` names the release
//! (`v0.5.0`, or `v0.5.0-3-gabc1234` past the tag), and the leading `v` comes
//! off so `--version` prints a plain semver. Where git cannot answer (a
//! tarball without `.git`, a missing binary) the build falls back to the
//! package version, which the release workflow stamps from the tag before
//! publishing.

use std::process::Command;

fn main() {
    // Re-resolve when the ref set can move under us: a new tag must change
    // the reported version even when no source file changed with it.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/");
    println!("cargo:rerun-if-changed=.git/packed-refs");

    let described = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let version = described
        .map(|s| s.strip_prefix('v').unwrap_or(&s).to_string())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=SANIWALENS_VERSION={version}");
}
