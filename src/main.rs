//! amber-dav: a tiny WebDAV file server + live gamepad button viewer for
//! Anbernic handhelds (Allwinner H700, aarch64 Linux).
//!
//! Three build shapes, selected by Cargo features (see CLAUDE.md and the
//! `[features]` comments in Cargo.toml):
//!
//! - **headless** (no features): WebDAV/file-manager server only — none of the
//!   device code is compiled. What the desktop/server release assets ship.
//! - **fb** (`--features fb`): static framebuffer/Wayland on-device screen +
//!   gamepad input. The sink is chosen at runtime by `display::detect()`:
//!   Wayland when a compositor socket is found (Steam Deck Game Mode, where
//!   Gamescope owns DRM) → `/dev/fb0` (Anbernic, raw TTY, Desktop Mode) →
//!   headless (banner only). `AMBERDAV_DISPLAY` (`wayland`|`fb`|`headless`)
//!   forces a sink; `AMBERDAV_FB_ROTATE` (90/180/270) rotates the framebuffer.
//! - **sdl** (`--features sdl`): the same screen in a fullscreen SDL2 window,
//!   dynamically linking the system libSDL2. The video driver is tried in
//!   preference order — `x11`, `mali`, `wayland`, `kmsdrm`, `fbcon`
//!   (`sdl::DRIVER_PREFERENCE`) — unless `SDL_VIDEODRIVER` forces one.
//!
//! If both `fb` and `sdl` are enabled, `sdl` wins (the sink and the
//! self-update asset both resolve to the SDL shape).
//!
//! `main()` is thin wiring: resolve settings (`cli`), build the shared
//! `state::AppState`, hand it to `router::router()`, serve.

mod auth;
#[cfg(all(target_os = "linux", any(feature = "fb", feature = "sdl")))]
mod bounce;
mod canvas;
mod cli;
mod config;
mod connection;
mod display;
mod files;
mod input;
mod password;
mod router;
mod screen;
#[cfg(all(target_os = "linux", feature = "sdl"))]
mod sdl;
mod state;
mod throttle;
mod ui;
mod update;
#[cfg(all(target_os = "linux", feature = "fb", not(feature = "sdl")))]
mod wayland;
mod webdav;

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

