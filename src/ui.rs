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
    let ip = crate::state::current_ip();
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
        // Non-null when the config file was unusable and defaults are in
        // effect — the Status tab shows this loudly (issue #19).
        "config_error": info.config_error,
        "version": env!("CARGO_PKG_VERSION"),
        // Gamepad input is only read on device builds; elsewhere the live-input
        // stream never emits, so the UI hides that card (issue #15).
        "live_input": cfg!(any(feature = "fb", feature = "sdl")),
    }))
    .into_response()
}

/// Current settings (session-gated), read-only — for display in the UI.
/// Settings are owned by the config file; the UI never writes them.
pub async fn get_settings(_: Session, State(state): State<AppState>) -> Response {
    Json(&*state.settings).into_response()
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
