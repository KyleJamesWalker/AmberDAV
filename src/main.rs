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
    #[cfg(any(feature = "fb", feature = "sdl"))]
    if !config_path.exists() {
        match config::save(&config_path, &config::Settings::default()) {
            Ok(()) => eprintln!("config: wrote default {}", config_path.display()),
            Err(e) => eprintln!("config: could not write {}: {e}", config_path.display()),
        }
    }

    // Resolve settings: CLI args and AMBERDAV_* env vars merged on top of the
    // config file (CLI > env > file > default). This also fixes the old bug
    // where the config `root` silently overrode the CLI argument.
    //
    // A broken config falls back to defaults but the error is carried along
    // and surfaced on the device screen and the web Status tab — on a handheld
    // stderr is invisible, so a silent fallback would look like the config is
    // simply ignored (issue #19).
    let (file_settings, config_error) = config::load(&config_path);
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
    );

    let screen_status: screen::Status = Arc::new(std::sync::Mutex::new("starting…".to_string()));

    // Cancelled on Ctrl+C so in-flight SSE streams end and graceful shutdown
    // can drain instead of hanging on a held-open Status page (issue #15).
    let shutdown = CancellationToken::new();

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
    // Paint the connection info + QR onto the device screen (password hidden
    // when configured, but only ever allowed when it's a fixed password).
    screen::show(
        port,
        shown_password,
        screen_status,
        screen_mode,
        bounce_paths,
        config_error,
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

async fn shutdown_signal(shutdown: CancellationToken) {
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
    }

    // End in-flight SSE streams so graceful shutdown can drain the connections
    // instead of waiting forever on a held-open Status page (issue #15).
    shutdown.cancel();
}
