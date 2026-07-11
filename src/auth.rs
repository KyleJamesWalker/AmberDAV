//! Session-cookie auth for the web UI. The password is shown on the device
//! screen (never in the browser); the user types it on the login page, and we
//! hand back an opaque session token cookie. The `/dav` WebDAV mount keeps its
//! own HTTP Basic auth for network-drive clients.

use std::time::Instant;

use axum::{
    extract::{FromRequestParts, Query, State},
    http::{header, request::Parts, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    Form,
};
use constant_time_eq::constant_time_eq;

use crate::{state::AppState, throttle, throttle::ClientIp};

const COOKIE: &str = "sid";

/// How long a login lasts: 24 hours. The token itself is per-boot, so a
/// restart logs everyone out regardless; within one boot, a browser that
/// logged in stays logged in this long — worth knowing on long-running
/// fixed-password deployments, where the server may not restart for weeks.
const SESSION_COOKIE_MAX_AGE_SECS: u32 = 86_400;

/// Extracted only when the request carries a valid session cookie; otherwise
/// rejects with 401 (for `/api/*` fetch calls).
pub struct Session;

impl FromRequestParts<AppState> for Session {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        if is_authed(&parts.headers, &state.session) {
            Ok(Session)
        } else {
            Err((StatusCode::UNAUTHORIZED, "authentication required").into_response())
        }
    }
}

/// True if the request's `sid` cookie matches the live session token.
/// Compared in constant time so the token can't be recovered byte-by-byte
/// through response timing (issue #27).
pub fn is_authed(headers: &HeaderMap, token: &str) -> bool {
    let header = headers.get(header::COOKIE).and_then(|v| v.to_str().ok());
    cookie_value(header, COOKIE).is_some_and(|v| constant_time_eq(v.as_bytes(), token.as_bytes()))
}

fn cookie_value(header: Option<&str>, name: &str) -> Option<String> {
    let header = header?;
    header.split(';').find_map(|part| {
        part.trim()
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|v| v.to_string())
    })
}

#[derive(serde::Deserialize)]
pub struct LoginForm {
    password: String,
}

/// The login page carries an optional `next` query param: the in-app URL the
/// browser was trying to reach when it was bounced to login. Honored after a
/// successful login so a deep link (or a folder open after a server restart)
/// lands where the user actually was, not back at Home (the SPA's `?path=`
/// folder routing).
#[derive(serde::Deserialize, Default)]
pub struct LoginQuery {
    next: Option<String>,
}

