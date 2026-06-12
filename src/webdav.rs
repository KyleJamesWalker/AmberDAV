//! WebDAV serving via `dav-server`, bridged into axum and gated behind
//! HTTP Basic auth using the per-boot password.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use base64::Engine;
use dav_server::{fakels::FakeLs, localfs::LocalFs, DavHandler};

use crate::{config::Permission, state::SharedSettings, throttle, throttle::Throttle};

/// Path prefix the WebDAV tree is mounted under.
pub const MOUNT: &str = "/dav";

/// Shared state for the WebDAV route: the handler, the boot password, and the
/// live settings (for permission enforcement on write methods).
#[derive(Clone)]
pub struct DavState {
    pub handler: DavHandler,
    pub password: Arc<str>,
    pub settings: SharedSettings,
    /// Per-IP auth-failure throttle, shared with the web login (one guess
    /// budget per client across both password surfaces — issue #27).
    pub throttle: Arc<Throttle>,
}

/// Build a read/write WebDAV handler serving `root`, mounted at [`MOUNT`].
///
/// The `LocalFs` platform flags are compile-time: on Windows hosts
/// `case_insensitive` enables dav-server's cached case-insensitive lookups
/// (the WebClient mini-redirector case-normalizes paths, which would
/// otherwise miss on exact-case filesystems), and on macOS hosts `macos`
/// enables its Finder optimizations (`._*` PROPSTAT caching and friends).
/// Both are `false` elsewhere — notably on the Linux device builds, which
/// keep the zero-overhead exact-match path.
pub fn build_handler(root: &str) -> DavHandler {
    DavHandler::builder()
        .filesystem(LocalFs::new(
            root,
            false,
            cfg!(windows),
            cfg!(target_os = "macos"),
        ))
        .locksystem(FakeLs::new())
        .strip_prefix(MOUNT)
        // Render directory listings in a browser, so `/dav` is browsable too.
        .autoindex(true)
        .build_handler()
}

/// axum handler: authenticate, enforce permission, then hand off to dav-server.
///
/// Brute-force throttling (issue #27): wrong credentials count against the
/// per-IP failure budget shared with the web login, and a throttled IP gets
/// `429` before the password is even examined. A request with *no*
/// credentials never counts — the 401 challenge is the normal first
/// round-trip of every WebDAV client, not a guess.
pub async fn route(State(state): State<DavState>, req: Request) -> Response {
    // The peer address comes from `into_make_service_with_connect_info`; only
    // socket-less test harnesses lack it (they share the sentinel key).
    let ip = throttle::client_ip(
        req.extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0),
    );
    let now = Instant::now();
    if let Some(wait) = state.throttle.retry_after(ip, now) {
        return throttle::too_many_attempts(wait);
    }

    match check_auth(&req, &state.password) {
        BasicAuth::Ok => state.throttle.record_success(ip),
        outcome @ (BasicAuth::Missing | BasicAuth::Wrong) => {
            if outcome == BasicAuth::Wrong {
                state.throttle.record_failure(ip, now);
            }
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, r#"Basic realm="amber-dav""#)],
                "Authentication required\n",
            )
                .into_response();
        }
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

/// Outcome of the Basic-auth check, split three ways so the throttle can tell
/// a password *guess* apart from the normal credential-less first request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BasicAuth {
    /// Correct password (username ignored).
    Ok,
    /// No `Authorization` header at all — every WebDAV client starts here to
    /// collect the 401 challenge; not a guessing attempt.
    Missing,
    /// Credentials were presented but are wrong (or unparseable) — a guess.
    Wrong,
}

/// Check `Authorization: Basic ...` against the password (username ignored).
/// The password comparison runs in constant time so a guess can't be confirmed
/// character-by-character through response timing (issue #27).
fn check_auth(req: &Request, password: &str) -> BasicAuth {
    let Some(value) = req.headers().get(header::AUTHORIZATION) else {
        return BasicAuth::Missing;
    };

    let Some(encoded) = value.to_str().ok().and_then(|v| v.strip_prefix("Basic ")) else {
        return BasicAuth::Wrong;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
        return BasicAuth::Wrong;
    };
    let Ok(text) = String::from_utf8(decoded) else {
        return BasicAuth::Wrong;
    };

    match text.split_once(':') {
        Some((_, pass))
            if constant_time_eq::constant_time_eq(pass.as_bytes(), password.as_bytes()) =>
        {
            BasicAuth::Ok
        }
        _ => BasicAuth::Wrong,
    }
}

#[cfg(test)]
mod tests {
    use super::{check_auth, method_allowed, BasicAuth};
    use crate::config::Permission;
    use axum::{body::Body, extract::Request, http::header};
    use base64::Engine;

    fn req_with_basic(creds: Option<&str>) -> Request {
        let mut b = Request::builder().uri("/dav/");
        if let Some(c) = creds {
            let encoded = base64::engine::general_purpose::STANDARD.encode(c);
            b = b.header(header::AUTHORIZATION, format!("Basic {encoded}"));
        }
        b.body(Body::empty()).unwrap()
    }

    // Behavioral check of the constant-time Basic-auth compare: only the exact
    // password passes (any username); wrong values, prefixes, and extensions
    // are Wrong (a guess); only a missing header is Missing (the normal
    // challenge round-trip, which must never count against the throttle).
    #[test]
    fn basic_auth_accepts_only_the_exact_password() {
        let check = |creds| check_auth(&req_with_basic(creds), "secret");
        assert_eq!(check(Some("user:secret")), BasicAuth::Ok);
        assert_eq!(check(Some(":secret")), BasicAuth::Ok);
        assert_eq!(check(Some("user:secres")), BasicAuth::Wrong);
        assert_eq!(check(Some("user:secre")), BasicAuth::Wrong);
        assert_eq!(check(Some("user:secrets")), BasicAuth::Wrong);
        assert_eq!(check(Some("user:")), BasicAuth::Wrong);
        assert_eq!(check(Some("nocolon")), BasicAuth::Wrong);
        assert_eq!(check(None), BasicAuth::Missing);

        // Unparseable credentials are a Wrong (counted) attempt, not Missing.
        let garbled = Request::builder()
            .uri("/dav/")
            .header(header::AUTHORIZATION, "Basic not!base64@@")
            .body(Body::empty())
            .unwrap();
        assert_eq!(check_auth(&garbled, "secret"), BasicAuth::Wrong);
    }

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
