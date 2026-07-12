//! WebDAV serving via `dav-server`, bridged into axum and gated behind
//! HTTP Basic auth using the per-boot password.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use base64::Engine;
use dav_server::{fakels::FakeLs, localfs::LocalFs, DavHandler};

use crate::{config::Permission, state::SharedSettings, throttle, throttle::Throttle};

/// Path prefix the WebDAV tree is mounted under.
pub const MOUNT: &str = "/dav";

/// The filesystem layer exposed to dav-server: either a single `LocalFs` for
/// single-root mode or a collection of per-mount handlers for multi-root mode.
#[derive(Clone)]
pub enum DavFs {
    /// Single root: one `DavHandler` with `strip_prefix(MOUNT)`.
    Single(DavHandler),
    /// Multi-root: each mount has its own handler with
    /// `strip_prefix("{MOUNT}/{name}")`. The virtual root is synthesized.
    Multi(Arc<Vec<(String, DavHandler)>>),
}

/// Shared state for the WebDAV route: the filesystem, the boot password, and
/// the live settings (for permission enforcement on write methods).
#[derive(Clone)]
pub struct DavState {
    pub fs: DavFs,
    pub password: Arc<crate::password::PasswordMatcher>,
    pub settings: SharedSettings,
    /// Per-IP auth-failure throttle, shared with the web login (one guess
    /// budget per client across both password surfaces — issue #27).
    pub throttle: Arc<Throttle>,
}

