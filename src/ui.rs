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
use tokio_stream::wrappers::BroadcastStream;

use crate::{auth::Session, AppState};

/// Landing page: the file manager if logged in, otherwise the login page.
pub async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if crate::auth::is_authed(&headers, &state.session) {
        Html(include_str!("web/app.html")).into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

pub async fn login_page() -> Html<&'static str> {
    Html(include_str!("web/login.html"))
}

/// Connection details for the Status tab (session-gated).
pub async fn info(_: Session, State(state): State<AppState>) -> Response {
    let info = &state.info;
    let ip = crate::current_ip();
    let screen = state
        .screen_status
        .lock()
        .map(|s| s.clone())
        .unwrap_or_else(|_| "unknown".to_string());
    Json(serde_json::json!({
        "ip": ip.to_string(),
        "port": info.port,
        "dav": format!("http://{}:{}{}", ip, info.port, crate::webdav::MOUNT),
        "root": state.root.to_string_lossy(),
        "screen": screen,
    }))
    .into_response()
}

/// Current settings (session-gated), read-only — for display in the UI.
/// Settings are owned by the config file; the UI never writes them.
pub async fn get_settings(_: Session, State(state): State<AppState>) -> Response {
    Json(&*state.settings).into_response()
}

/// Live SSE stream of input events for the Status tab.
pub async fn events(
    _: Session,
    State(state): State<AppState>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, axum::Error>>> {
    let stream = BroadcastStream::new(state.events.subscribe())
        .filter_map(|res| async move { res.ok().map(|ev| Event::default().json_data(ev)) });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