use clap::Parser;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use input::InputUpdate;
use state::{current_ip, AppState, ServerInfo, SharedSettings};
use webdav::DavState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = cli::Cli::parse();
    let config_path = config::config_path();

    // Handheld: keep auto-creating a default config on first run — the device
    // is configured through the web UI, so the file must exist to be edited.
    // Desktop/server builds never write implicitly; use `--save` to opt in.
    let config_write_error = ensure_default_config(&config_path);

    // Resolve settings: CLI args and AMBERDAV_* env vars merged on top of the
    // config file (CLI > env > file > default). This also fixes the old bug
    // where the config `root` silently overrode the CLI argument.
    //
    // A broken config falls back to defaults but the error is carried along
    // and surfaced on the device screen and the web Status tab — on a handheld
    // stderr is invisible, so a silent fallback would look like the config is
    // simply ignored (issue #19). A failed first-run write rides the same
    // plumbing (issue #35); it can only happen when the file is absent, and a
    // parse error only when it is present, so the two never compete.
    let (file_settings, config_error) = config::load(&config_path);
    let config_error = config_error.or(config_write_error);
    let settings = cli.resolve(file_settings);

    // --save: persist the fully-resolved config and exit. No server is started.
    if cli.save {
        config::save(&config_path, &settings)?;
        println!("config: wrote {}", config_path.display());
        return Ok(());
    }

    // Effective root/port/bind, falling back to the compiled defaults.
    let root = settings
        .root
        .clone()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| ".".to_string());
    let port = settings.port.unwrap_or(8080);
    let bind = settings
        .bind
        .clone()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_string());

    // Effective password: fixed from config, else a fresh random one. 8 chars
    // from the 31-symbol charset is ~40 bits — combined with the per-IP login
    // throttle this puts brute force far out of reach on a hostile LAN while
    // staying easy to read off the device screen and type (issue #27).
    let random_password = settings
        .password
        .as_deref()
        .map(str::is_empty)
        .unwrap_or(true);
    let password = settings
        .password
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| password::generate(8));

    // A random password must always be shown, or it can never be discovered.
    let display_password = random_password || settings.display_password;

    // Resolve the bounce-screensaver source paths up front (relative entries
    // are taken against the served root); only when the feature is enabled.
    let bounce_enabled = settings.bounce_screen.enabled;
    let bounce_paths: Vec<PathBuf> = if bounce_enabled {
        settings
            .bounce_screen
            .folders
            .iter()
            .filter(|f| !f.is_empty())
            .map(|f| {
                let p = PathBuf::from(f);
                if p.is_absolute() {
                    p
                } else {
                    PathBuf::from(&root).join(f)
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    // Long, unguessable session token (never shown; lives only in the cookie).
    let session = password::generate(32);
    let ip = current_ip();

    // Canonicalize the served root so path-safety checks have a stable base.
    let root_path = std::fs::canonicalize(&root).unwrap_or_else(|_| PathBuf::from(&root));

    let settings: SharedSettings = Arc::new(settings);

    // Shared screen mode, driven by the gamepad (A = blank, X = screensaver).
    let screen_mode = screen::mode_handle();

    // Cancelled on Ctrl+C/SIGTERM — and *by* the device exit paths (gamepad
    // exit key, SDL window close) — so in-flight SSE streams end and graceful
    // shutdown can drain instead of hanging on a held-open Status page or
    // killing uploads mid-write (issues #15, #34).
    let shutdown = CancellationToken::new();

    // Broadcast channel carrying input events to all connected SSE clients.
    let (events, _) = broadcast::channel::<InputUpdate>(256);
    input::spawn(
        events.clone(),
        screen_mode.clone(),
        input::InputKeys {
            exit: settings.exit_keys.clone(),
            blank: settings.blank_keys.clone(),
            bounce: settings.bounce_keys.clone(),
            bounce_enabled,
        },
        shutdown.clone(),
    );

    let screen_status: screen::Status = Arc::new(std::sync::Mutex::new("starting…".to_string()));

    // One per-IP failure throttle shared by both password surfaces (the web
    // login and the WebDAV Basic auth), so a guesser gets one budget total.
    let auth_throttle = Arc::new(throttle::Throttle::new());

    let state = AppState {
        root: Arc::new(root_path),
        session: Arc::from(session.as_str()),
        settings: settings.clone(),
        dav: DavState {
            handler: webdav::build_handler(&root),
            password: Arc::from(password.as_str()),
            settings: settings.clone(),
            throttle: auth_throttle.clone(),
        },
        info: Arc::new(ServerInfo {
            port,
            password: password.clone(),
            config_error: config_error.clone(),
        }),
        throttle: auth_throttle,
        events,
        screen_status: screen_status.clone(),
        shutdown: shutdown.clone(),
    };

    let app = router::router(state);

    // Password to surface (screen + sidecar), honoring the hidden-password rule.
    let shown_password = display_password.then(|| password.clone());

    // Optional sidecar for external launchers / Decky. Honors the hidden-pw rule.
    if let Some(cf) = settings
        .connection_file
        .as_deref()
        .filter(|p| !p.is_empty())
    {
        connection::ConnectionInfo::new(ip, port, shown_password.clone())
            .write(std::path::Path::new(cf));
    }

    print_banner(ip, port, &root, &password);
    // On-screen startup-error text: the first line is the red headline, the
    // rest the detail (the `canvas::info_canvas` contract).
    let screen_error = config_error
        .as_ref()
        .map(|e| format!("Config error - using defaults\n{e}"));
    // Paint the connection info + QR onto the device screen (password hidden
    // when configured, but only ever allowed when it's a fixed password).
    screen::show(
        port,
        shown_password,
        screen_status,
        screen_mode,
        bounce_paths,
        screen_error,
        shutdown.clone(),
    );

    let listener = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
    // `with_connect_info` exposes the TCP peer address to the handlers — the
    // key for per-IP login throttling (issue #27). The server is direct-serve
    // (no reverse proxy in the normal deployment), so the socket address is
    // authoritative; X-Forwarded-For is deliberately ignored.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(shutdown))
    .await?;
    Ok(())
}

/// Device builds: write a default config on first run (the device is
/// configured through the web UI, so the file must exist to be edited) and
/// return a human-readable error when that write fails — on a read-only or
/// full SD card, stderr is invisible from the OS menu, so the caller threads
/// the message into the `config_error` machinery that the device screen and
/// the web Status tab already display (issue #35).
#[cfg(any(feature = "fb", feature = "sdl"))]
fn ensure_default_config(path: &std::path::Path) -> Option<String> {
    if path.exists() {
        return None;
    }
    match config::save(path, &config::Settings::default()) {
        Ok(()) => {
            eprintln!("config: wrote default {}", path.display());
            None
        }
        Err(e) => {
            let msg = format!("cannot write default config {}: {e}", path.display());
            eprintln!("config: {msg}");
            Some(msg)
        }
    }
}

/// Desktop/server builds never write a config implicitly (`--save` opts in),
/// so there is nothing to attempt and nothing to report.
#[cfg(not(any(feature = "fb", feature = "sdl")))]
fn ensure_default_config(_path: &std::path::Path) -> Option<String> {
    None
}

fn print_banner(ip: IpAddr, port: u16, root: &str, password: &str) {
    let status_url = format!("http://{ip}:{port}/");
    println!("\n  amber-dav");
    println!("  serving:  {root}");
    println!("  status:   {status_url}");
    println!("  webdav:   http://{ip}:{port}{}", webdav::MOUNT);
    println!("  password: {password}   (user: anything)\n");

    // A scannable QR to the status page.
    match qrcode::QrCode::new(status_url.as_bytes()) {
        Ok(code) => {
            let art = code
                .render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build();
            println!("{art}\n");
        }
        Err(e) => eprintln!("  (qr unavailable: {e})\n"),
    }
}

/// How long graceful shutdown may spend draining in-flight connections before
/// the process exits anyway. Short enough to beat Docker's default 10s
/// SIGKILL, long enough to finish a write that is actually progressing.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(8);

/// The future handed to `axum::serve(...).with_graceful_shutdown`: resolves
/// when shutdown is requested, cancels the shared token (idempotent — the
/// device paths cancel it themselves), and arms the drain watchdog.
async fn shutdown_signal(shutdown: CancellationToken) {
    shutdown_requested(&shutdown).await;

    // End in-flight SSE streams so graceful shutdown can drain the connections
    // instead of waiting forever on a held-open Status page (issue #15).
    shutdown.cancel();

    // Watchdog: if a stalled client keeps a connection open past the grace
    // period (axum waits for *all* of them), exit anyway — on the handheld the
    // exit key must actually exit (issue #34).
    tokio::spawn(async {
        tokio::time::sleep(DRAIN_GRACE).await;
        eprintln!("shutdown: connections did not drain within {DRAIN_GRACE:?}; exiting now");
        std::process::exit(0);
    });
}

/// Resolves when anything asks the server to stop: Ctrl+C (SIGINT), SIGTERM
/// (Unix service managers — Docker/systemd/NAS, issue #34), or the shared
/// CancellationToken (the gamepad exit key and the SDL window-close path
/// cancel it instead of calling `std::process::exit`, so in-flight uploads
/// and WebDAV writes drain instead of dying mid-stream).
async fn shutdown_requested(shutdown: &CancellationToken) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    // SIGTERM is the stop signal Docker/systemd/Kubernetes send on the
    // headless server deployments; without this branch those get a hard kill
    // with no connection draining.
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            // Installing the handler essentially never fails; if it somehow
            // does, keep serving on the remaining branch instead of aborting.
            Err(e) => {
                eprintln!("shutdown: cannot install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
        () = shutdown.cancelled() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // The device exit paths (gamepad exit key, SDL window close) cancel the
    // shared token instead of calling std::process::exit; the serve loop's
    // shutdown future must resolve from that alone, no OS signal involved
    // (issue #34).
    #[tokio::test]
    async fn shutdown_requested_resolves_when_the_token_is_cancelled() {
        let token = CancellationToken::new();
        token.cancel();
        tokio::time::timeout(Duration::from_secs(1), shutdown_requested(&token))
            .await
            .expect("shutdown_requested must resolve once the token is cancelled");
    }

    // …and it must NOT resolve on its own: an un-cancelled token with no
    // signal keeps the server running.
    #[tokio::test]
    async fn shutdown_requested_pends_until_something_asks() {
        let token = CancellationToken::new();
        let waited =
            tokio::time::timeout(Duration::from_millis(50), shutdown_requested(&token)).await;
        assert!(
            waited.is_err(),
            "shutdown_requested resolved with no signal and no cancellation"
        );
    }

    // First-run config handling on device builds: a failed write must be
    // *returned* (not just logged) so it reaches the device screen and the
    // Status tab via the config_error machinery (issue #35).
    #[cfg(any(feature = "fb", feature = "sdl"))]
    mod ensure_default_config {
        use crate::ensure_default_config;

        fn tmp(name: &str) -> std::path::PathBuf {
            std::env::temp_dir().join(format!("amberdav-test-{}-{name}", std::process::id()))
        }

        #[test]
        fn write_failure_is_reported() {
            // A regular file where a directory is needed makes the save fail,
            // standing in for a read-only/full SD card.
            let blocker = tmp("blocker");
            std::fs::write(&blocker, b"in the way").unwrap();
            let err = ensure_default_config(&blocker.join("config.json"));
            assert!(
                err.expect("write failure must be reported")
                    .contains("cannot write default config"),
                "message should say what failed"
            );
            let _ = std::fs::remove_file(&blocker);
        }

        #[test]
        fn existing_config_is_left_alone() {
            let path = tmp("existing.jsonc");
            std::fs::write(&path, b"{ // mine\n}").unwrap();
            assert_eq!(ensure_default_config(&path), None);
            assert_eq!(std::fs::read(&path).unwrap(), b"{ // mine\n}");
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn first_run_writes_the_default() {
            let dir = tmp("first-run");
            let _ = std::fs::remove_dir_all(&dir);
            let path = dir.join("config.json");
            assert_eq!(ensure_default_config(&path), None);
            assert!(path.exists(), "default config should have been written");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
