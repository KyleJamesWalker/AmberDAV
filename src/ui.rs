//! Web UI: login page, the file-manager SPA, the connection-info endpoint,
//! and the live-input SSE stream.

use axum::{
    extract::State,
    http::{HeaderMap, Uri},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Redirect, Response,
    },
    Json,
};
use futures_util::StreamExt;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use crate::{auth::Session, input::InputUpdate, state::AppState};

const APP_HTML: &str = include_str!("web/app.html");
const LOGIN_HTML: &str = include_str!("web/login.html");
const DENIED_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Access Denied</title>
  <style>
    body {
      background: #121214;
      color: #e4e4e7;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      height: 100vh;
      margin: 0;
    }
    .card {
      background: #1a1a1e;
      padding: 2.5rem;
      border-radius: 12px;
      box-shadow: 0 4px 20px rgba(0,0,0,0.4);
      text-align: center;
      max-width: 400px;
    }
    h1 {
      color: #ef4444;
      font-size: 1.8rem;
      margin-top: 0;
    }
    p {
      color: #a1a1aa;
      line-height: 1.5;
    }
  </style>
</head>
<body>
  <div class="card">
    <h1>Access Denied</h1>
    <p>You do not have permission to access AmberDAV. Please contact your system administrator.</p>
  </div>
</body>
</html>"#;
// The SPA's CSS and JS live in their own files so each concern diffs on its
// own and CI can lint app.js. Still embedded — no build step.
const APP_CSS: &str = include_str!("web/app.css");
const APP_JS: &str = include_str!("web/app.js");

/// Strong ETags for the embedded assets (issue #58): a content hash rather
/// than the version string, because a dev rebuild can change app.html
/// without changing the git-describe version (same dirty hash) — a
/// version-derived ETag would then serve a stale 304 during local dev.
/// Hashed once on first use; zero per-request cost.
static APP_ETAG: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| content_etag(APP_HTML));
static LOGIN_ETAG: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| content_etag(LOGIN_HTML));
static APP_CSS_ETAG: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| content_etag(APP_CSS));
static APP_JS_ETAG: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| content_etag(APP_JS));

fn content_etag(body: &str) -> String {
    use sha2::Digest;
    format!("\"{:x}\"", sha2::Sha256::digest(body.as_bytes()))
}

/// Whether an `If-None-Match` header matches `etag`. Ours are strong, but
/// clients may echo a `W/` prefix and may send a list or `*` — If-None-Match
/// uses the weak comparison (RFC 9110), so the prefix is ignored. Pure for
/// unit testing.
fn none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(str::trim)
                .any(|t| t == "*" || t.strip_prefix("W/").unwrap_or(t) == etag)
        })
}

/// Serve an embedded asset with its ETag and content type, answering a
/// matching `If-None-Match` with 304 — the assets are immutable per build, so
/// a revisit skips re-downloading the SPA over the device's slow link.
fn cached_asset(
    headers: &HeaderMap,
    etag: &'static str,
    content_type: &'static str,
    body: &'static str,
) -> Response {
    let hdrs = [
        (axum::http::header::ETAG, etag),
        (axum::http::header::CONTENT_TYPE, content_type),
    ];
    if none_match(headers, etag) {
        (axum::http::StatusCode::NOT_MODIFIED, hdrs).into_response()
    } else {
        (hdrs, body).into_response()
    }
}