/// Percent-encode `s` for use as a query-string value: everything but the
/// RFC 3986 unreserved set becomes `%XX`. Small and dependency-free on purpose
/// (the size budget rules out pulling in a URL crate just for this); used to
/// stuff a whole in-app URL into the login page's `next` param.
pub fn encode_next(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Build the redirect target for an unauthenticated request to a gated page:
/// `/login`, carrying the original location as `next` when there is a query
/// string worth preserving (the SPA encodes the current folder there). A bare
/// `/` has nothing to carry, so it stays the plain `/login` (no `next`).
pub fn login_redirect(uri: &Uri) -> String {
    match uri.query() {
        Some(q) if !q.is_empty() => {
            format!(
                "/login?next={}",
                encode_next(&format!("{}?{}", uri.path(), q))
            )
        }
        _ => "/login".to_string(),
    }
}

/// Validate a `next` redirect target so login can never be turned into an open
/// redirect: only same-origin, root-relative paths are accepted. Protocol-
/// relative (`//evil.com`) and backslash-smuggled (`/\evil.com`) forms — which
/// browsers treat as absolute — are rejected, as is anything not starting `/`.
fn safe_redirect(next: Option<&str>) -> Option<&str> {
    let n = next?;
    (n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\")).then_some(n)
}

/// Handle the login form: set the session cookie on the right password.
///
/// Brute-force defenses (issue #27): the password check runs in constant time
/// so a guess can't be confirmed character-by-character through response
/// timing, and repeated failures from one IP are throttled with an
/// exponential backoff before the password is even looked at. The client IP
/// comes from the TCP peer address ([`ClientIp`]); requests without one
/// (router tests driving the service via `oneshot`) share a sentinel key.
pub async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Query(query): Query<LoginQuery>,
    Form(form): Form<LoginForm>,
) -> Response {
    let now = Instant::now();
    if let Some(wait) = state.throttle.retry_after(ip, now) {
        return throttle::too_many_attempts(wait);
    }

    if state.info.password.verify(&form.password) {
        state.throttle.record_success(ip);
        let cookie = format!(
            "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_COOKIE_MAX_AGE_SECS}",
            state.session
        );
        // Return to where the browser was headed (a deep-linked folder), or
        // Home when there was no — or an unsafe — `next`.
        let dest = safe_redirect(query.next.as_deref()).unwrap_or("/");
        ([(header::SET_COOKIE, cookie)], Redirect::to(dest)).into_response()
    } else {
        state.throttle.record_failure(ip, now);
        // Keep `next` across a wrong guess so the retry still lands on the
        // intended folder once the right code goes in.
        let mut loc = String::from("/login?e=1");
        if let Some(n) = safe_redirect(query.next.as_deref()) {
            loc.push_str("&next=");
            loc.push_str(&encode_next(n));
        }
        Redirect::to(&loc).into_response()
    }
}

/// Clear the session cookie.
pub async fn logout() -> Response {
    let cookie = format!("{COOKIE}=; Path=/; HttpOnly; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Redirect::to("/login")).into_response()
}

#[cfg(test)]
mod tests {
    use super::{cookie_value, encode_next, is_authed, login_redirect, safe_redirect};
    use axum::http::{header, HeaderMap, Uri};

    #[test]
    fn encode_next_escapes_everything_but_unreserved() {
        // Unreserved (RFC 3986) bytes pass through untouched…
        assert_eq!(encode_next("aZ09-._~"), "aZ09-._~");
        // …everything else (the URL delimiters, the percent itself) is escaped,
        // so a whole in-app URL survives intact as one query value.
        assert_eq!(
            encode_next("/?path=Roms%2FGameBoy"),
            "%2F%3Fpath%3DRoms%252FGameBoy"
        );
        assert_eq!(encode_next("a b&c"), "a%20b%26c");
    }

    #[test]
    fn safe_redirect_allows_only_local_paths() {
        // Root-relative in-app links are fine.
        assert_eq!(safe_redirect(Some("/?path=Roms")), Some("/?path=Roms"));
        assert_eq!(safe_redirect(Some("/")), Some("/"));
        // Open-redirect vectors are rejected: protocol-relative, backslash
        // smuggling, absolute URLs, and anything not rooted at `/`.
        assert_eq!(safe_redirect(Some("//evil.com")), None);
        assert_eq!(safe_redirect(Some("/\\evil.com")), None);
        assert_eq!(safe_redirect(Some("https://evil.com")), None);
        assert_eq!(safe_redirect(Some("evil.com")), None);
        assert_eq!(safe_redirect(None), None);
    }

    #[test]
    fn login_redirect_carries_only_a_real_query() {
        // A bare path has nothing worth preserving → plain /login.
        assert_eq!(login_redirect(&Uri::from_static("/")), "/login");
        // A deep link's query is percent-encoded into `next`.
        assert_eq!(
            login_redirect(&Uri::from_static("/?path=Roms%2FGameBoy")),
            "/login?next=%2F%3Fpath%3DRoms%252FGameBoy"
        );
    }

    // Behavioral check of the constant-time session compare: the exact token
    // passes, anything else (prefix, wrong value, different length, absent)
    // does not. Timing itself isn't testable here — only correctness is.
    #[test]
    fn is_authed_accepts_only_the_exact_token() {
        let with_cookie = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::COOKIE, format!("sid={v}").parse().unwrap());
            h
        };
        assert!(is_authed(&with_cookie("tok-123"), "tok-123"));
        assert!(!is_authed(&with_cookie("tok-124"), "tok-123"));
        assert!(!is_authed(&with_cookie("tok-12"), "tok-123"));
        assert!(!is_authed(&with_cookie("tok-1234"), "tok-123"));
        assert!(!is_authed(&with_cookie(""), "tok-123"));
        assert!(!is_authed(&HeaderMap::new(), "tok-123"));
    }

    #[test]
    fn parses_target_cookie() {
        assert_eq!(
            cookie_value(Some("a=1; sid=abc123; b=2"), "sid"),
            Some("abc123".to_string())
        );
        assert_eq!(cookie_value(Some("other=x"), "sid"), None);
        assert_eq!(cookie_value(None, "sid"), None);
        // Must not match a cookie whose name merely ends with the target.
        assert_eq!(cookie_value(Some("xsid=nope"), "sid"), None);
    }
}
