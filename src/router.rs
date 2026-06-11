//! HTTP routing: every route the server exposes, wired to its handler.
//!
//! Extracted from `main()` so the full application — session auth, permission
//! enforcement, traversal rejection, the WebDAV mount — can be driven in
//! tests through `tower::ServiceExt::oneshot` without binding a socket.

use axum::{
    routing::{any, get, post, put},
    Router,
};

use crate::{auth, files, state::AppState, ui, update, webdav};

/// Build the complete application router over `state`.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui::index))
        .route("/login", get(ui::login_page).post(auth::login))
        .route("/logout", get(auth::logout))
        .route("/events", get(ui::events))
        .route("/api/info", get(ui::info))
        .route("/api/list", get(files::list))
        .route("/api/download", get(files::download))
        .route("/api/zip", get(files::zip))
        .route("/api/raw", get(files::raw))
        .route("/api/thumb", get(files::thumb))
        .route("/api/upload", put(files::upload))
        .route("/api/mkdir", post(files::mkdir))
        .route("/api/delete", post(files::delete))
        .route("/api/rename", post(files::rename))
        .route("/api/move", post(files::move_))
        .route("/api/copy", post(files::copy))
        .route("/api/settings", get(ui::get_settings))
        .route("/api/update/check", get(update::check))
        .route("/api/update/apply", post(update::apply))
        // `any` routes every method — including WebDAV's PROPFIND/MKCOL/etc.
        // The wildcard matches one-or-more segments, so the collection root
        // (`/dav` and `/dav/`) needs its own routes for clients to mount it.
        .route(webdav::MOUNT, any(webdav::route))
        .route(&format!("{}/", webdav::MOUNT), any(webdav::route))
        .route(&format!("{}/{{*rest}}", webdav::MOUNT), any(webdav::route))
        .with_state(state)
}