/// Landing page: the file manager if logged in, otherwise the login page.
///
/// The redirect carries the original location (the SPA's `?path=` folder) as
/// `next` so a deep link or a post-restart refresh returns to the same folder
/// after the password is re-entered, instead of dropping back to Home.
pub async fn index(
    State(state): State<AppState>,
    crate::throttle::ClientIp(ip): crate::throttle::ClientIp,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let mut authed = crate::auth::is_authed(&headers, &state.session);
    let mut is_denied = false;

    if !authed && state.settings.proxy_auth.enabled {
        if let Some(user_hdr) = headers.get(&state.settings.proxy_auth.user_header) {
            let ip_str = ip.to_string();
            let is_trusted = state.settings.proxy_auth.trusted_proxies.is_empty()
                || state
                    .settings
                    .proxy_auth
                    .trusted_proxies
                    .iter()
                    .any(|p| p == &ip_str);

            if is_trusted {
                if let Ok(user) = user_hdr.to_str() {
                    if !user.is_empty() {
                        let groups = headers
                            .get(&state.settings.proxy_auth.groups_header)
                            .and_then(|g| g.to_str().ok())
                            .unwrap_or("");
                        let permission = crate::auth::determine_proxy_permission(
                            groups,
                            &state.settings.proxy_auth.group_permissions,
                            state.settings.proxy_auth.default_permission,
                        );
                        if permission == crate::config::Permission::None {
                            is_denied = true;
                        } else {
                            authed = true;
                        }
                    }
                }
            } else {
                tracing::warn!("Rejecting proxy auth index access from untrusted IP: {ip_str}");
            }
        }
    }

    if !authed && state.settings.permission == crate::config::Permission::None {
        is_denied = true;
    }

    if is_denied {
        (
            axum::http::StatusCode::FORBIDDEN,
            axum::response::Html(DENIED_HTML),
        )
            .into_response()
    } else if authed {
        cached_asset(&headers, &APP_ETAG, "text/html; charset=utf-8", APP_HTML)
    } else {
        if state.settings.proxy_auth.enabled {
            let header_keys: Vec<String> = headers.keys().map(|k| k.to_string()).collect();
            tracing::info!(
                "Redirecting to login. Incoming headers present: {:?}",
                header_keys
            );
        }
        Redirect::to(&crate::auth::login_redirect(&uri)).into_response()
    }
}

pub async fn login_page(
    State(state): State<AppState>,
    crate::throttle::ClientIp(ip): crate::throttle::ClientIp,
    axum::extract::Query(query): axum::extract::Query<crate::auth::LoginQuery>,
    headers: HeaderMap,
) -> Response {
    let mut authed = crate::auth::is_authed(&headers, &state.session);
    let mut is_denied = false;

    if !authed && state.settings.proxy_auth.enabled {
        if let Some(user_hdr) = headers.get(&state.settings.proxy_auth.user_header) {
            let ip_str = ip.to_string();
            let is_trusted = state.settings.proxy_auth.trusted_proxies.is_empty()
                || state
                    .settings
                    .proxy_auth
                    .trusted_proxies
                    .iter()
                    .any(|p| p == &ip_str);

            if is_trusted {
                if let Ok(user) = user_hdr.to_str() {
                    if !user.is_empty() {
                        let groups = headers
                            .get(&state.settings.proxy_auth.groups_header)
                            .and_then(|g| g.to_str().ok())
                            .unwrap_or("");
                        let permission = crate::auth::determine_proxy_permission(
                            groups,
                            &state.settings.proxy_auth.group_permissions,
                            state.settings.proxy_auth.default_permission,
                        );
                        if permission == crate::config::Permission::None {
                            is_denied = true;
                        } else {
                            authed = true;
                        }
                    }
                }
            }
        }
    }

    if !authed && state.settings.permission == crate::config::Permission::None {
        is_denied = true;
    }

    if is_denied {
        (
            axum::http::StatusCode::FORBIDDEN,
            axum::response::Html(DENIED_HTML),
        )
            .into_response()
    } else if authed {
        let dest = crate::auth::safe_redirect(query.next.as_deref()).unwrap_or("/");
        Redirect::to(dest).into_response()
    } else {
        cached_asset(
            &headers,
            &LOGIN_ETAG,
            "text/html; charset=utf-8",
            LOGIN_HTML,
        )
    }
}

/// The SPA's external stylesheet and script. Both are public — they hold no
/// secrets, mirror the always-public login page's needs, and the HTML that
/// references them is itself session-gated.
pub async fn app_css(headers: HeaderMap) -> Response {
    cached_asset(&headers, &APP_CSS_ETAG, "text/css; charset=utf-8", APP_CSS)
}

pub async fn app_js(headers: HeaderMap) -> Response {
    cached_asset(
        &headers,
        &APP_JS_ETAG,
        "text/javascript; charset=utf-8",
        APP_JS,
    )
}