/// Build a single-root WebDAV handler serving `root`, stripped at [`MOUNT`].
///
/// The `LocalFs` platform flags are compile-time: on Windows hosts
/// `case_insensitive` enables dav-server's cached case-insensitive lookups
/// (the WebClient mini-redirector case-normalizes paths, which would
/// otherwise miss on exact-case filesystems), and on macOS hosts `macos`
/// enables its Finder optimizations (`._*` PROPSTAT caching and friends).
/// Both are `false` elsewhere — notably on the Linux device builds, which
/// keep the zero-overhead exact-match path.
pub fn build_single_handler(root: &str) -> DavHandler {
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

/// Build a per-mount WebDAV handler for one named mount in multi-root mode.
/// The handler strips `{MOUNT}/{mount_name}` so dav-server sees paths
/// relative to the mount root.
pub fn build_mount_handler(mount_name: &str, root: &str) -> DavHandler {
    DavHandler::builder()
        .filesystem(LocalFs::new(
            root,
            false,
            cfg!(windows),
            cfg!(target_os = "macos"),
        ))
        .locksystem(FakeLs::new())
        .strip_prefix(format!("{MOUNT}/{mount_name}"))
        .autoindex(true)
        .build_handler()
}

/// Back-compat alias for single-root callers in tests.
#[doc(hidden)]
#[cfg_attr(not(test), allow(dead_code))]
pub fn build_handler(root: &str) -> DavHandler {
    build_single_handler(root)
}

/// Build a `DavFs::Multi` from an ordered list of `(mount_name, root_path)`.
/// Called from `main` where the private `DavHandler` type is not visible.
pub fn build_multi_fs(mounts: &[(String, std::path::PathBuf)]) -> DavFs {
    let handlers: Vec<(String, DavHandler)> = mounts
        .iter()
        .map(|(name, path)| {
            (
                name.clone(),
                build_mount_handler(name, &path.to_string_lossy()),
            )
        })
        .collect();
    DavFs::Multi(Arc::new(handlers))
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

    let mut permission = state.settings.permission;
    let mut authed_by_proxy = false;

    if state.settings.proxy_auth.enabled {
        if let Some(user_hdr) = req.headers().get(&state.settings.proxy_auth.user_header) {
            let ip_str = ip.to_string();
            let is_trusted = state.settings.proxy_auth.trusted_proxies.is_empty()
                || state
                    .settings
                    .proxy_auth
                    .trusted_proxies
                    .iter()
                    .any(|p| p == &ip_str);

            if !is_trusted {
                tracing::warn!("Rejecting proxy auth from untrusted IP: {ip_str}");
                return (StatusCode::UNAUTHORIZED, "untrusted proxy IP\n").into_response();
            }

            if let Ok(user) = user_hdr.to_str() {
                if !user.is_empty() {
                    let groups = req
                        .headers()
                        .get(&state.settings.proxy_auth.groups_header)
                        .and_then(|g| g.to_str().ok())
                        .unwrap_or("");
                    permission = crate::auth::determine_proxy_permission(
                        groups,
                        &state.settings.proxy_auth.group_permissions,
                        state.settings.permission,
                    );
                    authed_by_proxy = true;
                }
            }
        }
    }

    if !authed_by_proxy {
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
    }

    // Enforce the permission level on WebDAV write/delete methods, matching the
    // JSON API (otherwise read-only could be bypassed via the mount).
    if !method_allowed(req.method().as_str(), permission) {
        return (StatusCode::FORBIDDEN, "operation not permitted\n").into_response();
    }

    match &state.fs {
        DavFs::Single(handler) => {
            // axum's body implements http_body::Body<Data = Bytes>, which is
            // exactly what `handle` wants; the response body just gets
            // re-wrapped for axum.
            let (parts, body) = handler.handle(req).await.into_parts();
            Response::from_parts(parts, Body::new(body))
        }
        DavFs::Multi(mounts) => dispatch_multi(mounts, req).await,
    }
}

/// Percent-decode one URL path segment. `None` for malformed escapes or
/// non-UTF-8 bytes — no mount can have such a name.
fn percent_decode(seg: &str) -> Option<String> {
    let bytes = seg.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Percent-encode a mount name for use in an href: everything but the
/// RFC 3986 unreserved set is escaped (also making the result XML-safe).
fn percent_encode(name: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(name.len());
    for &b in name.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Minimal XML text escaping for element content (displayname).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Where a MOVE/COPY `Destination` header points, relative to the mount
/// handling the request.
#[derive(Debug, PartialEq)]
enum DestCheck {
    SameMount,
    VirtualRoot,
    OtherMount,
    Invalid,
}

/// Classify a `Destination` header (absolute URL or absolute path, per
/// RFC 4918) against the mount the request was routed to.
///
/// The per-mount handlers strip their prefix from the destination with a
/// byte-wise `starts_with` inside dav-server, so a destination in a sibling
/// mount whose name merely *extends* this one (`doc` vs `docs`) would
/// otherwise be stripped to a garbage subpath and land inside the wrong
/// mount. Only `SameMount` destinations may be forwarded; cross-mount
/// transfers exist in the JSON file API, not over WebDAV.
fn classify_destination(dest: &str, mount_name: &str) -> DestCheck {
    // Reduce an absolute URL to its path.
    let path = match dest.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(i) => &rest[i..],
            None => "/",
        },
        None => dest,
    };
    let Some(after_dav) = path.strip_prefix(MOUNT) else {
        return DestCheck::Invalid; // outside the DAV tree (or relative)
    };
    if !after_dav.is_empty() && !after_dav.starts_with('/') {
        return DestCheck::Invalid; // e.g. /davx — a different route entirely
    }
    let first = after_dav
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
    if first.is_empty() {
        return DestCheck::VirtualRoot;
    }
    match percent_decode(first) {
        Some(name) if name == mount_name => DestCheck::SameMount,
        Some(_) => DestCheck::OtherMount,
        None => DestCheck::Invalid,
    }
}

/// Dispatch a WebDAV request in multi-root mode.
///
/// - `PROPFIND` / `OPTIONS` at the virtual root → synthesized response.
/// - Write methods at the virtual root → `403 Forbidden`.
/// - `MOVE`/`COPY` whose `Destination` leaves the source mount → refused
///   (see [`classify_destination`]).
/// - Anything else → route to the appropriate per-mount handler.
async fn dispatch_multi(mounts: &[(String, DavHandler)], req: Request) -> Response {
    // Path segment immediately after MOUNT: either empty (virtual root) or a
    // mount name possibly followed by further path components. The URI path
    // is still percent-encoded; mount names are stored decoded.
    let path = req.uri().path();
    let after_dav = path.strip_prefix(MOUNT).unwrap_or(path);
    let after_dav = after_dav.trim_start_matches('/');
    let mount_name = match percent_decode(after_dav.split('/').next().unwrap_or("")) {
        Some(name) => name,
        None => return (StatusCode::BAD_REQUEST, "malformed path encoding\n").into_response(),
    };

    if mount_name.is_empty() {
        // Request is at the virtual root (/dav or /dav/).
        let method = req.method().as_str();
        return match method {
            "PROPFIND" => virtual_root_propfind(mounts, req.headers()),
            "OPTIONS" => virtual_root_options(),
            // Writes at the virtual root are always refused.
            _ if !matches!(method, "GET" | "HEAD") => {
                (StatusCode::FORBIDDEN, "cannot write to the virtual root\n").into_response()
            }
            _ => virtual_root_options(), // GET/HEAD: return OPTIONS-like response
        };
    }

    // MOVE/COPY may only stay inside the mount; classify before forwarding so
    // a cross-mount Destination can never reach a handler that would
    // mis-strip it (a missing header is left to dav-server's own 400).
    if matches!(req.method().as_str(), "MOVE" | "COPY") {
        if let Some(dest) = req
            .headers()
            .get("Destination")
            .and_then(|v| v.to_str().ok())
        {
            match classify_destination(dest, &mount_name) {
                DestCheck::SameMount => {}
                DestCheck::VirtualRoot => {
                    return (StatusCode::FORBIDDEN, "cannot write to the virtual root\n")
                        .into_response()
                }
                DestCheck::OtherMount => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        "cross-mount MOVE/COPY is not supported over WebDAV; \
                         use the web file manager\n",
                    )
                        .into_response()
                }
                DestCheck::Invalid => {
                    return (StatusCode::BAD_GATEWAY, "invalid Destination\n").into_response()
                }
            }
        }
    }

    // Find the matching per-mount handler and forward.
    if let Some((_, handler)) = mounts.iter().find(|(n, _)| *n == mount_name) {
        let (parts, body) = handler.handle(req).await.into_parts();
        Response::from_parts(parts, Body::new(body))
    } else {
        (StatusCode::NOT_FOUND, "unknown mount\n").into_response()
    }
}

