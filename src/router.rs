//! HTTP routing: every route the server exposes, wired to its handler.
//!
//! Extracted from `main()` so the full application — session auth, permission
//! enforcement, traversal rejection, the WebDAV mount — can be driven in
//! tests through `tower::ServiceExt::oneshot` without binding a socket.

use axum::{
    extract::Request,
    middleware::Next,
    response::Response,
    routing::{any, get, post, put},
    Router,
};

use crate::{auth, files, state::AppState, ui, update, webdav};

/// One-line access log per request: `METHOD /path -> status (ms)` (issue
/// #47). Levels are chosen so the default (info) log shows what *changed*
/// without drowning in browsing noise:
///
/// - server errors (5xx) → warn;
/// - mutations (anything but GET/HEAD/OPTIONS/PROPFIND) → info, including
///   their 4xx rejections (a denied write is worth seeing);
/// - reads → debug. This also keeps the WebDAV mount's routine 401
///   challenge round-trips (every client starts credential-less) and
///   per-thumbnail GETs out of the default view.
///
/// The duration is handler time: `next.run` resolves when the response —
/// headers plus body *stream* — is produced, so long-lived bodies (the
/// `/events` SSE stream, a large download) log their startup latency, not a
/// misleading connection lifetime.
async fn access_log(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    // Path only: query strings (file paths) stay out so the log shape is
    // predictable; request bodies and credentials are never logged.
    let path = req.uri().path().to_string();
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    let status = resp.status().as_u16();
    let ms = start.elapsed().as_millis();
    let read = matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS" | "PROPFIND");
    if status >= 500 {
        tracing::warn!("{method} {path} -> {status} ({ms}ms)");
    } else if read {
        tracing::debug!("{method} {path} -> {status} ({ms}ms)");
    } else {
        tracing::info!("{method} {path} -> {status} ({ms}ms)");
    }
    resp
}

/// Build the complete application router over `state`.
///
/// Route map — auth: `S` = session cookie ([`auth::Session`]), `B` = HTTP
/// Basic, `-` = public; permission: the level the handler additionally
/// enforces on top of auth:
///
/// ```text
/// GET  /                  -   app.html when authed, else redirect to /login
/// GET  /app.css           -   SPA stylesheet (public; no secrets)
/// GET  /app.js            -   SPA script (public; no secrets)
/// GET  /login             -   login page
/// POST /login             -   checks the password, sets the `sid` cookie
/// GET  /logout            -   clears the session cookie
/// GET  /events            S   live-input SSE stream (Status tab)
/// GET  /api/name          -   device name for the browser tab title (public)
/// GET  /api/info          S   connection info for the Status tab
/// GET  /api/list          S   read
/// GET  /api/download      S   read
/// GET  /api/zip           S   read
/// GET  /api/raw           S   read; HTTP Range + conditional cache validators
/// GET  /api/thumb         S   read; server-side downscale + disk cache (#28)
/// PUT  /api/upload        S   write
/// POST /api/mkdir         S   write
/// POST /api/delete        S   delete
/// POST /api/rename        S   write
/// POST /api/move          S   write
/// POST /api/copy          S   write
/// GET  /api/settings      S   read-only settings view
/// GET  /api/update/check  S   queries GitHub Releases
/// POST /api/update/apply  S   downloads + installs the matching asset
/// ANY  /dav[/...]         B   read; write/delete methods gated by permission
/// ```
///
/// Permission enforcement lives in TWO places that must stay in sync:
/// `files.rs` checks `can_write`/`can_delete` per handler (the write/delete
/// rows above), and `webdav::method_allowed` gates the WebDAV methods on the
/// `/dav` mount. A mutating surface added to one without the other silently
/// bypasses the permission ladder.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(ui::index))
        .route("/app.css", get(ui::app_css))
        .route("/app.js", get(ui::app_js))
        .route("/login", get(ui::login_page).post(auth::login))
        .route("/logout", get(auth::logout))
        .route("/events", get(ui::events))
        .route("/api/info", get(ui::info))
        .route("/api/name", get(ui::public_name))
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
        .layer(axum::middleware::from_fn(access_log))
        .with_state(state)
}

