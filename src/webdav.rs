//! WebDAV serving via `dav-server`, bridged into axum and gated behind
//! HTTP Basic auth using the per-boot password.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use base64::Engine;
use dav_server::{fakels::FakeLs, localfs::LocalFs, DavHandler};

use crate::SharedSettings;

/// Path prefix the WebDAV tree is mounted under.
pub const MOUNT: &str = "/dav";

/// Shared state for the WebDAV route: the handler, the boot password, and the
/// live settings (for permission enforcement on write methods).
#[derive(Clone)]
pub struct DavState {
    pub handler: DavHandler,
    pub password: Arc<str>,
    pub settings: SharedSettings,
}

/// Build a read/write WebDAV handler serving `root`, mounted at [`MOUNT`].
pub fn build_handler(root: &str) -> DavHandler {
    DavHandler::builder()
        .filesystem(LocalFs::new(root, false, false, false))
        .locksystem(FakeLs::new())
        .strip_prefix(MOUNT)
        // Render directory listings in a browser, so `/dav` is browsable too.
        .autoindex(true)
        .build_handler()
}

/// axum handler: authenticate, enforce permission, then hand off to dav-server.
pub async fn route(State(state): State<DavState>, req: Request) -> Response {
    if !authorized(&req, &state.password) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, r#"Basic realm="amber-dav""#)],
            "Authentication required\n",
        )
            .into_response();
    }

    // Enforce the permission level on WebDAV write/delete methods, matching the
    // JSON API (otherwise read-only could be bypassed via the mount).
    let perm = state.settings.permission;
    let method = req.method().as_str();
    let needs_delete = method == "DELETE";
    let needs_write = matches!(
        method,
        "PUT" | "DELETE" | "MKCOL" | "MOVE" | "COPY" | "PROPPATCH" | "LOCK" | "UNLOCK"
    );
    if (needs_delete && !perm.can_delete()) || (needs_write && !perm.can_write()) {
        return (StatusCode::FORBIDDEN, "operation not permitted\n").into_response();
    }

    // axum's body implements http_body::Body<Data = Bytes>, which is exactly
    // what `handle` wants; the response body just gets re-wrapped for axum.
    let (parts, body) = state.handler.handle(req).await.into_parts();
    Response::from_parts(parts, Body::new(body))
}

/// Check `Authorization: Basic ...` against the password (username ignored).
fn authorized(req: &Request, password: &str) -> bool {
    let Some(value) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
    else {
        return false;
    };

    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(value) else {
        return false;
    };
    let Ok(text) = String::from_utf8(decoded) else {
        return false;
    };

    matches!(text.split_once(':'), Some((_, pass)) if pass == password)
}
