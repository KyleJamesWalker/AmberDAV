//! Web UI: login page, the file-manager SPA, the connection-info endpoint,
//! and the live-input SSE stream.

use axum::{
    extract::State,
    http::HeaderMap,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Redirect, Response,
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

/// Strong ETags for the embedded pages (issue #58): a content hash rather
/// than the version string, because a dev rebuild can change app.html
/// without changing the git-describe version (same dirty hash) — a
/// version-derived ETag would then serve a stale 304 during local dev.
/// Hashed once on first use; zero per-request cost.
static APP_ETAG: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| content_etag(APP_HTML));
static LOGIN_ETAG: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| content_etag(LOGIN_HTML));

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

/// Serve an embedded page with its ETag, answering a matching
/// `If-None-Match` with 304 — the pages are immutable per build, so a
/// revisit skips re-downloading the SPA over the device's slow link.
fn cached_page(headers: &HeaderMap, etag: &'static str, body: &'static str) -> Response {
    let tag = [(axum::http::header::ETAG, etag)];
    if none_match(headers, etag) {
        (axum::http::StatusCode::NOT_MODIFIED, tag).into_response()
    } else {
        (tag, Html(body)).into_response()
    }
}

/// Landing page: the file manager if logged in, otherwise the login page.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if crate::auth::is_authed(&headers, &state.session) {
        cached_page(&headers, &APP_ETAG, APP_HTML)
    } else {
        Redirect::to("/login").into_response()
    }
}

pub async fn login_page(headers: HeaderMap) -> Response {
    cached_page(&headers, &LOGIN_ETAG, LOGIN_HTML)
}

/// Connection details for the Status tab (session-gated).
pub async fn info(_: Session, State(state): State<AppState>) -> Response {
    let info = &state.info;
    let ip = crate::state::current_ip();
    let screen = state
        .screen_status
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| "unknown".to_string());
    let disk = disk_space(&state.root);
    Json(serde_json::json!({
        "ip": ip.to_string(),
        "port": info.port,
        "dav": format!("http://{}:{}{}", ip, info.port, crate::webdav::MOUNT),
        "root": state.root.to_string_lossy(),
        "screen": screen,
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
        "live_input": cfg!(any(feature = "fb", feature = "sdl")),
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

/// Current settings (session-gated), read-only — for display in the UI.
/// Settings are owned by the config file; the UI never writes them.
///
/// The `password` value is redacted server-side: the login page and README
/// promise the password is never shown in the browser, and the Settings tab
/// only needs fixed-vs-random — so a fixed password is replaced with a masked
/// placeholder (still truthy for the UI) and a random one stays `null`
/// (issue #27 / review §2.21).
pub async fn get_settings(_: Session, State(state): State<AppState>) -> Response {
    let mut value = serde_json::to_value(&*state.settings).expect("settings serialize");
    if let Some(obj) = value.as_object_mut() {
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
