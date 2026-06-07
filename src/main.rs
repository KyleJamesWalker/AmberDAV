//! amber-dav: a tiny WebDAV file server + live gamepad button viewer for
//! Anbernic handhelds (Allwinner H700, aarch64 Linux).

mod auth;
mod config;
mod files;
mod input;
mod password;
mod screen;
mod ui;
mod webdav;

use std::{net::IpAddr, path::PathBuf, sync::Arc};

use config::Settings;

use axum::{
    extract::FromRef,
    routing::{any, get, post, put},
    Router,
};
use tokio::sync::broadcast;

use input::InputUpdate;
use webdav::DavState;

/// Server facts shown on the status page and startup banner.
pub struct ServerInfo {
    pub port: u16,
    pub password: String,
}

/// Resolve the device's current LAN IP. Re-queried live (not cached at boot)
/// so the screen/info recover once Wi-Fi connects after launch.
pub fn current_ip() -> IpAddr {
    local_ip_address::local_ip().unwrap_or(IpAddr::from([0, 0, 0, 0]))
}

/// Settings, loaded once at boot from the config file (file-owned, read-only
/// at runtime — the UI only displays them).
pub type SharedSettings = Arc<Settings>;

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    /// Canonical directory served over WebDAV and the file API.
    pub root: Arc<PathBuf>,
    /// Opaque per-boot session token handed out on successful login.
    pub session: Arc<str>,
    /// Settings loaded from the config file.
    pub settings: SharedSettings,
    pub dav: DavState,
    pub info: Arc<ServerInfo>,
    pub events: broadcast::Sender<InputUpdate>,
    pub screen_status: screen::Status,
}

impl AppState {
    /// Configured permission level.
    pub fn permission(&self) -> config::Permission {
        self.settings.permission
    }
}

// Lets the WebDAV route extract just its slice of state.
impl FromRef<AppState> for DavState {
    fn from_ref(state: &AppState) -> Self {
        state.dav.clone()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Args: [root_dir] [port]. Defaults serve the current dir on :8080.
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".to_string());
    let port: u16 = args
        .next()
        .and_then(|p| p.parse().ok())
        .or_else(|| std::env::var("PORT").ok().and_then(|p| p.parse().ok()))
        .unwrap_or(8080);

    // Load settings; on first run, write a default file the user can edit.
    let config_path = config::config_path();
    if !config_path.exists() {
        match config::save(&config_path, &Settings::default()) {
            Ok(()) => eprintln!("config: wrote default {}", config_path.display()),
            Err(e) => eprintln!("config: could not write {}: {e}", config_path.display()),
        }
    }
    let settings = config::load(&config_path);

    // Effective root: config overrides the CLI arg when set.
    let root = settings
        .root
        .clone()
        .filter(|r| !r.is_empty())
        .unwrap_or(root);

    // Effective password: fixed from config, else a fresh random one.
    let random_password = settings
        .password
        .as_deref()
        .map(str::is_empty)
        .unwrap_or(true);
    let password = settings
        .password
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| password::generate(5));

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
    input::spawn(events.clone(), screen_mode.clone(), bounce_enabled);

    let screen_status: screen::Status = Arc::new(std::sync::Mutex::new("starting…".to_string()));

    let state = AppState {
        root: Arc::new(root_path),
        session: Arc::from(session.as_str()),
        settings: settings.clone(),
        dav: DavState {
            handler: webdav::build_handler(&root),
            password: Arc::from(password.as_str()),
            settings: settings.clone(),
        },
        info: Arc::new(ServerInfo {
            port,
            password: password.clone(),
        }),
        events,
        screen_status: screen_status.clone(),
    };

    let app = Router::new()
        .route("/", get(ui::index))
        .route("/login", get(ui::login_page).post(auth::login))
        .route("/logout", get(auth::logout))
        .route("/events", get(ui::events))
        .route("/api/info", get(ui::info))
        .route("/api/list", get(files::list))
        .route("/api/download", get(files::download))
        .route("/api/zip", get(files::zip))
        .route("/api/raw", get(files::raw))
        .route("/api/upload", put(files::upload))
        .route("/api/mkdir", post(files::mkdir))
        .route("/api/delete", post(files::delete))
        .route("/api/rename", post(files::rename))
        .route("/api/move", post(files::move_))
        .route("/api/copy", post(files::copy))
        .route("/api/settings", get(ui::get_settings))
        // `any` routes every method — including WebDAV's PROPFIND/MKCOL/etc.
        // The wildcard matches one-or-more segments, so the collection root
        // (`/dav` and `/dav/`) needs its own routes for clients to mount it.
        .route(webdav::MOUNT, any(webdav::route))
        .route(&format!("{}/", webdav::MOUNT), any(webdav::route))
        .route(&format!("{}/{{*rest}}", webdav::MOUNT), any(webdav::route))
        .with_state(state);

    print_banner(ip, port, &root, &password);
    // Paint the connection info + QR onto the device screen (password hidden
    // when configured, but only ever allowed when it's a fixed password).
    let screen_pw = display_password.then(|| password.clone());
    screen::show(port, screen_pw, screen_status, screen_mode, bounce_paths);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
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

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
