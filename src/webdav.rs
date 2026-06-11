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

use crate::{config::Permission, state::SharedSettings};

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
    if !method_allowed(req.method().as_str(), state.settings.permission) {
        return (StatusCode::FORBIDDEN, "operation not permitted\n").into_response();
    }

    // axum's body implements http_body::Body<Data = Bytes>, which is exactly
    // what `handle` wants; the response body just gets re-wrapped for axum.
    let (parts, body) = state.handler.handle(req).await.into_parts();
    Response::from_parts(parts, Body::new(body))
}

/// True when `method` may proceed at permission level `perm`. This list *is*
/// the read-only/read-write guarantee for the WebDAV mount: the write methods
/// require `can_write`, and `DELETE` additionally requires `can_delete`.
/// Anything else (GET, HEAD, OPTIONS, PROPFIND, …) is read-only and always
/// passes — dav-server itself rejects methods it does not implement.
fn method_allowed(method: &str, perm: Permission) -> bool {
    let needs_delete = method == "DELETE";
    let needs_write = matches!(
        method,
        "PUT" | "DELETE" | "MKCOL" | "MOVE" | "COPY" | "PROPPATCH" | "LOCK" | "UNLOCK"
    );
    (!needs_delete || perm.can_delete()) && (!needs_write || perm.can_write())
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

#[cfg(test)]
mod tests {
    use super::method_allowed;
    use crate::config::Permission;

    // The full method × permission table. Every WebDAV write method must be
    // listed here with `false` under read_only — a method missing from the
    // gate in `method_allowed` would show up as an unexpected `true`.
    #[test]
    fn method_gate_matches_the_permission_ladder() {
        // (method, allowed at: read_only, read_write, read_write_delete)
        let table = [
            // Read methods pass at every level.
            ("GET", true, true, true),
            ("HEAD", true, true, true),
            ("OPTIONS", true, true, true),
            ("PROPFIND", true, true, true),
            // Write methods need read_write.
            ("PUT", false, true, true),
            ("MKCOL", false, true, true),
            ("MOVE", false, true, true),
            ("COPY", false, true, true),
            ("PROPPATCH", false, true, true),
            ("LOCK", false, true, true),
            ("UNLOCK", false, true, true),
            // Delete needs the full read_write_delete level.
            ("DELETE", false, false, true),
        ];
        for (method, ro, rw, rwd) in table {
            assert_eq!(
                method_allowed(method, Permission::ReadOnly),
                ro,
                "{method} at read_only"
            );
            assert_eq!(
                method_allowed(method, Permission::ReadWrite),
                rw,
                "{method} at read_write"
            );
            assert_eq!(
                method_allowed(method, Permission::ReadWriteDelete),
                rwd,
                "{method} at read_write_delete"
            );
        }
    }
}
