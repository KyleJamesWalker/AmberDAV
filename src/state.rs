//! Shared application state for the HTTP handlers.
//!
//! Lives in its own module (rather than `main.rs`) so handler modules import
//! their state from here instead of the binary root, and so the router can be
//! built — and driven in tests — without going through `main()`.

use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use axum::extract::FromRef;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::{config, input::InputUpdate, screen, throttle::Throttle, webdav::DavState};

// --- path safety ---------------------------------------------------------------
//
// The textual half of the path-safety trio lives here, next to `MountTable`
// — every consumer (`files`, the mount resolver) shares ONE copy, so a
// hardening change can never apply to one surface and miss the other.
// Symlink containment is the other half, enforced by `files::confine`.

/// True when `seg` is a plain file name: no backslash or NUL, and it parses
/// as exactly one `Normal` path component on this OS. The component check is
/// what keeps `PathBuf::push` honest on Windows, where pushing a segment with
/// a prefix (`C:`, `\\?\C:\x`) or a root *replaces* the base path instead of
/// appending — handing out the whole drive. Rooted segments (`/etc`) and
/// `.`/`..` parse as non-`Normal` components and are rejected the same way.
pub(crate) fn plain_segment(seg: &str) -> bool {
    if seg.contains('\\') || seg.contains('\0') {
        return false;
    }
    // Reserved device names (`con`, `nul.txt`, …) never name a regular file
    // on Windows: creating one fails with an opaque OS error and *opening*
    // one reaches the device itself, so reject them up front there. On Unix
    // they are ordinary names and stay allowed.
    if cfg!(windows) && windows_reserved(seg) {
        return false;
    }
    let mut comps = Path::new(seg).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

/// True when `name` (a single path segment) is a Windows reserved device
/// name — `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9` — either
/// bare or with an extension (`nul.txt` still opens the NUL device). The
/// comparison is ASCII case-insensitive and ignores trailing spaces in the
/// stem, matching Win32 name resolution. Pure so the table is testable on
/// every host; [`plain_segment`] only *applies* it on Windows targets.
pub(crate) fn windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    if !stem.is_ascii() {
        return false;
    }
    match stem.len() {
        3 => ["con", "prn", "aux", "nul"]
            .into_iter()
            .any(|dev| stem.eq_ignore_ascii_case(dev)),
        4 => {
            let (dev, digit) = stem.split_at(3);
            (dev.eq_ignore_ascii_case("com") || dev.eq_ignore_ascii_case("lpt"))
                && matches!(digit.as_bytes()[0], b'1'..=b'9')
        }
        _ => false,
    }
}

/// Resolve a `/`-separated relative path into an absolute path lexically
/// under `root`: every segment must pass [`plain_segment`], so no `..`, no
/// rooted or drive-letter segments, no separators.
pub(crate) fn resolve_segments(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for seg in rel.split('/') {
        match seg {
            "" | "." => continue,
            s if plain_segment(s) => out.push(s),
            _ => return None,
        }
    }
    Some(out)
}

// --- MountTable ---------------------------------------------------------------

/// Named mount points, or a single anonymous root.
///
/// **Single mode** (one root, no name): the virtual path maps directly to the
/// filesystem; no mount-name prefix appears in URLs. Identical to the
/// pre-multi-root behavior.
///
/// **Multi mode** (two or more named roots): the virtual `/` is a synthetic
/// read-only collection listing the mount names; real content starts one level
/// down (`/name/file.txt`). The names are the stable URL identifiers that DAV
/// clients bookmark.
#[derive(Clone)]
pub struct MountTable {
    /// Ordered list of `(name, canonical_root)`. For single mode the name is
    /// `""`. At least one entry is always present.
    mounts: Arc<Vec<(String, PathBuf)>>,
    /// True when this is a single anonymous root (mounts.len() == 1, name == "").
    single: bool,
}

impl MountTable {
    /// Single-root mode: one directory, no mount prefix in URLs.
    pub fn single(path: PathBuf) -> Self {
        MountTable {
            mounts: Arc::new(vec![("".to_string(), path)]),
            single: true,
        }
    }

    /// Multi-root mode: named mounts, virtual root synthesized. Must contain
    /// at least two entries; panics in debug builds otherwise.
    pub fn multi(mounts: Vec<(String, PathBuf)>) -> Self {
        debug_assert!(mounts.len() >= 2, "multi needs at least two mounts");
        MountTable {
            mounts: Arc::new(mounts),
            single: false,
        }
    }

    pub fn is_single(&self) -> bool {
        self.single
    }

    /// True for multi-root when `rel` addresses the virtual root (`""` or any
    /// number of leading/trailing slashes). Always false in single mode.
    pub fn is_virtual_root(&self, rel: &str) -> bool {
        !self.single && rel.trim_matches('/').is_empty()
    }