/// Synthesize a `207 Multi-Status` PROPFIND response for the virtual root,
/// listing each mount as a `DAV:collection`. Handles `Depth: 0` (root only)
/// and `Depth: 1` (root + mounts); `Depth: infinity` is forbidden.
fn virtual_root_propfind(mounts: &[(String, DavHandler)], headers: &HeaderMap) -> Response {
    let depth = headers
        .get("Depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1");

    if depth == "infinity" {
        return (
            StatusCode::FORBIDDEN,
            "Depth: infinity is not supported on the virtual root\n",
        )
            .into_response();
    }

    let root_href = format!("{MOUNT}/");
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <multistatus xmlns=\"DAV:\">\
         <response>\
         <href>",
    );
    xml.push_str(&root_href);
    xml.push_str(
        "</href>\
         <propstat><prop><displayname/>\
         <resourcetype><collection/></resourcetype>\
         </prop><status>HTTP/1.1 200 OK</status></propstat>\
         </response>",
    );

    if depth != "0" {
        for (name, _) in mounts {
            // Percent-encode the href (its output is XML-safe by
            // construction) and XML-escape the human-readable displayname.
            let href = format!("{MOUNT}/{}/", percent_encode(name));
            xml.push_str("<response><href>");
            xml.push_str(&href);
            xml.push_str("</href><propstat><prop><displayname>");
            xml.push_str(&xml_escape(name));
            xml.push_str(
                "</displayname>\
                 <resourcetype><collection/></resourcetype>\
                 </prop><status>HTTP/1.1 200 OK</status></propstat></response>",
            );
        }
    }

    xml.push_str("</multistatus>");

    (
        StatusCode::MULTI_STATUS,
        [(header::CONTENT_TYPE, "application/xml; charset=\"utf-8\"")],
        xml,
    )
        .into_response()
}

