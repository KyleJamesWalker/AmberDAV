//! Session-cookie auth for the web UI. The password is shown on the device
//! screen (never in the browser); the user types it on the login page, and we
//! hand back an opaque session token cookie. The `/dav` WebDAV mount keeps its
//! own HTTP Basic auth for network-drive clients.

use axum::{
    extract::{FromRequestParts, State},
    http::{header, request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Form,
};
use constant_time_eq::constant_time_eq;

use crate::state::AppState;

const COOKIE: &str = "sid";

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

/// Handle the login form: set the session cookie on the right password.
/// The password check runs in constant time so a guess can't be confirmed
/// character-by-character through response timing (issue #27).
pub async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    if constant_time_eq(form.password.as_bytes(), state.info.password.as_bytes()) {
        let cookie = format!(
            "{COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=86400",
            state.session
        );
        ([(header::SET_COOKIE, cookie)], Redirect::to("/")).into_response()
    } else {
        Redirect::to("/login?e=1").into_response()
    }
}

/// Clear the session cookie.
pub async fn logout() -> Response {
    let cookie = format!("{COOKIE}=; Path=/; HttpOnly; Max-Age=0");
    ([(header::SET_COOKIE, cookie)], Redirect::to("/login")).into_response()
}

#[cfg(test)]
mod tests {
    use super::{cookie_value, is_authed};
    use axum::http::{header, HeaderMap};

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