    /// For single-root: the canonical root path. For multi-root: `None`.
    pub fn single_root(&self) -> Option<&Path> {
        self.single.then(|| self.mounts[0].1.as_path())
    }

    /// All mounts as `(name, canonical_path)` slices. The name is `""` for
    /// single-root; callers that need to display names should check `is_single`.
    pub fn mounts(&self) -> &[(String, PathBuf)] {
        &self.mounts
    }

    /// Resolve a request-relative path to `(mount_root, absolute_path)`.
    ///
    /// - **Single mode**: maps `rel` into the single root.
    /// - **Multi mode**: the first `/`-separated segment selects the mount;
    ///   the rest is resolved inside that mount.
    ///
    /// Returns `None` for traversal attempts, unknown mount names, and (in
    /// multi mode) an empty first segment — the virtual root has no filesystem
    /// path; handlers check [`is_virtual_root`](Self::is_virtual_root) first.
    pub fn resolve(&self, rel: &str) -> Option<(&Path, PathBuf)> {
        if self.single {
            let root = &self.mounts[0].1;
            resolve_segments(root, rel).map(|p| (root.as_path(), p))
        } else {
            let rel = rel.trim_start_matches('/');
            // Split on the first `/` to peel off the mount name.
            let (mount_name, rest) = rel.split_once('/').unwrap_or((rel, ""));
            if mount_name.is_empty() {
                return None; // virtual root — caller checks is_virtual_root
            }
            let (_, root) = self.mounts.iter().find(|(n, _)| n == mount_name)?;
            resolve_segments(root, rest).map(|p| (root.as_path(), p))
        }
    }
}

/// Server facts shown on the status page and startup banner.
pub struct ServerInfo {
    pub port: u16,
    pub password: crate::password::PasswordMatcher,
    /// Set when the config file existed but could not be used (parse/read
    /// failure) — surfaced on the Status tab so a broken config is never
    /// invisible (issue #19).
    pub config_error: Option<String>,
}

/// Fixed advertised address, set once at startup when `--bind` names a
/// specific address (issue #59). Unset = advertise the detected LAN IP.
static FIXED_IP: OnceLock<IpAddr> = OnceLock::new();

/// Parse the configured bind address into the address user-facing surfaces
/// must advertise. `Some` when it names a specific address (including
/// loopback): that is the only address accepting connections, so showing the
/// detected LAN IP would hand out a URL that refuses to connect (issue #59).
/// `None` for the unspecified addresses (`0.0.0.0`, `::`) — the server
/// listens everywhere, so the detected LAN IP is right — and for anything
/// unparseable (e.g. a hostname), where live detection is the safest
/// fallback.
pub fn advertised_ip(bind: &str) -> Option<IpAddr> {
    bind.trim()
        .parse::<IpAddr>()
        .ok()
        .filter(|ip| !ip.is_unspecified())
}

/// Record the bind address for [`current_ip`]. Called once from `main`, after
/// the bind address is resolved and before any surface renders an address.
pub fn pin_advertised_ip(bind: &str) {
    if let Some(ip) = advertised_ip(bind) {
        let _ = FIXED_IP.set(ip);
    }
}

/// The address every user-facing surface advertises (stdout banner + QR, the
/// device screen, `/api/info`, the `connection.json` sidecar). Bound to a
/// specific address → always that address (issue #59). Bound to all
/// interfaces → the detected LAN IP, re-queried live (not cached at boot) so
/// the screen/info recover once Wi-Fi connects after launch.
pub fn current_ip() -> IpAddr {
    match FIXED_IP.get() {
        Some(ip) => *ip,
        None => local_ip_address::local_ip().unwrap_or(IpAddr::from([0, 0, 0, 0])),
    }
}

/// Settings, loaded once at boot from the config file (file-owned, read-only
/// at runtime — the UI only displays them).
pub type SharedSettings = Arc<config::Settings>;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// Mount table: one root (single mode) or several named mounts (multi
    /// mode). All file-API handlers route through this.
    pub mounts: Arc<MountTable>,
    /// Opaque per-boot session token handed out on successful login.
    pub session: Arc<str>,
    /// Settings loaded from the config file.
    pub settings: SharedSettings,
    pub dav: DavState,
    pub info: Arc<ServerInfo>,
    /// Per-IP auth-failure throttle, shared with the WebDAV mount (one guess
    /// budget per client across both password surfaces — issue #27).
    pub throttle: Arc<Throttle>,
    pub events: broadcast::Sender<InputUpdate>,
    pub screen_status: screen::Status,
    /// Fires on shutdown so long-lived SSE streams (the Status page's live
    /// input) end and don't stall graceful shutdown.
    pub shutdown: CancellationToken,
}

impl AppState {
    /// Configured permission level.
    pub fn permission(&self) -> config::Permission {
        self.settings.permission
    }
}