// Integration tests: drive the real router end-to-end (extractors, session
// auth, permission gates, the WebDAV bridge) against a scratch directory.
// `oneshot` feeds one request through the whole tower service — no port, no
// process, so these run anywhere `cargo test` does.
#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{header, Method, Request, Response, StatusCode},
        Router,
    };
    use base64::Engine;
    use http_body_util::BodyExt;
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    use crate::config::{Permission, Settings};
    use crate::state::{AppState, ServerInfo};
    use crate::webdav::{self, DavState};

    const PASSWORD: &str = "test-pw";
    const SESSION: &str = "test-session-token";

    /// A scratch served root that cleans itself up (same shape as the
    /// `files.rs` tests).
    struct TmpRoot(PathBuf);

    impl TmpRoot {
        fn new(name: &str) -> TmpRoot {
            let path = std::env::temp_dir().join(format!(
                "amberdav-router-test-{}-{name}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TmpRoot(path)
        }
    }

    impl Drop for TmpRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Real `AppState` over `root`, exactly as `main()` wires it (canonical
    /// root, fixed session token/password) but with test-controlled settings.
    fn state_for(root: &Path, permission: Permission) -> AppState {
        state_with_settings(
            root,
            Settings {
                permission,
                ..Settings::default()
            },
        )
    }

    fn state_with_settings(root: &Path, settings: Settings) -> AppState {
        let settings = Arc::new(settings);
        let (events, _) = broadcast::channel(8);
        let throttle = Arc::new(crate::throttle::Throttle::new());
        let canon_root = std::fs::canonicalize(root).unwrap();
        AppState {
            mounts: Arc::new(crate::state::MountTable::single(canon_root)),
            session: Arc::from(SESSION),
            settings: settings.clone(),
            dav: DavState {
                fs: webdav::DavFs::Single(webdav::build_handler(root.to_str().unwrap())),
                password: Arc::from(PASSWORD),
                settings,
                throttle: throttle.clone(),
            },
            info: Arc::new(ServerInfo {
                port: 8080,
                password: PASSWORD.to_string(),
                config_error: None,
            }),
            throttle,
            events,
            screen_status: Arc::new(std::sync::Mutex::new("test".to_string())),
            shutdown: CancellationToken::new(),
        }
    }

    fn app(root: &TmpRoot, permission: Permission) -> Router {
        super::router(state_for(&root.0, permission))
    }

    /// Multi-root `AppState` over two scratch mounts named `one` and `two`,
    /// wired exactly as `main()` does it (canonical roots, per-mount DAV
    /// handlers, synthesized virtual root).
    fn multi_app(one: &TmpRoot, two: &TmpRoot, permission: Permission) -> Router {
        let settings = Arc::new(Settings {
            permission,
            ..Settings::default()
        });
        let (events, _) = broadcast::channel(8);
        let throttle = Arc::new(crate::throttle::Throttle::new());
        let mounts = vec![
            ("one".to_string(), std::fs::canonicalize(&one.0).unwrap()),
            ("two".to_string(), std::fs::canonicalize(&two.0).unwrap()),
        ];
        let state = AppState {
            dav: DavState {
                fs: webdav::build_multi_fs(&mounts),
                password: Arc::from(PASSWORD),
                settings: settings.clone(),
                throttle: throttle.clone(),
            },
            mounts: Arc::new(crate::state::MountTable::multi(mounts)),
            session: Arc::from(SESSION),
            settings,
            info: Arc::new(ServerInfo {
                port: 8080,
                password: PASSWORD.to_string(),
                config_error: None,
            }),
            throttle,
            events,
            screen_status: Arc::new(std::sync::Mutex::new("test".to_string())),
            shutdown: CancellationToken::new(),
        };
        super::router(state)
    }

    /// The session cookie a successful login would have set.
    fn session_cookie() -> (header::HeaderName, String) {
        (header::COOKIE, format!("sid={SESSION}"))
    }

    /// `Authorization: Basic` for the WebDAV mount (username is ignored).
    fn basic_auth(password: &str) -> String {
        let creds = base64::engine::general_purpose::STANDARD.encode(format!("user:{password}"));
        format!("Basic {creds}")
    }

    async fn send(app: &Router, req: Request<Body>) -> Response<Body> {
        app.clone().oneshot(req).await.unwrap()
    }

    async fn body_string(resp: Response<Body>) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    fn get_authed(uri: &str) -> Request<Body> {
        let (k, v) = session_cookie();
        Request::builder()
            .uri(uri)
            .header(k, v)
            .body(Body::empty())
            .unwrap()
    }

    fn json_authed(method: &str, uri: &str, body: &str) -> Request<Body> {
        let (k, v) = session_cookie();
        Request::builder()
            .method(method)
            .uri(uri)
            .header(k, v)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn dav(method: &str, uri: &str, auth: Option<&str>, body: &str) -> Request<Body> {
        let mut req = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(uri);
        if let Some(pw) = auth {
            req = req.header(header::AUTHORIZATION, basic_auth(pw));
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    // --- session gating -------------------------------------------------

    // Every /api/* route requires the session cookie; without it (or with a
    // stale token from a previous boot) the answer is 401, never data.
    #[tokio::test]
    async fn api_without_session_cookie_is_401() {
        let root = TmpRoot::new("api-401");
        std::fs::write(root.0.join("secret.txt"), b"hidden").unwrap();
        let app = app(&root, Permission::ReadWriteDelete);

        for uri in [
            "/api/list",
            "/api/info",
            "/api/settings",
            "/api/download?path=secret.txt",
        ] {
            let resp = send(&app, get(uri)).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }

        // Wrong token: same rejection.
        let req = Request::builder()
            .uri("/api/list")
            .header(header::COOKIE, "sid=stale-token-from-last-boot")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, req).await.status(), StatusCode::UNAUTHORIZED);

        // Write endpoints reject before touching the body or the disk.
        let req = Request::builder()
            .method("POST")
            .uri("/api/mkdir")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"path":"","name":"d"}"#))
            .unwrap();
        assert_eq!(send(&app, req).await.status(), StatusCode::UNAUTHORIZED);
        assert!(!root.0.join("d").exists());
    }

    // The cookie happy path: a valid session token reaches the data.
    #[tokio::test]
    async fn api_list_with_session_cookie_returns_entries() {
        let root = TmpRoot::new("api-list");
        std::fs::write(root.0.join("a.txt"), b"abc").unwrap();
        std::fs::create_dir(root.0.join("sub")).unwrap();
        let app = app(&root, Permission::ReadOnly);

        let resp = send(&app, get_authed("/api/list")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let entries: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        // Order-independent on purpose: listings are deliberately unsorted
        // (readdir order) — the web UI re-sorts client-side (issue #58).
        let entries = entries.as_array().expect("listing is an array");
        assert_eq!(entries.len(), 2);
        let sub = entries.iter().find(|e| e["name"] == "sub").expect("sub");
        assert_eq!(sub["dir"], true);
        let file = entries
            .iter()
            .find(|e| e["name"] == "a.txt")
            .expect("a.txt");
        assert_eq!(file["dir"], false);
        assert_eq!(file["size"], 3);
    }

    // /api/info carries the disk gauge fields for the served root: numbers on
    // unix (where the statvfs shim is wired up), null where unreportable —
    // the UI hides the gauge on null, so the keys must always be present.
    #[tokio::test]
    async fn api_info_reports_disk_space_fields() {
        let root = TmpRoot::new("api-info-disk");
        let app = app(&root, Permission::ReadOnly);

        let resp = send(&app, get_authed("/api/info")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let info: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert!(info.get("disk_free").is_some(), "disk_free key missing");
        assert!(info.get("disk_total").is_some(), "disk_total key missing");
        #[cfg(unix)]
        {
            let free = info["disk_free"].as_u64().expect("disk_free is a number");
            let total = info["disk_total"].as_u64().expect("disk_total is a number");
            assert!(total > 0 && free <= total, "free={free} total={total}");
        }
        // The Settings help renders this instead of a hardcoded device path
        // (issue #60); it must always be a non-empty path string.
        let cfg = info["config_path"].as_str().expect("config_path string");
        assert!(
            cfg.ends_with("config.json"),
            "unexpected config_path: {cfg}"
        );
    }

    // The embedded SPA and login page are immutable per build, so they carry
    // a strong ETag and answer a matching If-None-Match with 304 (issue #58).
    // Auth still comes first: an unauthenticated / redirects even with a
    // matching tag.
    #[tokio::test]
    async fn embedded_pages_serve_etags_and_304() {
        let root = TmpRoot::new("etag");
        let app = app(&root, Permission::ReadOnly);

        // First visit: 200 with a quoted ETag and the real page body.
        let resp = send(&app, get_authed("/")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let etag = resp.headers()[header::ETAG].to_str().unwrap().to_string();
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
        assert!(body_string(resp).await.contains("AmberDAV"));

        // Revisit with the tag: 304 with the tag, no body re-sent.
        let resp = send(
            &app,
            get_authed_with("/", &[(header::IF_NONE_MATCH, &etag)]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers()[header::ETAG].to_str().unwrap(), etag);
        assert!(body_string(resp).await.is_empty());

        // A stale tag (e.g. from a previous build) re-serves the full page.
        let resp = send(
            &app,
            get_authed_with("/", &[(header::IF_NONE_MATCH, "\"stale\"")]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // The public login page gets the same treatment, with its own tag.
        let resp = send(&app, get("/login")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let login_etag = resp.headers()[header::ETAG].to_str().unwrap().to_string();
        assert_ne!(login_etag, etag, "distinct content, distinct tags");
        let req = Request::builder()
            .uri("/login")
            .header(header::IF_NONE_MATCH, &login_etag)
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, req).await.status(), StatusCode::NOT_MODIFIED);

        // Auth outranks caching: no session means redirect, never 304.
        let req = Request::builder()
            .uri("/")
            .header(header::IF_NONE_MATCH, &etag)
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, req).await.status(), StatusCode::SEE_OTHER);
    }

    // The SPA's CSS and JS are public (no session needed), carry the right
    // content type, and get the same ETag/304 treatment as the pages.
    #[tokio::test]
    async fn app_assets_serve_with_content_type_and_etags() {
        let root = TmpRoot::new("assets");
        let app = app(&root, Permission::ReadOnly);

        for (uri, ctype, needle) in [
            ("/app.css", "text/css", ":root"),
            ("/app.js", "text/javascript", "let cwd"),
        ] {
            // No session cookie: still served (the assets hold no secrets).
            let resp = send(&app, get(uri)).await;
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            assert!(
                resp.headers()[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with(ctype),
                "{uri} content type"
            );
            let etag = resp.headers()[header::ETAG].to_str().unwrap().to_string();
            assert!(
                etag.starts_with('"') && etag.ends_with('"'),
                "{uri}: {etag}"
            );
            assert!(body_string(resp).await.contains(needle), "{uri} body");

            // Revisit with the tag: 304, no body re-sent.
            let req = Request::builder()
                .uri(uri)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap();
            let resp = send(&app, req).await;
            assert_eq!(resp.status(), StatusCode::NOT_MODIFIED, "{uri} revisit");
            assert!(body_string(resp).await.is_empty(), "{uri} 304 body");
        }
    }

    // --- zip downloads ----------------------------------------------------

    // /api/zip names the archive after the selection (issue #45) and streams
    // a complete zip — the end-of-central-directory record is present, which
    // is exactly what a truncated stream lacks.
    #[tokio::test]
    async fn zip_download_is_named_after_the_selection() {
        let root = TmpRoot::new("zip-name");
        std::fs::create_dir(root.0.join("GB")).unwrap();
        std::fs::write(root.0.join("GB/a.gb"), b"rom bytes").unwrap();
        let app = app(&root, Permission::ReadOnly);

        let p = base64::engine::general_purpose::STANDARD.encode(r#"["GB"]"#);
        let resp = send(&app, get_authed(&format!("/api/zip?p={p}"))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"GB.zip\""
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&bytes[..4], b"PK\x03\x04", "zip local-file header");
        assert!(
            bytes.windows(4).any(|w| w == b"PK\x05\x06"),
            "end-of-central-directory record missing — stream did not finish"
        );
    }

    // A file that fails to open mid-walk must not yield a silently
    // "complete" truncated zip (review §1.12): the body stream ends with an
    // error, which hyper turns into a dropped connection without the final
    // chunk in real serving.
    #[cfg(unix)]
    #[tokio::test]
    async fn zip_mid_stream_failure_surfaces_as_a_body_error() {
        use std::os::unix::fs::PermissionsExt;
        // Root ignores file modes; the failure can't be staged there.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = TmpRoot::new("zip-abort");
        std::fs::create_dir(root.0.join("d")).unwrap();
        std::fs::write(root.0.join("d/ok.txt"), b"fine").unwrap();
        std::fs::write(root.0.join("d/locked.txt"), b"nope").unwrap();
        std::fs::set_permissions(
            root.0.join("d/locked.txt"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let app = app(&root, Permission::ReadOnly);

        let p = base64::engine::general_purpose::STANDARD.encode(r#"["d"]"#);
        let resp = send(&app, get_authed(&format!("/api/zip?p={p}"))).await;
        // The headers were already committed; only the body can signal.
        assert_eq!(resp.status(), StatusCode::OK);
        let collected = resp.into_body().collect().await;
        assert!(
            collected.is_err(),
            "a truncated zip must end in a body error, not a clean EOF"
        );
        let _ = std::fs::set_permissions(
            root.0.join("d/locked.txt"),
            std::fs::Permissions::from_mode(0o644),
        );
    }

    // An unreadable *directory* in the selection is caught by the pre-flight
    // and rejected before any byte (or the irrevocable 200) is sent.
    #[cfg(unix)]
    #[tokio::test]
    async fn zip_unreadable_dir_is_rejected_before_streaming() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let root = TmpRoot::new("zip-preflight");
        std::fs::create_dir(root.0.join("sealed")).unwrap();
        std::fs::set_permissions(
            root.0.join("sealed"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let app = app(&root, Permission::ReadOnly);

        let p = base64::engine::general_purpose::STANDARD.encode(r#"["sealed"]"#);
        let resp = send(&app, get_authed(&format!("/api/zip?p={p}"))).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let _ = std::fs::set_permissions(
            root.0.join("sealed"),
            std::fs::Permissions::from_mode(0o755),
        );
    }

    // --- login ------------------------------------------------------------

    // auth::login must set the session cookie only on the right password; a
    // wrong guess bounces back to the login page cookie-less.
    #[tokio::test]
    async fn login_sets_cookie_only_on_correct_password() {
        let root = TmpRoot::new("login");
        let app = app(&root, Permission::ReadWrite);
        let form = |pw: &str| {
            Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("password={pw}")))
                .unwrap()
        };

        // Right password: redirect home, session cookie set.
        let resp = send(&app, form(PASSWORD)).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers()[header::LOCATION], "/");
        let cookie = resp.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.starts_with(&format!("sid={SESSION}")), "{cookie}");
        assert!(cookie.contains("HttpOnly"), "{cookie}");

        // The issued cookie is accepted by the API.
        let req = Request::builder()
            .uri("/api/list")
            .header(header::COOKIE, cookie.split(';').next().unwrap())
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, req).await.status(), StatusCode::OK);

        // Wrong password: back to the login page, no cookie.
        let resp = send(&app, form("wrong-guess")).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers()[header::LOCATION], "/login?e=1");
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
    }

    // --- brute-force throttling (issue #27) --------------------------------

    /// Attach a fake TCP peer address, the same way
    /// `into_make_service_with_connect_info` does on a real connection.
    fn with_peer(mut req: Request<Body>, ip: [u8; 4]) -> Request<Body> {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                ip, 49152,
            ))));
        req
    }

    // After the free failure budget, the login endpoint answers 429 (with
    // Retry-After) before even looking at the password — the correct guess is
    // refused too. Another source IP keeps its own budget.
    #[tokio::test]
    async fn login_throttles_repeated_failures_per_ip() {
        let root = TmpRoot::new("login-throttle");
        let app = app(&root, Permission::ReadWrite);
        let form = |pw: &str, ip: [u8; 4]| {
            let req = Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(format!("password={pw}")))
                .unwrap();
            with_peer(req, ip)
        };

        // The free attempts: normal wrong-password redirects.
        for _ in 0..3 {
            let resp = send(&app, form("wrong-guess", [10, 0, 0, 9])).await;
            assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        }

        // Budget exhausted: throttled before the check — even the correct
        // password is refused, so the throttle can't be used as an oracle.
        let resp = send(&app, form(PASSWORD, [10, 0, 0, 9])).await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().contains_key(header::RETRY_AFTER));

        // A different client is unaffected and can log in.
        let resp = send(&app, form(PASSWORD, [10, 0, 0, 10])).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers()[header::LOCATION], "/");
    }

    // The WebDAV mount shares the same per-IP budget: repeated wrong Basic
    // credentials lead to 429, while credential-less requests (the normal
    // 401-challenge round-trip every DAV client starts with) never count.
    #[tokio::test]
    async fn dav_throttles_wrong_credentials_but_not_the_challenge() {
        let root = TmpRoot::new("dav-throttle");
        std::fs::write(root.0.join("a.txt"), b"abc").unwrap();
        let app = app(&root, Permission::ReadWrite);
        let ip = [10, 0, 0, 20];

        // Credential-less requests: always the 401 challenge, never throttled.
        for _ in 0..10 {
            let resp = send(&app, with_peer(dav("GET", "/dav/a.txt", None, ""), ip)).await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
            assert!(resp.headers().contains_key(header::WWW_AUTHENTICATE));
        }
        // …and the right password still works right after.
        let resp = send(
            &app,
            with_peer(dav("GET", "/dav/a.txt", Some(PASSWORD), ""), ip),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Wrong credentials burn the budget…
        for _ in 0..3 {
            let resp = send(
                &app,
                with_peer(dav("GET", "/dav/a.txt", Some("wrong"), ""), ip),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        // …then the IP is throttled, even with the correct password.
        let resp = send(
            &app,
            with_peer(dav("GET", "/dav/a.txt", Some(PASSWORD), ""), ip),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

        // A different client is unaffected.
        let resp = send(
            &app,
            with_peer(dav("GET", "/dav/a.txt", Some(PASSWORD), ""), [10, 0, 0, 21]),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // The budget is shared across surfaces: failures on the login form also
    // throttle the same IP's WebDAV access (one guess budget per client).
    #[tokio::test]
    async fn throttle_budget_is_shared_between_login_and_dav() {
        let root = TmpRoot::new("shared-throttle");
        std::fs::write(root.0.join("a.txt"), b"abc").unwrap();
        let app = app(&root, Permission::ReadWrite);
        let ip = [10, 0, 0, 30];

        for _ in 0..3 {
            let req = Request::builder()
                .method("POST")
                .uri("/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from("password=wrong-guess"))
                .unwrap();
            let resp = send(&app, with_peer(req, ip)).await;
            assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        }

        let resp = send(
            &app,
            with_peer(dav("GET", "/dav/a.txt", Some(PASSWORD), ""), ip),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    // --- permission enforcement (JSON API) ---------------------------------

    // read_only must block every mutating /api route even with a valid
    // session — the cookie authenticates, the permission still gates.
    #[tokio::test]
    async fn read_only_blocks_api_writes() {
        let root = TmpRoot::new("ro-api");
        std::fs::write(root.0.join("a.txt"), b"abc").unwrap();
        let app = app(&root, Permission::ReadOnly);

        let resp = send(
            &app,
            json_authed("POST", "/api/mkdir", r#"{"path":"","name":"d"}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(!root.0.join("d").exists());

        let resp = send(
            &app,
            json_authed("POST", "/api/delete", r#"{"paths":["a.txt"]}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(root.0.join("a.txt").exists());

        let (k, v) = session_cookie();
        let upload = Request::builder()
            .method("PUT")
            .uri("/api/upload?path=&name=up.txt")
            .header(k, v)
            .body(Body::from("data"))
            .unwrap();
        assert_eq!(send(&app, upload).await.status(), StatusCode::FORBIDDEN);
        assert!(!root.0.join("up.txt").exists());

        // Reads still work at read_only.
        let resp = send(&app, get_authed("/api/download?path=a.txt")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "abc");
    }

    // read_write may create but not delete; read_write_delete may do both.
    #[tokio::test]
    async fn delete_requires_the_delete_permission() {
        let root = TmpRoot::new("rw-delete");
        std::fs::write(root.0.join("a.txt"), b"abc").unwrap();

        let rw = app(&root, Permission::ReadWrite);
        let resp = send(
            &rw,
            json_authed("POST", "/api/mkdir", r#"{"path":"","name":"made"}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(root.0.join("made").is_dir());
        let resp = send(
            &rw,
            json_authed("POST", "/api/delete", r#"{"paths":["a.txt"]}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(root.0.join("a.txt").exists());

        let rwd = app(&root, Permission::ReadWriteDelete);
        let resp = send(
            &rwd,
            json_authed("POST", "/api/delete", r#"{"paths":["a.txt"]}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!root.0.join("a.txt").exists());
    }

    // --- permission enforcement (WebDAV mount) ------------------------------

    // The /dav mount keeps its own Basic auth; no/wrong credentials are 401
    // with the challenge header so network-drive clients prompt.
    #[tokio::test]
    async fn dav_requires_basic_auth() {
        let root = TmpRoot::new("dav-auth");
        std::fs::write(root.0.join("a.txt"), b"abc").unwrap();
        let app = app(&root, Permission::ReadWrite);

        let resp = send(&app, dav("GET", "/dav/a.txt", None, "")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().contains_key(header::WWW_AUTHENTICATE));

        let resp = send(&app, dav("GET", "/dav/a.txt", Some("wrong"), "")).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = send(&app, dav("GET", "/dav/a.txt", Some(PASSWORD), "")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "abc");
    }

    // read_only must hold across the WebDAV mount too — otherwise the JSON
    // API's permission checks could simply be bypassed via /dav.
    #[tokio::test]
    async fn read_only_blocks_dav_write_methods() {
        let root = TmpRoot::new("ro-dav");
        std::fs::write(root.0.join("a.txt"), b"abc").unwrap();
        let ro = app(&root, Permission::ReadOnly);

        for (method, uri) in [
            ("PUT", "/dav/new.txt"),
            ("MKCOL", "/dav/newdir"),
            ("MOVE", "/dav/a.txt"),
            ("COPY", "/dav/a.txt"),
            ("DELETE", "/dav/a.txt"),
        ] {
            let resp = send(&ro, dav(method, uri, Some(PASSWORD), "x")).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        }
        assert!(!root.0.join("new.txt").exists());
        assert!(!root.0.join("newdir").exists());
        assert!(root.0.join("a.txt").exists());

        // Reads still pass at read_only.
        let resp = send(&ro, dav("GET", "/dav/a.txt", Some(PASSWORD), "")).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // read_write: PUT goes through, DELETE is still gated.
        let rw = app(&root, Permission::ReadWrite);
        let resp = send(&rw, dav("PUT", "/dav/new.txt", Some(PASSWORD), "hi")).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(std::fs::read(root.0.join("new.txt")).unwrap(), b"hi");
        let resp = send(&rw, dav("DELETE", "/dav/new.txt", Some(PASSWORD), "")).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(root.0.join("new.txt").exists());
    }

    // --- traversal -----------------------------------------------------------

    // URL-encoded `..` must be rejected end-to-end: axum percent-decodes the
    // query, so the handlers see a literal `..` and `resolve` refuses it
    // before any filesystem access.
    #[tokio::test]
    async fn url_encoded_traversal_is_rejected() {
        let outside = TmpRoot::new("traversal-outside");
        std::fs::write(outside.0.join("secret.txt"), b"top secret").unwrap();
        let root = TmpRoot::new("traversal-root");
        let app = app(&root, Permission::ReadWriteDelete);

        // The sibling tempdir really is reachable via `..` on disk; only the
        // request validation stands in the way.
        let sibling = outside.0.file_name().unwrap().to_str().unwrap().to_string();
        for uri in [
            "/api/list?path=%2E%2E".to_string(),
            format!("/api/list?path=..%2F{sibling}"),
            format!("/api/download?path=..%2F{sibling}%2Fsecret.txt"),
            format!("/api/raw?path=%2e%2e%2f{sibling}%2fsecret.txt"),
        ] {
            let resp = send(&app, get_authed(&uri)).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{uri}");
            let body = body_string(resp).await;
            assert!(!body.contains("top secret"), "{uri} leaked the file");
        }

        // safe_name composition: a traversal smuggled in the *name* field of
        // a write operation is rejected the same way.
        let resp = send(
            &app,
            json_authed("POST", "/api/mkdir", r#"{"path":"","name":"../evil"}"#),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(!outside.0.join("evil").exists());

        let (k, v) = session_cookie();
        let upload = Request::builder()
            .method("PUT")
            .uri(format!("/api/upload?path=&name=..%2F{sibling}%2Fpwn.txt"))
            .header(k, v)
            .body(Body::from("data"))
            .unwrap();
        assert_eq!(send(&app, upload).await.status(), StatusCode::BAD_REQUEST);
        assert!(!outside.0.join("pwn.txt").exists());
    }

    // --- files handlers over the wire ---------------------------------------

    // upload → list → download round trip: safe_name + resolve + confine
    // composed through the real extractors against a real directory.
    #[tokio::test]
    async fn upload_download_round_trip() {
        let root = TmpRoot::new("round-trip");
        std::fs::create_dir(root.0.join("sub")).unwrap();
        let app = app(&root, Permission::ReadWrite);

        let (k, v) = session_cookie();
        let upload = Request::builder()
            .method("PUT")
            .uri("/api/upload?path=sub&name=hello.txt")
            .header(k, v)
            .body(Body::from("hello router"))
            .unwrap();
        let resp = send(&app, upload).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read(root.0.join("sub/hello.txt")).unwrap(),
            b"hello router"
        );

        let resp = send(&app, get_authed("/api/download?path=sub%2Fhello.txt")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let disposition = resp.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .to_string();
        assert!(disposition.contains("hello.txt"), "{disposition}");
        assert_eq!(body_string(resp).await, "hello router");

        // Re-upload without overwrite: 409, original intact.
        let (k, v) = session_cookie();
        let again = Request::builder()
            .method("PUT")
            .uri("/api/upload?path=sub&name=hello.txt")
            .header(k, v)
            .body(Body::from("clobber"))
            .unwrap();
        assert_eq!(send(&app, again).await.status(), StatusCode::CONFLICT);
        assert_eq!(
            std::fs::read(root.0.join("sub/hello.txt")).unwrap(),
            b"hello router"
        );
    }

    /// `get_authed` plus extra request headers (Range/If-Range tests).
    fn get_authed_with(uri: &str, extra: &[(header::HeaderName, &str)]) -> Request<Body> {
        let (k, v) = session_cookie();
        let mut req = Request::builder().uri(uri).header(k, v);
        for (name, val) in extra {
            req = req.header(name, *val);
        }
        req.body(Body::empty()).unwrap()
    }

    // /api/download is resumable (issue #29): a Range request gets 206 with
    // the exact slice + Content-Range, the full download is unchanged, both
    // stay attachments, and an unsatisfiable range is 416 — mirroring the
    // /api/raw machinery it shares.
    #[tokio::test]
    async fn download_honors_range_requests() {
        let root = TmpRoot::new("download-range");
        std::fs::write(root.0.join("disc.bin"), b"0123456789abcdef").unwrap();
        let app = app(&root, Permission::ReadOnly);

        // Full download: 200 + Accept-Ranges advertises resumability.
        let resp = send(&app, get_authed("/api/download?path=disc.bin")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(resp.headers()[header::CONTENT_LENGTH], "16");
        let cd = resp.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .to_string();
        assert!(cd.contains("attachment") && cd.contains("disc.bin"), "{cd}");
        assert_eq!(body_string(resp).await, "0123456789abcdef");

        // Resume from byte 10 (the "died at 90%" case): the open-ended form.
        let resp = send(
            &app,
            get_authed_with(
                "/api/download?path=disc.bin",
                &[(header::RANGE, "bytes=10-")],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 10-15/16");
        assert_eq!(resp.headers()[header::CONTENT_LENGTH], "6");
        let cd = resp.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .to_string();
        assert!(cd.contains("attachment"), "partial must stay an attachment");
        assert_eq!(body_string(resp).await, "abcdef");

        // Bounded mid-file slice.
        let resp = send(
            &app,
            get_authed_with(
                "/api/download?path=disc.bin",
                &[(header::RANGE, "bytes=4-7")],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes 4-7/16");
        assert_eq!(body_string(resp).await, "4567");

        // Start past EOF: 416 with the total, and no attachment disposition
        // (nothing should invite a "save as" on an error body).
        let resp = send(
            &app,
            get_authed_with(
                "/api/download?path=disc.bin",
                &[(header::RANGE, "bytes=99-")],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers()[header::CONTENT_RANGE], "bytes */16");
        assert!(resp.headers().get(header::CONTENT_DISPOSITION).is_none());
    }

    // The If-Range gate carries over from /api/raw: resuming with a stale
    // validator must serve the full body (200), never splice two versions;
    // a current validator keeps the 206.
    #[tokio::test]
    async fn download_if_range_gates_the_resume() {
        let root = TmpRoot::new("download-if-range");
        std::fs::write(root.0.join("disc.bin"), b"0123456789abcdef").unwrap();
        let app = app(&root, Permission::ReadOnly);

        // The validator a client would have from its first attempt.
        let resp = send(&app, get_authed("/api/download?path=disc.bin")).await;
        let etag = resp.headers()[header::ETAG].to_str().unwrap().to_string();

        // Current validator → the resume is honored.
        let resp = send(
            &app,
            get_authed_with(
                "/api/download?path=disc.bin",
                &[
                    (header::RANGE, "bytes=10-"),
                    (header::IF_RANGE, etag.as_str()),
                ],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(body_string(resp).await, "abcdef");

        // Stale validator (the file changed since): full body instead.
        let resp = send(
            &app,
            get_authed_with(
                "/api/download?path=disc.bin",
                &[
                    (header::RANGE, "bytes=10-"),
                    (header::IF_RANGE, "\"deadbeef.0.0\""),
                ],
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "0123456789abcdef");
    }

    // Downloading a folder is rejected up front (folders go through /api/zip)
    // instead of failing mid-stream after a 200.
    #[tokio::test]
    async fn download_of_a_directory_is_a_400() {
        let root = TmpRoot::new("download-dir");
        std::fs::create_dir(root.0.join("folder")).unwrap();
        let app = app(&root, Permission::ReadOnly);
        let resp = send(&app, get_authed("/api/download?path=folder")).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // Folder uploads (issue #30): the `dir` query field creates the missing
    // folder chain under the destination, the file lands at the leaf, and the
    // per-file 409 + overwrite semantics keep working for nested paths.
    #[tokio::test]
    async fn upload_with_dir_creates_nested_folders() {
        let root = TmpRoot::new("upload-dir");
        std::fs::create_dir(root.0.join("Roms")).unwrap();
        let app = app(&root, Permission::ReadWrite);

        let put = |overwrite: bool, body: &'static str| {
            let (k, v) = session_cookie();
            Request::builder()
                .method("PUT")
                .uri(format!(
                    "/api/upload?path=Roms&dir=GameBoy%2FSaves&name=zelda.sav{}",
                    if overwrite { "&overwrite=true" } else { "" }
                ))
                .header(k, v)
                .body(Body::from(body))
                .unwrap()
        };

        let resp = send(&app, put(false, "save data")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read(root.0.join("Roms/GameBoy/Saves/zelda.sav")).unwrap(),
            b"save data"
        );

        // Same per-file collision semantics as flat uploads: 409 without
        // consent, replaced with `overwrite=true`.
        assert_eq!(
            send(&app, put(false, "clobber")).await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            std::fs::read(root.0.join("Roms/GameBoy/Saves/zelda.sav")).unwrap(),
            b"save data"
        );
        assert_eq!(send(&app, put(true, "v2")).await.status(), StatusCode::OK);
        assert_eq!(
            std::fs::read(root.0.join("Roms/GameBoy/Saves/zelda.sav")).unwrap(),
            b"v2"
        );
    }

    // The `dir` field is a new path surface; traversal smuggled through it —
    // `..`, absolute, backslash-rooted, drive-letter segments — must be 400
    // with nothing created, exactly like `path` and `name` (issue #21 rules).
    #[tokio::test]
    async fn upload_dir_traversal_is_rejected() {
        let outside = TmpRoot::new("upload-dir-outside");
        let root = TmpRoot::new("upload-dir-root");
        let app = app(&root, Permission::ReadWriteDelete);

        let sibling = outside.0.file_name().unwrap().to_str().unwrap().to_string();
        for dir in [
            "%2E%2E".to_string(),                  // ..
            format!("..%2F{sibling}"),             // ../<sibling tempdir>
            format!("good%2F..%2F..%2F{sibling}"), // valid prefix then escape
            "%2Fetc".to_string(),                  // absolute
            "%5Cetc".to_string(),                  // backslash-rooted
            "C%3A%5Cevil".to_string(),             // drive letter
            "a%5Cb".to_string(),                   // backslash separator
        ] {
            let (k, v) = session_cookie();
            let req = Request::builder()
                .method("PUT")
                .uri(format!("/api/upload?path=&dir={dir}&name=pwn.txt"))
                .header(k, v)
                .body(Body::from("data"))
                .unwrap();
            let resp = send(&app, req).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "dir={dir}");
        }
        assert!(!outside.0.join("pwn.txt").exists());
        assert!(!outside.0.join("evil").exists());
        // Nothing was created inside the root either — not even the valid
        // `good` prefix of the half-valid escape attempt.
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 0);
    }

    // --- settings exposure (issue #27) ---------------------------------------

    // /api/settings must never carry the password to the browser, even for a
    // logged-in session — the login page and README promise it is never shown
    // there. The UI only needs fixed-vs-random: a fixed password serializes as
    // a masked placeholder (truthy), a random one as null.
    #[tokio::test]
    async fn settings_response_redacts_the_password() {
        let root = TmpRoot::new("settings-redact");
        let secret = "fixed-Sup3r-secret";

        // Fixed password: redacted to a placeholder, the secret never appears
        // anywhere in the response body.
        let fixed = super::router(state_with_settings(
            &root.0,
            Settings {
                password: Some(secret.to_string()),
                ..Settings::default()
            },
        ));
        let resp = send(&fixed, get_authed("/api/settings")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(!body.contains(secret), "password leaked: {body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        // Still truthy so the Settings tab keeps showing "fixed code".
        assert_eq!(json["password"], "(hidden)");

        // Random (per-boot) password: stays null, so the tab shows "random".
        let random = app(&root, Permission::ReadWrite);
        let resp = send(&random, get_authed("/api/settings")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(json["password"], serde_json::Value::Null);
    }

    // The web UI needs to know the virtual root is read-only to hide the
    // write affordances there (issue #76): /api/settings carries an
    // authoritative multi_root flag (the settings' own root/roots fields are
    // pre-resolution and can't be trusted for this).
    #[tokio::test]
    async fn settings_response_carries_the_multi_root_flag() {
        let root = TmpRoot::new("settings-single");
        let resp = send(
            &app(&root, Permission::ReadWrite),
            get_authed("/api/settings"),
        )
        .await;
        let json: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(json["multi_root"], serde_json::Value::Bool(false));

        let one = TmpRoot::new("settings-multi-one");
        let two = TmpRoot::new("settings-multi-two");
        let resp = send(
            &multi_app(&one, &two, Permission::ReadWrite),
            get_authed("/api/settings"),
        )
        .await;
        let json: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        assert_eq!(json["multi_root"], serde_json::Value::Bool(true));
    }

    // Multi-root end to end: `/` lists the mount names, content resolves one
    // level down, and every write surface refuses the virtual root on both
    // the JSON API and WebDAV (issue #76).
    #[tokio::test]
    async fn multi_root_virtual_root_lists_mounts_and_refuses_writes() {
        let one = TmpRoot::new("vroot-one");
        let two = TmpRoot::new("vroot-two");
        std::fs::write(one.0.join("a.txt"), b"in one").unwrap();
        let app = multi_app(&one, &two, Permission::ReadWriteDelete);

        // Virtual root listing: the two mounts, as directories.
        let resp = send(&app, get_authed("/api/list?path=")).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
        let names: Vec<&str> = json
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["one", "two"]);

        // Content lives one level down.
        let resp = send(&app, get_authed("/api/list?path=one")).await;
        let body = body_string(resp).await;
        assert!(body.contains("a.txt"), "{body}");

        // JSON API write surfaces refuse the virtual root.
        let mkdir = json_authed("POST", "/api/mkdir", r#"{"path":"","name":"x"}"#);
        assert_eq!(send(&app, mkdir).await.status(), StatusCode::FORBIDDEN);
        let del = json_authed("POST", "/api/delete", r#"{"paths":["/"]}"#);
        assert_eq!(send(&app, del).await.status(), StatusCode::BAD_REQUEST);

        // WebDAV: PROPFIND at the virtual root lists the mounts; writes 403.
        let resp = send(&app, dav("PROPFIND", "/dav/", Some(PASSWORD), "")).await;
        assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
        let body = body_string(resp).await;
        assert!(
            body.contains("/dav/one/") && body.contains("/dav/two/"),
            "{body}"
        );
        let resp = send(&app, dav("MKCOL", "/dav/x", Some(PASSWORD), "")).await;
        assert_ne!(resp.status(), StatusCode::CREATED);
        let resp = send(&app, dav("PUT", "/dav/", Some(PASSWORD), "data")).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    // The landing page routes by session: file manager when logged in,
    // redirect to /login otherwise.
    #[tokio::test]
    async fn index_redirects_anonymous_to_login() {
        let root = TmpRoot::new("index");
        let app = app(&root, Permission::ReadWrite);

        let resp = send(&app, get("/")).await;
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers()[header::LOCATION], "/login");

        let resp = send(&app, get_authed("/")).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