/// Minimal `OPTIONS` response for the virtual root: advertise WebDAV class 1
/// and the allowed read methods.
fn virtual_root_options() -> Response {
    (
        StatusCode::OK,
        [
            ("DAV", "1"),
            ("Allow", "OPTIONS, PROPFIND"),
            ("MS-Author-Via", "DAV"),
        ],
        "",
    )
        .into_response()
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
fn check_auth(req: &Request, password: &crate::password::PasswordMatcher) -> BasicAuth {
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
        Some((_, pass)) if password.verify(pass) => BasicAuth::Ok,
        _ => BasicAuth::Wrong,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        check_auth, classify_destination, method_allowed, percent_decode, percent_encode,
        xml_escape, BasicAuth, DestCheck,
    };
    use crate::config::Permission;
    use axum::{body::Body, extract::Request, http::header};
    use base64::Engine;

    // End-to-end through dispatch_multi: an encoded mount segment must reach
    // its handler (not 404 as "unknown mount"), and a MOVE whose Destination
    // names a sibling mount must be refused before dav-server can mis-strip
    // the prefix ("doc" vs "docs").
    #[tokio::test]
    async fn dispatch_decodes_segments_and_gates_destinations() {
        let dir = std::env::temp_dir().join(format!(
            "amberdav-webdav-dispatch-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let root = dir.to_string_lossy();
        let mounts = vec![
            (
                "my files".to_string(),
                super::build_mount_handler("my files", &root),
            ),
            ("doc".to_string(), super::build_mount_handler("doc", &root)),
        ];

        let propfind = Request::builder()
            .method("PROPFIND")
            .uri("/dav/my%20files/")
            .header("Depth", "0")
            .body(Body::empty())
            .unwrap();
        let resp = super::dispatch_multi(&mounts, propfind).await;
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::MULTI_STATUS,
            "encoded mount segment must route to its handler"
        );

        let cross = Request::builder()
            .method("MOVE")
            .uri("/dav/doc/f.txt")
            .header("Destination", "http://h/dav/docs/g.txt")
            .body(Body::empty())
            .unwrap();
        let resp = super::dispatch_multi(&mounts, cross).await;
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_GATEWAY,
            "prefix-sibling destination must be refused, not forwarded"
        );

        let to_root = Request::builder()
            .method("COPY")
            .uri("/dav/doc/f.txt")
            .header("Destination", "/dav/")
            .body(Body::empty())
            .unwrap();
        let resp = super::dispatch_multi(&mounts, to_root).await;
        assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // The dispatch layer sees the raw (still percent-encoded) URI path while
    // mount names are configured decoded — the segment must be decoded before
    // the lookup, and names must be re-encoded when emitted into hrefs.
    #[test]
    fn percent_codec_round_trips_mount_names() {
        assert_eq!(percent_decode("my%20files").as_deref(), Some("my files"));
        assert_eq!(percent_decode("one").as_deref(), Some("one"));
        assert_eq!(percent_decode("na%C3%AFve").as_deref(), Some("naïve"));
        assert_eq!(percent_decode("100%25").as_deref(), Some("100%"));
        // Malformed escapes and non-UTF-8 can't name a mount.
        assert_eq!(percent_decode("bad%zz"), None);
        assert_eq!(percent_decode("trunc%2"), None);
        assert_eq!(percent_decode("%FF"), None);

        assert_eq!(percent_encode("my files"), "my%20files");
        assert_eq!(percent_encode("a&b"), "a%26b");
        assert_eq!(percent_encode("plain-1._~"), "plain-1._~");
        // encode → decode is the identity.
        assert_eq!(
            percent_decode(&percent_encode("naïve & spaced")).as_deref(),
            Some("naïve & spaced")
        );
    }

    // Mount names land inside PROPFIND XML; `&`/`<`/`>` must not produce a
    // document DAV clients fail to parse.
    #[test]
    fn xml_escape_covers_markup_characters() {
        assert_eq!(xml_escape("a&b <c>"), "a&amp;b &lt;c&gt;");
        assert_eq!(xml_escape("plain"), "plain");
    }

    // dav-server strips each handler's prefix from the Destination with a
    // byte-wise starts_with, so a destination in a sibling mount whose name
    // extends this one ("doc" vs "docs") would silently land INSIDE this
    // mount — the dispatch layer must classify destinations itself and only
    // forward same-mount transfers (issue #76 review).
    #[test]
    fn classify_destination_keeps_transfers_inside_the_mount() {
        let c = classify_destination;
        // Same mount: full URL or bare path, encoded or not.
        assert_eq!(
            c("http://h:8080/dav/one/x.txt", "one"),
            DestCheck::SameMount
        );
        assert_eq!(c("/dav/one/sub/", "one"), DestCheck::SameMount);
        assert_eq!(c("/dav/my%20files/x", "my files"), DestCheck::SameMount);
        // The prefix-sibling trap: "docs" extends "doc".
        assert_eq!(c("/dav/docs/g", "doc"), DestCheck::OtherMount);
        assert_eq!(c("https://h/dav/two/x", "one"), DestCheck::OtherMount);
        // The virtual root is never a write destination.
        assert_eq!(c("http://h/dav/", "one"), DestCheck::VirtualRoot);
        assert_eq!(c("/dav", "one"), DestCheck::VirtualRoot);
        // Outside the DAV tree entirely, or unparseable: invalid.
        assert_eq!(c("/elsewhere/x", "one"), DestCheck::Invalid);
        assert_eq!(c("/davx/y", "one"), DestCheck::Invalid);
        assert_eq!(c("not a url", "one"), DestCheck::Invalid);
        assert_eq!(c("/dav/bad%zz/x", "one"), DestCheck::Invalid);
    }

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
        let matcher = crate::password::PasswordMatcher::Plain("secret".to_string());
        let check = |creds| check_auth(&req_with_basic(creds), &matcher);
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
        assert_eq!(check_auth(&garbled, &matcher), BasicAuth::Wrong);
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
