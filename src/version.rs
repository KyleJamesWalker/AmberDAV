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

/// True for from-source development builds: the unstamped `0.0.0`
/// placeholder, with or without describe metadata. Dev builds get special
/// update-check handling — semver-wise 0.0.0 is older than every release,
/// so a plain comparison would always offer to overwrite the custom build.
pub fn is_dev(version: &str) -> bool {
    version == "0.0.0" || version.starts_with("0.0.0+") || version.starts_with("0.0.0-")
}

/// Numeric `x.y.z` ordering. Anything after `+` or `-` (build metadata,
/// pre-release tags) is ignored and missing/unparseable components count as
/// zero — release tags are plain `x.y.z`, so this is exactly as much semver
/// as the update check needs, without pulling in a crate for it.
pub fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    parse(a).cmp(&parse(b))
}

fn parse(v: &str) -> (u64, u64, u64) {
    let core = v.split(['+', '-']).next().unwrap_or("");
    let mut parts = core
        .split('.')
        .map(|p| p.trim().parse::<u64>().unwrap_or(0));
    let mut next = || parts.next().unwrap_or(0);
    (next(), next(), next())
}

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

    // Dev classification: every shape build.rs can produce for an unstamped
    // build is a dev build; stamped releases never are — even 0.0.x ones.
    #[test]
    fn dev_classification() {
        assert!(is_dev("0.0.0"));
        assert!(is_dev("0.0.0+abc1234"));
        assert!(is_dev("0.0.0+v1.3.0-12-gc72bc1a-dirty"));
        assert!(is_dev("0.0.0-rc1"));
        assert!(!is_dev("1.2.3"));
        assert!(!is_dev("0.0.1"));
        assert!(!is_dev("0.0.10"));
    }

    // The ordering the update check relies on: newer-than-latest must not
    // read as out of date (no downgrade offer), equality is up to date, and
    // metadata/pre-release suffixes don't disturb the numeric core.
    #[test]
    fn version_ordering_is_numeric() {
        use std::cmp::Ordering::*;
        assert_eq!(cmp_versions("1.2.3", "1.2.3"), Equal);
        assert_eq!(cmp_versions("1.2.3", "1.2.4"), Less);
        assert_eq!(cmp_versions("1.3.0", "1.2.9"), Greater);
        assert_eq!(cmp_versions("1.10.0", "1.9.9"), Greater); // numeric, not lexical
        assert_eq!(cmp_versions("2.0.0", "1.99.99"), Greater);
        assert_eq!(cmp_versions("1.2", "1.2.0"), Equal); // missing component = 0
        assert_eq!(cmp_versions("1.2.3+meta", "1.2.3"), Equal);
        assert_eq!(cmp_versions("0.0.0+v1.3.0-12-g123", "1.3.0"), Less);
        assert_eq!(cmp_versions("garbage", "0.0.0"), Equal); // unparseable -> zeros
    }
}
