//! Compose the version string the binary will report (issue #46).
//!
//! `Cargo.toml` deliberately ships `version = "0.0.0"` — the release
//! workflow stamps the real version from the git tag (`cargo set-version`)
//! before building, and that must stay the single source of truth. So:
//!
//! - **Stamped release builds** (version != 0.0.0): pass the stamped version
//!   through untouched; git is never invoked.
//! - **From-source builds** (the 0.0.0 placeholder): append
//!   `git describe --tags --dirty --always` as semver build metadata, e.g.
//!   `0.0.0+v1.2.0-4-gabc1234-dirty` or `0.0.0+abc1234` on a tagless
//!   checkout (CI checks out PRs without tags) — recognizably a dev build
//!   everywhere the version is shown.
//! - **No git at all** (tarball builds, missing binary): plain `0.0.0`,
//!   exactly the old behavior.
//!
//! The result is exposed as `AMBERDAV_VERSION`; `src/version.rs` falls back
//! to `CARGO_PKG_VERSION` if the variable is ever absent.

fn main() {
    // Printing any rerun-if directive replaces cargo's default "rerun on any
    // file change", so list build.rs itself plus the cheap, best-effort git
    // state markers: HEAD moves on checkout/branch switch, the index is
    // rewritten by commits and staging. (Unstaged edits can leave a stale
    // -dirty flag until the next build script rerun — acceptable for a dev
    // version label.)
    println!("cargo:rerun-if-changed=build.rs");
    for marker in [".git/HEAD", ".git/index"] {
        if std::path::Path::new(marker).exists() {
            println!("cargo:rerun-if-changed={marker}");
        }
    }

    let cargo_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let version = if cargo_version == "0.0.0" {
        match git_describe() {
            Some(describe) => format!("{cargo_version}+{describe}"),
            None => cargo_version,
        }
    } else {
        // Stamped by the release workflow: stay out of the way.
        cargo_version
    };
    println!("cargo:rustc-env=AMBERDAV_VERSION={version}");
}

/// `git describe --tags --dirty --always`, or `None` when git is missing,
/// this isn't a work tree, or the output is unusable.
fn git_describe() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["describe", "--tags", "--dirty", "--always"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    // Guard against anything that can't ride in a version string (and feed
    // a header/canvas) — describe output is tag-name + hex + known suffixes.
    let clean = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    clean.then_some(s)
}