/// Connection details for the Status tab (session-gated).
pub async fn info(session: Session, State(state): State<AppState>) -> Response {
    let info = &state.info;
    let ip = crate::state::current_ip();
    let screen = state
        .screen_status
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| "unknown".to_string());
    // For disk space: report the single root's filesystem; in multi-root mode
    // report the first mount's filesystem (UI shows the gauge for situational
    // awareness, not per-mount breakdown).
    let disk_path = state
        .mounts
        .single_root()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            state
                .mounts
                .mounts()
                .first()
                .map(|(_, p)| p.clone())
                .unwrap_or_default()
        });
    let disk = disk_space(&disk_path);
    // "root" field: single path string for single-root, or null for multi-root
    // (multi-root clients use the `/api/list` response to discover mounts).
    let root_display = state
        .mounts
        .single_root()
        .map(|p| p.to_string_lossy().into_owned());
    Json(serde_json::json!({
        "ip": ip.to_string(),
        "port": info.port,
        "dav": format!("http://{}:{}{}", ip, info.port, crate::webdav::MOUNT),
        "root": root_display,
        "screen": screen,
        "permission": session.permission,
        // Free/total bytes of the filesystem holding the served root — null
        // when unreportable, and the UI hides the gauge (issue #43).
        "disk_free": disk.map(|(free, _)| free),
        "disk_total": disk.map(|(_, total)| total),
        // Non-null when the config file was unusable and defaults are in
        // effect — the Status tab shows this loudly (issue #19).
        "config_error": info.config_error,
        // Where the config file actually lives on this build/platform — the
        // Settings help used to hardcode the device location ("next to the
        // binary"), wrong for desktop builds since the platform-dirs change
        // (issue #60). The same resolution main() loaded from: it consults
        // only the environment and platform dirs, neither changes at runtime.
        "config_path": crate::config::config_path().to_string_lossy(),
        "version": crate::version::VERSION,
        // Gamepad input is only read on device builds; elsewhere the live-input
        // stream never emits, so the UI hides that card (issue #15).
        "live_input": cfg!(device),
        // Device name for the browser tab title (issue #101). null = fall back
        // to the default subtitle "web access".
        "name": state.settings.name,
    }))
    .into_response()
}