// Lets the WebDAV route extract just its slice of state.
impl FromRef<AppState> for DavState {
    fn from_ref(state: &AppState) -> Self {
        state.dav.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // ---- MountTable: single-root mode ----------------------------------------

    #[test]
    fn single_root_resolve_maps_directly() {
        let t = MountTable::single(PathBuf::from("/srv/root"));
        let (root, path) = t.resolve("a/b").unwrap();
        assert_eq!(root, Path::new("/srv/root"));
        assert_eq!(path, PathBuf::from("/srv/root/a/b"));
    }

    #[test]
    fn single_root_resolve_empty_is_root() {
        let t = MountTable::single(PathBuf::from("/srv/root"));
        let (_, path) = t.resolve("").unwrap();
        assert_eq!(path, PathBuf::from("/srv/root"));
    }

    #[test]
    fn single_root_resolve_blocks_traversal() {
        let t = MountTable::single(PathBuf::from("/srv/root"));
        assert!(t.resolve("../../etc").is_none());
        assert!(t.resolve("a/../../../etc").is_none());
    }

    #[test]
    fn single_root_is_virtual_root_always_false() {
        let t = MountTable::single(PathBuf::from("/srv/root"));
        assert!(!t.is_virtual_root(""));
        assert!(!t.is_virtual_root("/"));
    }

    // ---- MountTable: multi-root mode ------------------------------------------

    fn two_mount_table() -> MountTable {
        MountTable::multi(vec![
            ("one".to_string(), PathBuf::from("/srv/one")),
            ("two".to_string(), PathBuf::from("/srv/two")),
        ])
    }

    #[test]
    fn multi_resolve_dispatches_on_first_segment() {
        let t = two_mount_table();
        let (root, path) = t.resolve("one/file.txt").unwrap();
        assert_eq!(root, Path::new("/srv/one"));
        assert_eq!(path, PathBuf::from("/srv/one/file.txt"));

        let (root, path) = t.resolve("/two/sub/dir").unwrap();
        assert_eq!(root, Path::new("/srv/two"));
        assert_eq!(path, PathBuf::from("/srv/two/sub/dir"));
    }

    #[test]
    fn multi_resolve_mount_root_itself() {
        let t = two_mount_table();
        let (root, path) = t.resolve("one").unwrap();
        assert_eq!(root, Path::new("/srv/one"));
        assert_eq!(path, PathBuf::from("/srv/one"));
    }

    #[test]
    fn multi_resolve_unknown_mount_is_none() {
        let t = two_mount_table();
        assert!(t.resolve("unknown/file").is_none());
    }

    #[test]
    fn multi_resolve_virtual_root_is_none() {
        let t = two_mount_table();
        assert!(t.resolve("").is_none());
        assert!(t.resolve("/").is_none());
    }

    #[test]
    fn multi_is_virtual_root_detects_empty_path() {
        let t = two_mount_table();
        assert!(t.is_virtual_root(""));
        assert!(t.is_virtual_root("/"));
        assert!(t.is_virtual_root("///"));
        assert!(!t.is_virtual_root("one"));
        assert!(!t.is_virtual_root("one/file"));
    }

    #[test]
    fn multi_resolve_blocks_traversal_within_mount() {
        let t = two_mount_table();
        assert!(t.resolve("one/../../etc").is_none());
        assert!(t.resolve("one/../two/secret").is_none()); // can't jump mounts
    }

    // A specific bind address — loopback included — is the only address that
    // accepts connections, so it is what every surface must advertise
    // (issue #59). Loopback is the motivating case: `--bind 127.0.0.1` used
    // to print a QR pointing at the LAN IP, which refuses connections.
    #[test]
    fn specific_bind_addresses_are_advertised() {
        assert_eq!(
            advertised_ip("192.168.1.5"),
            Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)))
        );
        assert_eq!(
            advertised_ip("127.0.0.1"),
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
        assert_eq!(advertised_ip("::1"), Some(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert_eq!(
            advertised_ip("fe80::1"),
            Some(IpAddr::V6("fe80::1".parse().unwrap()))
        );
        // Whitespace from a hand-edited config must not disable the pinning.
        assert_eq!(
            advertised_ip(" 127.0.0.1 "),
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }

    // Unspecified binds listen on every interface, so the live-detected LAN
    // IP stays correct; unparseable values (hostnames, garbage) also fall
    // back to detection — the pre-existing behavior is the safest guess.
    #[test]
    fn unspecified_and_unparseable_binds_fall_back_to_detection() {
        assert_eq!(advertised_ip("0.0.0.0"), None);
        assert_eq!(advertised_ip("::"), None);
        assert_eq!(advertised_ip("0:0:0:0:0:0:0:0"), None);
        assert_eq!(advertised_ip("localhost"), None);
        assert_eq!(advertised_ip(""), None);
        assert_eq!(advertised_ip("not-an-ip"), None);
    }
}
