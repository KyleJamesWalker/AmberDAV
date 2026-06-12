//! The running build's version string (issue #46).
//!
//! Release binaries are stamped by CI (`cargo set-version` from the release
//! tag), so their version is the plain `x.y.z` from `CARGO_PKG_VERSION`.
//! From-source builds keep the deliberate `0.0.0` placeholder; `build.rs`
//! appends `git describe` output as build metadata (`0.0.0+abc1234-dirty`),
//! and tarball builds without git stay exactly `0.0.0`.

/// The version shown on the device screen, `/api/info`, `--version`, and
/// compared by the update check. Composed by `build.rs`; the fallback only
/// matters if the build script ever stops emitting the variable.
pub const VERSION: &str = match option_env!("AMBERDAV_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

#[cfg(test)]
mod tests {
    use super::*;

    // Whatever shape the build produced, the version must start with the
    // cargo version (release: stamped x.y.z as-is; dev: 0.0.0 plus optional
    // +describe metadata) — i.e. build.rs never *replaces* the version.
    #[test]
    fn version_extends_but_never_replaces_the_cargo_version() {
        assert!(VERSION.starts_with(env!("CARGO_PKG_VERSION")));
        let suffix = &VERSION[env!("CARGO_PKG_VERSION").len()..];
        assert!(
            suffix.is_empty() || suffix.starts_with('+'),
            "unexpected version shape: {VERSION}"
        );
    }
}
