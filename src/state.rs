//! Shared application state for the HTTP handlers.
//!
//! Lives in its own module (rather than `main.rs`) so handler modules import
//! their state from here instead of the binary root, and so the router can be
//! built — and driven in tests — without going through `main()`.

use std::{
    net::IpAddr,
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use axum::extract::FromRef;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::{config, input::InputUpdate, screen, throttle::Throttle, webdav::DavState};

/// Server facts shown on the status page and startup banner.
pub struct ServerInfo {
    pub port: u16,
    pub password: String,
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
    /// Canonical directory served over WebDAV and the file API.
    pub root: Arc<PathBuf>,
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
