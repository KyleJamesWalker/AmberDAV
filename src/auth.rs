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
pub fn is_authed(headers: &HeaderMap, token: &str) -> bool {
    let header = headers.get(header::COOKIE).and_then(|v| v.to_str().ok());
    cookie_value(header, COOKIE).is_some_and(|v| v == token)
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
pub async fn login(State(state): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    if form.password == state.info.password {
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
    use super::cookie_value;

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