/// Free/total bytes of the filesystem holding `path`, via `statvfs`. "Free"
/// is what an unprivileged writer can actually use (`f_bavail`, not
/// `f_bfree`, which counts root-reserved blocks). `None` when the call fails
/// or reports a zero-sized filesystem — the JSON fields become null and the
/// web UI degrades by hiding the gauge.
#[cfg(unix)]
fn disk_space(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` outlives the call and is NUL-terminated; `s` is a writable
    // out-param that statvfs fills only on the success (0) return.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    // The field widths differ per platform (u32 on macOS, u64 on Linux),
    // hence the casts clippy would otherwise flag as redundant on Linux.
    #[allow(clippy::unnecessary_cast)]
    let (frsize, bavail, blocks) = (s.f_frsize as u64, s.f_bavail as u64, s.f_blocks as u64);
    let free = bavail.saturating_mul(frsize);
    let total = blocks.saturating_mul(frsize);
    (total > 0).then_some((free, total))
}

/// No statvfs counterpart wired up on this platform (e.g. Windows): report
/// "unavailable" rather than pulling in a platform API dependency.
#[cfg(not(unix))]
fn disk_space(_path: &std::path::Path) -> Option<(u64, u64)> {
    None
}

/// Public device-name endpoint — no auth required so the login page can set its
/// tab title before the user logs in (issue #101). Returns only `{"name": ...}`;
/// nothing sensitive is exposed.
pub async fn public_name(State(state): State<AppState>) -> Response {
    Json(serde_json::json!({ "name": state.settings.name })).into_response()
}

/// Current settings (session-gated), read-only — for display in the UI.
/// Settings are owned by the config file; the UI never writes them.
///
/// The `password` value is redacted server-side: the login page and README
/// promise the password is never shown in the browser, and the Settings tab
/// only needs fixed-vs-random — so a fixed password is replaced with a masked
/// placeholder (still truthy for the UI) and a random one stays `null`
/// (issue #27 / review §2.21).
pub async fn get_settings(session: Session, State(state): State<AppState>) -> Response {
    let mut value = serde_json::to_value(&*state.settings).expect("settings serialize");
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "permission".to_string(),
            serde_json::to_value(session.permission).unwrap(),
        );
        let fixed = state
            .settings
            .password
            .as_deref()
            .is_some_and(|p| !p.is_empty());
        obj.insert(
            "password".to_string(),
            if fixed {
                serde_json::Value::from("(hidden)")
            } else {
                serde_json::Value::Null
            },
        );
        let has_hash = state
            .settings
            .password_hash
            .as_deref()
            .is_some_and(|h| !h.is_empty());
        obj.insert(
            "password_hash".to_string(),
            if has_hash {
                serde_json::Value::from("(hidden)")
            } else {
                serde_json::Value::Null
            },
        );
        // Authoritative multi-root flag (issue #76): the UI hides the write
        // affordances at the read-only virtual root. The raw root/roots
        // fields are pre-resolution and can't answer this.
        obj.insert(
            "multi_root".to_string(),
            serde_json::Value::from(!state.mounts.is_single()),
        );
    }
    Json(value).into_response()
}

/// Build the live-input SSE stream. The stream ends as soon as `shutdown`
/// fires, so an open Status page can't keep its connection alive and stall the
/// server's graceful shutdown (issue #15).
fn input_event_stream(
    rx: broadcast::Receiver<InputUpdate>,
    shutdown: CancellationToken,
) -> impl futures_util::Stream<Item = Result<Event, axum::Error>> {
    BroadcastStream::new(rx)
        .filter_map(|res| async move { res.ok().map(|ev| Event::default().json_data(ev)) })
        .take_until(async move { shutdown.cancelled().await })
}

/// Live SSE stream of input events for the Status tab.
pub async fn events(
    _: Session,
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, axum::Error>>> {
    Sse::new(input_event_stream(
        state.events.subscribe(),
        state.shutdown.clone(),
    ))
    .keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn h(v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(axum::http::header::IF_NONE_MATCH, v.parse().unwrap());
        m
    }

    // If-None-Match matching (issue #58): exact tag, weak-prefixed echo,
    // lists, and `*` all match; other tags and a missing header don't.
    #[test]
    fn if_none_match_comparison() {
        let etag = "\"abc123\"";
        assert!(none_match(&h("\"abc123\""), etag));
        assert!(none_match(&h("W/\"abc123\""), etag), "weak echo matches");
        assert!(none_match(&h("\"zzz\", \"abc123\""), etag), "list matches");
        assert!(none_match(&h("*"), etag));
        assert!(!none_match(&h("\"other\""), etag));
        assert!(!none_match(&HeaderMap::new(), etag), "no header, no match");
    }

    // The embedded pages hash to distinct, quoted, stable tags.
    #[test]
    fn embedded_etags_are_quoted_and_distinct() {
        assert!(APP_ETAG.starts_with('"') && APP_ETAG.ends_with('"'));
        assert_ne!(*APP_ETAG, *LOGIN_ETAG);
        assert_eq!(*APP_ETAG, content_etag(APP_HTML), "stable across calls");
    }

    // The shim's invariants on a real filesystem: positive total, free never
    // exceeding it. (Runs on every unix host; non-unix returns None and is
    // covered by the missing-path case below behaving identically.)
    #[cfg(unix)]
    #[test]
    fn disk_space_reports_plausible_numbers() {
        let (free, total) =
            disk_space(std::path::Path::new("/")).expect("statvfs on / should succeed");
        assert!(total > 0, "total bytes must be positive");
        assert!(free <= total, "free ({free}) cannot exceed total ({total})");
    }

    // A path that doesn't exist must yield None (→ null JSON fields), never
    // an error response from /api/info.
    #[test]
    fn disk_space_on_a_missing_path_is_none() {
        let p = std::path::Path::new("/definitely/not/a/real/amberdav/path");
        assert_eq!(disk_space(p), None);
    }

    fn sample() -> InputUpdate {
        InputUpdate {
            device: "test".to_string(),
            kind: "button",
            name: "BTN_SOUTH".to_string(),
            code: 304,
            value: 1,
            state: "down",
        }
    }

    /// The live-input stream must terminate once shutdown is signaled; otherwise
    /// an open Status page holds the connection open and graceful shutdown hangs
    /// forever (issue #15). A 1s timeout turns a hang into a clean failure.
    #[tokio::test]
    async fn input_stream_ends_when_shutdown_is_signaled() {
        let (tx, _) = broadcast::channel(8);
        let shutdown = CancellationToken::new();
        let mut stream = Box::pin(input_event_stream(tx.subscribe(), shutdown.clone()));

        tx.send(sample()).unwrap();
        let first = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("stream should yield the broadcast event");
        assert!(first.is_some(), "expected the broadcast event");

        shutdown.cancel();
        let ended = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("stream must terminate after shutdown, but it hung");
        assert!(ended.is_none(), "stream should end (None) after shutdown");
    }
}
