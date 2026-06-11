//! Shared application state for the HTTP handlers.
//!
//! Lives in its own module (rather than `main.rs`) so handler modules import
//! their state from here instead of the binary root, and so the router can be
//! built — and driven in tests — without going through `main()`.

use std::{net::IpAddr, path::PathBuf, sync::Arc};

use axum::extract::FromRef;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::{config, input::InputUpdate, screen, webdav::DavState};

/// Server facts shown on the status page and startup banner.
pub struct ServerInfo {
    pub port: u16,
    pub password: String,
    /// Set when the config file existed but could not be used (parse/read
    /// failure) — surfaced on the Status tab so a broken config is never
    /// invisible (issue #19).
    pub config_error: Option<String>,
}

/// Resolve the device's current LAN IP. Re-queried live (not cached at boot)
/// so the screen/info recover once Wi-Fi connects after launch.
pub fn current_ip() -> IpAddr {
    local_ip_address::local_ip().unwrap_or(IpAddr::from([0, 0, 0, 0]))
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
