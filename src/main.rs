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
#[cfg(all(target_os = "linux", device))]
mod bounce;
mod canvas;
mod cli;
mod config;
mod connection;
mod display;
mod files;
mod input;
mod logging;
mod password;
mod render;
mod router;
mod screen;
#[cfg(all(target_os = "linux", feature = "sdl"))]
mod sdl;
mod state;
mod throttle;
mod ui;
mod update;
mod version;
#[cfg(all(target_os = "linux", feature = "fb", not(feature = "sdl")))]
mod wayland;
mod webdav;

use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

use clap::Parser;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use input::InputUpdate;
use state::{current_ip, AppState, MountTable, ServerInfo, SharedSettings};
use webdav::{DavFs, DavState};

/// Random-password length: 8 chars from the 31-symbol charset is ~40 bits —
/// combined with the per-IP login throttle this puts brute force far out of
/// reach on a hostile LAN while staying easy to read off the device screen
/// and type (issue #27).
const PASSWORD_LEN: usize = 8;

/// Session-token length: never displayed or typed, so it can be long enough
/// (~160 bits) that guessing the cookie is hopeless.
const SESSION_TOKEN_LEN: usize = 32;

/// Input-event broadcast depth. A slow SSE consumer that falls more than
/// this many events behind starts losing the oldest ones (the live viewer
/// is diagnostic; freshness beats completeness).
const INPUT_EVENT_BUFFER: usize = 256;

/// Effective boot values: what the server actually runs with after the
/// settings ladder (CLI > env > file > default, [`cli::Cli::resolve`]) is
/// topped with the compiled fallbacks and per-boot derivations.
struct Effective {
    /// The primary root path string (first or only entry), used for the
    /// startup banner and screensaver path resolution.
    root: String,
    /// Ordered named mounts: `(name, path_string)`. Single root → one entry
    /// with an empty name. Set by [`resolve_mounts`].
    mounts: Vec<(String, String)>,
    port: u16,
    bind: String,
    password: crate::password::PasswordMatcher,
    is_random: bool,
    /// Forced true for a random password — it must be shown somewhere, or it
    /// could never be discovered.
    display_password: bool,
    bounce_enabled: bool,
    /// Screensaver sources with relative entries resolved against the primary
    /// root; empty when the screensaver is disabled.
    bounce_paths: Vec<PathBuf>,
}

/// True when `name` can serve as a mount name. Mount names are matched as
/// exactly one URL path segment ([`state::MountTable::resolve`] and the
/// WebDAV dispatch), so a name containing a separator, or spelling `.`/`..`,
/// could never be addressed — reject it at startup instead of serving a
/// silently unreachable mount. Spaces and non-ASCII are fine (percent-encoded
/// on the wire).
fn valid_mount_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(|c| c.is_control())
}

/// Auto-mounts for the Windows all-drives virtual root (issue #76): one named
/// mount per set bit in the `GetLogicalDrives` mask (bit 0 = `A:`, bit 1 =
/// `B:`, …). Pure so the mapping is testable on every host; only the
/// `cfg(windows)` call site queries the real mask.
#[cfg_attr(not(windows), allow(dead_code))]
fn all_drive_mounts(mask: u32) -> Vec<(String, String)> {
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| {
            let letter = (b'A' + i) as char;
            (letter.to_string(), format!("{letter}:\\"))
        })
        .collect()
}

/// True when the resolved mounts are the single anonymous root `/` (or `\`) —
/// the spelling that means "share everything". On Windows, where no real
/// all-encompassing root exists, [`main`] expands this into per-drive
/// auto-mounts; this deliberately overrides Win32's native meaning of `/`
/// ("root of the current drive"), which `C:\` still spells explicitly.
#[cfg_attr(not(windows), allow(dead_code))]
fn all_drives_requested(mounts: &[(String, String)]) -> bool {
    matches!(mounts, [(name, path)] if name.is_empty() && (path == "/" || path == "\\"))
}

/// The Win32 logical-drive bitmask (kernel32). Declared directly so the
/// dependency-free static build stays that way.
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetLogicalDrives() -> u32;
}

/// Parse the `root`/`roots` settings into an ordered `(name, path)` list.
///
/// Single root → one entry with an empty name. Multi-root → one entry per
/// named mount in alphabetical order (BTreeMap iteration order). This is pure
/// so the validation and parsing are unit-testable.
///
/// Returns `Err(message)` when the config is invalid (both `root` and `roots`
/// set, or a multi-root mount name that can't be one URL segment).
fn resolve_mounts(settings: &config::Settings) -> Result<Vec<(String, String)>, String> {
    match (&settings.root, &settings.roots) {
        (Some(_), Some(_)) => Err(
            "config error: both \"root\" and \"roots\" are set; use one or the other".to_string(),
        ),
        (None, Some(roots)) if roots.is_empty() => {
            // Empty roots map: fall back to default single root.
            Ok(vec![("".to_string(), ".".to_string())])
        }
        (None, Some(roots)) if roots.len() == 1 => {
            // Single entry in roots: treat as single root (no mount prefix).
            let (name, path) = roots.iter().next().unwrap();
            if !name.is_empty() {
                tracing::info!("single root: mount name \"{name}\" ignored; content served at /");
            }
            let path = if path.is_empty() { "." } else { path };
            Ok(vec![("".to_string(), path.to_string())])
        }
        (None, Some(roots)) => {
            // Multi-root mode.
            if let Some((bad, _)) = roots.iter().find(|(n, _)| !valid_mount_name(n)) {
                return Err(format!(
                    "config error: invalid mount name {bad:?} — a mount name must be a \
                     single path segment (non-empty, no '/' or '\\', not '.' or '..')"
                ));
            }
            Ok(roots
                .iter()
                .map(|(n, p)| {
                    (
                        n.clone(),
                        if p.is_empty() {
                            ".".to_string()
                        } else {
                            p.clone()
                        },
                    )
                })
                .collect())
        }
        // Single root (from `root` field, or neither set → default).
        (maybe_root, None) => {
            let path = maybe_root
                .as_deref()
                .filter(|r| !r.is_empty())
                .unwrap_or(".");
            Ok(vec![("".to_string(), path.to_string())])
        }
    }
}

/// Resolve a virtual `/`-separated path against a `(name, path)` mount list:
/// the single anonymous root maps directly; in multi-root the first segment
/// selects the mount. The boot-time counterpart of
/// [`state::MountTable::resolve`] (which needs canonicalized [`PathBuf`]s
/// that don't exist yet at this layer).
fn resolve_virtual(mounts: &[(String, String)], rel: &str) -> Option<PathBuf> {
    match mounts {
        [(name, path)] if name.is_empty() => Some(Path::new(path).join(rel)),
        _ => {
            let (mount, rest) = rel.split_once('/').unwrap_or((rel, ""));
            let (_, path) = mounts.iter().find(|(n, _)| n == mount)?;
            let base = PathBuf::from(path);
            Some(if rest.is_empty() {
                base
            } else {
                base.join(rest)
            })
        }
    }
}

/// The second resolution layer (issue #54): derive the effective boot values
/// from resolved settings. Pure — the random password is injected via
/// `generate`, called only when no fixed password is configured — so the
/// fallback rules are unit-testable.
fn effective(
    settings: &config::Settings,
    generate: impl FnOnce() -> String,
) -> Result<Effective, String> {
    let mounts = resolve_mounts(settings)?;
    // Primary root: first entry's path.
    let root = mounts.first().map(|(_, p)| p.clone()).unwrap_or_default();

    let port = settings.port.unwrap_or(8080);
    let bind = settings
        .bind
        .clone()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_string());

    // Fixed from config when non-empty, else a fresh random one per boot.
    let (password, is_random) = if let Some(hash) = settings
        .password_hash
        .clone()
        .filter(|h| !h.is_empty())
    {
        (crate::password::PasswordMatcher::Hash(hash), false)
    } else if let Some(plain) = settings
        .password
        .clone()
        .filter(|p| !p.is_empty())
    {
        (crate::password::PasswordMatcher::Plain(plain), false)
    } else {
        (crate::password::PasswordMatcher::Plain(generate()), true)
    };

    let bounce_enabled = settings.bounce_screen.enabled;
    let bounce_paths: Vec<PathBuf> = if bounce_enabled {
        settings
            .bounce_screen
            .folders
            .iter()
            .filter(|f| !f.is_empty())
            .filter_map(|f| {
                let p = PathBuf::from(f);
                if p.is_absolute() {
                    Some(p)
                } else {
                    // Relative entries resolve against the VIRTUAL root, same
                    // convention as default_folder: single root → under that
                    // root; multi-root → the first segment names the mount
                    // ("one/roms" → <one's path>/roms). A relative path that
                    // names no mount points at nothing in the virtual tree.
                    resolve_virtual(&mounts, f).or_else(|| {
                        tracing::warn!("bounce folder {f:?} matches no mount; ignored");
                        None
                    })
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    Ok(Effective {
        root,
        mounts,
        port,
        bind,
        password,
        is_random,
        display_password: is_random || settings.display_password,
        bounce_enabled,
        bounce_paths,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = cli::Cli::parse();
    // First thing, so the config-load diagnostics below already go through
    // the subscriber. The banner/QR stay plain println! (user-facing output).
    logging::init(cli.verbose);

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
    let mut settings = match cli.resolve(file_settings) {
        Ok(s) => s,
        Err(msg) => {
            tracing::error!("{msg}");
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
    };

    if let Some(mut plain) = cli.hash_password.clone() {
        if plain.is_empty() {
            match rpassword::prompt_password("Enter password to hash: ") {
                Ok(p) => {
                    plain = p;
                }
                Err(e) => {
                    eprintln!("error: failed to read password: {e}");
                    std::process::exit(1);
                }
            }
        }
        match password::hash(&plain) {
            Ok(h) => {
                if cli.save {
                    settings.password_hash = Some(h);
                    settings.password = None;
                    if let Err(e) = config::save(&config_path, &settings) {
                        eprintln!("error: failed to save config: {e}");
                        std::process::exit(1);
                    }
                    println!("config: wrote {}", config_path.display());
                } else {
                    println!("{h}");
                }
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("error: failed to hash password: {e}");
                std::process::exit(1);
            }
        }
    }

    // --save: persist the fully-resolved config and exit. No server is started.
    if cli.save {
        config::save(&config_path, &settings)?;
        println!("config: wrote {}", config_path.display());
        return Ok(());
    }

    // Second resolution layer: settings -> effective boot values. Pure and
    // unit-tested (issue #54); the randomness is injected.
    let Effective {
        root,
        mounts: mount_specs,
        port,
        bind,
        password,
        is_random,
        display_password,
        bounce_enabled,
        bounce_paths,
    } = match effective(&settings, || password::generate(PASSWORD_LEN)) {
        Ok(e) => e,
        Err(msg) => {
            tracing::error!("{msg}");
            eprintln!("error: {msg}");
            std::process::exit(1);
        }
    };

    // Windows all-drives (issue #76): `--root /` (or `\`) means "share
    // everything"; with no real filesystem root to bind, expand it into one
    // auto-mount per logical drive. Enumerated once at boot — hot-plugged
    // drives appear after a relaunch, consistent with root binding at boot.
    #[cfg(windows)]
    let mount_specs = if all_drives_requested(&mount_specs) {
        let drives = all_drive_mounts(unsafe { GetLogicalDrives() });
        if drives.is_empty() {
            tracing::error!("all-drives root: no logical drives reported; check permissions");
            eprintln!("error: all-drives root: no logical drives reported");
            std::process::exit(1);
        }
        drives
    } else {
        mount_specs
    };

    // A specific bind address (not 0.0.0.0/::) is the only address that
    // accepts connections — pin it now so every user-facing surface (banner +
    // QR, device screen, /api/info, connection.json) advertises it instead of
    // a detected LAN IP that would refuse to connect (issue #59).
    state::pin_advertised_ip(&bind);

    // Long, unguessable session token (never shown; lives only in the cookie).
    let session = password::generate(SESSION_TOKEN_LEN);
    let ip = current_ip();

    // Canonicalize every mount root for stable path-safety comparisons.
    // Overlapping/nested mounts are a config error (same files at two virtual
    // paths → ambiguous rename/delete); check after canonicalization.
    let mut canon_mounts: Vec<(String, PathBuf)> = mount_specs
        .iter()
        .map(|(name, path)| {
            let canon = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
            (name.clone(), canon)
        })
        .collect();

    // Detect overlapping roots only in multi-root mode (single-root is always fine).
    if canon_mounts.len() > 1 {
        let paths: Vec<&PathBuf> = canon_mounts.iter().map(|(_, p)| p).collect();
        for i in 0..paths.len() {
            for j in 0..paths.len() {
                if i != j && (paths[i].starts_with(paths[j]) || paths[j].starts_with(paths[i])) {
                    let msg = format!(
                        "config error: mount \"{}\" ({}) and mount \"{}\" ({}) overlap or are nested — \
                         use non-overlapping directories",
                        canon_mounts[i].0, paths[i].display(),
                        canon_mounts[j].0, paths[j].display(),
                    );
                    tracing::error!("{msg}");
                    eprintln!("error: {msg}");
                    std::process::exit(1);
                }
            }
        }
    }

    // Duplicate mount names cannot reach this point: CLI/env lists error in
    // `cli::apply_root_entries`, and the config `roots` map has unique keys.

    let mount_table = if canon_mounts.len() == 1 && canon_mounts[0].0.is_empty() {
        MountTable::single(canon_mounts.remove(0).1)
    } else {
        MountTable::multi(canon_mounts)
    };

    let settings: SharedSettings = Arc::new(settings);

    // Shared screen mode, driven by the gamepad (A = blank, X = screensaver).
    let screen_mode = screen::mode_handle();

    // Cancelled on Ctrl+C/SIGTERM — and *by* the device exit paths (gamepad
    // exit key, SDL window close) — so in-flight SSE streams end and graceful
    // shutdown can drain instead of hanging on a held-open Status page or
    // killing uploads mid-write (issues #15, #34).
    let shutdown = CancellationToken::new();

    // Broadcast channel carrying input events to all connected SSE clients.
    let (events, _) = broadcast::channel::<InputUpdate>(INPUT_EVENT_BUFFER);
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

    // Build the DAV filesystem layer.
    let dav_fs = if mount_table.is_single() {
        DavFs::Single(webdav::build_single_handler(&root))
    } else {
        webdav::build_multi_fs(mount_table.mounts())
    };

    let state = AppState {
        mounts: Arc::new(mount_table),
        session: Arc::from(session.as_str()),
        settings: settings.clone(),
        dav: DavState {
            fs: dav_fs,
            password: Arc::new(password.clone()),
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
    let shown_password = match &password {
        crate::password::PasswordMatcher::Plain(plain) if display_password => Some(plain.clone()),
        _ => None,
    };

    // Optional sidecar for external launchers / Decky. Honors the hidden-pw
    // rule. Written now and then kept fresh by a background task: Wi-Fi often
    // associates after launch, so the boot-time IP can be 0.0.0.0 and would
    // otherwise be served to launchers forever (issue #48).
    if let Some(cf) = settings
        .connection_file
        .as_deref()
        .filter(|p| !p.is_empty())
    {
        connection::spawn_refresher(
            std::path::PathBuf::from(cf),
            port,
            shown_password.clone(),
            shutdown.clone(),
        );
    }

    print_banner(ip, port, &root, &password, is_random);

    // Bind BEFORE painting the device screen: a failed bind must not flash the
    // normal info screen (which implies the server is up) and then dump the
    // user back at the OS menu with the only evidence buried in log.txt. On
    // failure the same screen machinery paints the error instead, holds it
    // long enough to read, then exits (issue #35).
    let listener = match tokio::net::TcpListener::bind((bind.as_str(), port)).await {
        Ok(l) => l,
        Err(e) => {
            let msg = bind_error_message(&bind, port, &e);
            tracing::error!("{msg}");
            screen::show(
                port,
                None,
                screen_status,
                screen_mode,
                Vec::new(),
                Some(format!("Cannot start server\n{msg}")),
                shutdown,
            );
            // Give a handheld user time to read the panel before the process
            // (and with it the screen) is gone; headless builds exit at once.
            #[cfg(all(target_os = "linux", device))]
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            std::process::exit(1);
        }
    };

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
#[cfg(device)]
fn ensure_default_config(path: &std::path::Path) -> Option<String> {
    if path.exists() {
        return None;
    }
    match config::save(path, &config::Settings::default()) {
        Ok(()) => {
            tracing::info!("wrote default config {}", path.display());
            None
        }
        Err(e) => {
            let msg = format!("cannot write default config {}: {e}", path.display());
            tracing::warn!("{msg}");
            Some(msg)
        }
    }
}

/// Desktop/server builds never write a config implicitly (`--save` opts in),
/// so there is nothing to attempt and nothing to report.
#[cfg(not(device))]
fn ensure_default_config(_path: &std::path::Path) -> Option<String> {
    None
}

/// Friendly description of a TCP bind failure. The raw OS error alone
/// (`Os { code: 48, kind: AddrInUse }`) gives no clue what to do about it, so
/// name the likely cause and the knob that changes it: the port being taken
/// by another instance, or a bad `--bind` address (which surfaces as a
/// parse/lookup failure) (issue #35).
fn bind_error_message(bind: &str, port: u16, e: &std::io::Error) -> String {
    let hint = if e.kind() == std::io::ErrorKind::AddrInUse {
        "is another instance running? (change with --port / config \"port\")"
    } else {
        "check the bind address and port (--bind / --port, config \"bind\" / \"port\")"
    };
    format!("cannot listen on {bind}:{port}: {e} — {hint}")
}

fn print_banner(ip: IpAddr, port: u16, root: &str, password: &crate::password::PasswordMatcher, is_random: bool) {
    let status_url = format!("http://{ip}:{port}/");
    println!("\n  amber-dav");
    println!("  serving:  {root}");
    println!("  status:   {status_url}");
    println!("  webdav:   http://{ip}:{port}{}", webdav::MOUNT);
    if is_random {
        if let crate::password::PasswordMatcher::Plain(plain) = password {
            println!("  password: {plain}   (user: anything)");
        }
    } else {
        println!("  password: [configured]   (user: anything)");
    }
    // A loopback address only ever shows up here when the server is bound to
    // one (issue #59) — say so, or the URLs look broken from another device.
    if ip.is_loopback() {
        println!("  note:     bound to {ip} — only this machine can connect");
    }
    println!();

    // A scannable QR to the status page.
    match qrcode::QrCode::new(status_url.as_bytes()) {
        Ok(code) => {
            let art = code
                .render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build();
            println!("{art}\n");
        }
        Err(e) => tracing::warn!("qr unavailable: {e}"),
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
        tracing::warn!("connections did not drain within {DRAIN_GRACE:?}; exiting now");
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
                tracing::warn!("cannot install SIGTERM handler: {e}");
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

    fn no_gen() -> String {
        panic!("generate must not be called when a fixed password is set")
    }

    fn eff(s: &config::Settings, gen: impl FnOnce() -> String) -> Effective {
        effective(s, gen).expect("effective() must not fail in tests with valid settings")
    }

    // Empty/absent settings fall back to the compiled defaults, and an
    // absent password triggers exactly one generation, which forces
    // display_password on (a never-shown random password is undiscoverable).
    #[test]
    fn effective_defaults_and_random_password() {
        let e = eff(&config::Settings::default(), || "gen-pw".to_string());
        assert_eq!(e.root, ".");
        assert_eq!(e.port, 8080);
        assert_eq!(e.bind, "0.0.0.0");
        assert_eq!(e.password, crate::password::PasswordMatcher::Plain("gen-pw".to_string()));
        assert!(e.display_password, "random password must be displayable");
        assert!(e.is_random);
        assert!(!e.bounce_enabled);
        assert!(e.bounce_paths.is_empty());

        // Empty strings count as unset, same as None (a blanked config line).
        let s = config::Settings {
            root: Some(String::new()),
            bind: Some(String::new()),
            password: Some(String::new()),
            password_hash: Some(String::new()),
            ..config::Settings::default()
        };
        let e = eff(&s, || "gen2".to_string());
        assert_eq!((e.root.as_str(), e.bind.as_str()), (".", "0.0.0.0"));
        assert_eq!(e.password, crate::password::PasswordMatcher::Plain("gen2".to_string()));
        assert!(e.is_random);
    }

    // A configured password is used verbatim, never regenerated, and
    // display_password stays whatever the config says.
    #[test]
    fn effective_fixed_password_respects_display_flag() {
        let s = config::Settings {
            password: Some("fixed".to_string()),
            display_password: false,
            root: Some("/srv/files".to_string()),
            port: Some(9000),
            bind: Some("127.0.0.1".to_string()),
            ..config::Settings::default()
        };
        let e = eff(&s, no_gen);
        assert_eq!(e.password, crate::password::PasswordMatcher::Plain("fixed".to_string()));
        assert!(!e.is_random);
        assert!(!e.display_password, "fixed password may stay hidden");
        assert_eq!(e.root, "/srv/files");
        assert_eq!(e.port, 9000);
        assert_eq!(e.bind, "127.0.0.1");
    }

    #[test]
    fn effective_hashed_password() {
        let s = config::Settings {
            password_hash: Some("$argon2id$v=19$m=65536,t=3,p=1$abc".to_string()),
            display_password: true,
            ..config::Settings::default()
        };
        let e = eff(&s, no_gen);
        assert_eq!(
            e.password,
            crate::password::PasswordMatcher::Hash("$argon2id$v=19$m=65536,t=3,p=1$abc".to_string())
        );
        assert!(!e.is_random);
    }

    // Bounce paths resolve only when enabled: relative entries against the
    // served root, absolute ones verbatim, empties dropped; disabled means
    // no paths at all even if folders are configured.
    #[test]
    fn effective_bounce_path_resolution() {
        let mut s = config::Settings {
            root: Some("/srv/files".to_string()),
            ..config::Settings::default()
        };
        s.bounce_screen.enabled = true;
        s.bounce_screen.folders = vec!["covers".to_string(), "/abs/art".to_string(), String::new()];
        let e = eff(&s, || "x".to_string());
        assert!(e.bounce_enabled);
        assert_eq!(
            e.bounce_paths,
            vec![
                PathBuf::from("/srv/files/covers"),
                PathBuf::from("/abs/art")
            ]
        );

        s.bounce_screen.enabled = false;
        let e = eff(&s, || "x".to_string());
        assert!(!e.bounce_enabled);
        assert!(e.bounce_paths.is_empty(), "disabled resolves nothing");
    }

    // Multi-root: relative screensaver entries resolve against the VIRTUAL
    // root — the first segment names the mount ("one/roms" → <one>/roms).
    // A relative entry that names no mount points at nothing in the virtual
    // tree and is dropped; absolute paths stay verbatim (issue #76).
    #[test]
    fn effective_bounce_paths_resolve_against_the_virtual_root() {
        use std::collections::BTreeMap;
        let mut roots = BTreeMap::new();
        roots.insert("one".to_string(), "/srv/one".to_string());
        roots.insert("two".to_string(), "/srv/two".to_string());
        let mut s = config::Settings {
            roots: Some(roots),
            ..config::Settings::default()
        };
        s.bounce_screen.enabled = true;
        s.bounce_screen.folders = vec![
            "one/roms".to_string(),    // mount "one", subfolder roms
            "two".to_string(),         // a mount by itself
            "missing/art".to_string(), // names no mount → dropped
            "/abs/art".to_string(),    // absolute → verbatim
        ];
        let e = eff(&s, || "x".to_string());
        assert_eq!(
            e.bounce_paths,
            vec![
                PathBuf::from("/srv/one/roms"),
                PathBuf::from("/srv/two"),
                PathBuf::from("/abs/art"),
            ]
        );
    }

    // Multi-root via `roots` field: resolve_mounts returns an ordered list of
    // named entries (issue #76).
    #[test]
    fn resolve_mounts_multi_root() {
        use std::collections::BTreeMap;
        let mut roots = BTreeMap::new();
        roots.insert("one".to_string(), "/srv/one".to_string());
        roots.insert("two".to_string(), "/srv/two".to_string());
        let s = config::Settings {
            roots: Some(roots),
            ..config::Settings::default()
        };
        let mounts = resolve_mounts(&s).expect("multi roots must resolve");
        assert_eq!(mounts.len(), 2);
        // BTreeMap is alphabetically ordered.
        assert_eq!(mounts[0], ("one".to_string(), "/srv/one".to_string()));
        assert_eq!(mounts[1], ("two".to_string(), "/srv/two".to_string()));
    }

    // Setting both root and roots is a config error.
    #[test]
    fn resolve_mounts_both_root_and_roots_is_error() {
        use std::collections::BTreeMap;
        let mut roots = BTreeMap::new();
        roots.insert("x".to_string(), "/x".to_string());
        let s = config::Settings {
            root: Some("/y".to_string()),
            roots: Some(roots),
            ..config::Settings::default()
        };
        assert!(resolve_mounts(&s).is_err());
    }

    // A single-entry roots map behaves as single-root (no mount prefix).
    #[test]
    fn resolve_mounts_single_entry_roots_is_single_root() {
        use std::collections::BTreeMap;
        let mut roots = BTreeMap::new();
        roots.insert("only".to_string(), "/srv/one".to_string());
        let s = config::Settings {
            roots: Some(roots),
            ..config::Settings::default()
        };
        let mounts = resolve_mounts(&s).expect("single-entry roots must resolve");
        // Single entry → single-root mode: empty name.
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].0, "");
        assert_eq!(mounts[0].1, "/srv/one");
    }

    // Mount names become URL path segments; anything that can't be one exact
    // segment (empty, separators, `.`/`..`, control chars) would be silently
    // unreachable — reject it at startup instead (issue #76).
    #[test]
    fn resolve_mounts_rejects_invalid_names() {
        use std::collections::BTreeMap;
        for bad in ["", "a/b", "a\\b", ".", "..", "a\nb"] {
            let mut roots = BTreeMap::new();
            roots.insert(bad.to_string(), "/x".to_string());
            roots.insert("ok".to_string(), "/y".to_string());
            let s = config::Settings {
                roots: Some(roots),
                ..config::Settings::default()
            };
            let err = resolve_mounts(&s).expect_err(&format!("name {bad:?} must be rejected"));
            assert!(err.contains("mount name"), "{err}");
        }
        // Spaces and unicode are fine — they are encoded on the wire.
        let mut roots = BTreeMap::new();
        roots.insert("my files".to_string(), "/x".to_string());
        roots.insert("naïve".to_string(), "/y".to_string());
        let s = config::Settings {
            roots: Some(roots),
            ..config::Settings::default()
        };
        resolve_mounts(&s).expect("space/unicode names are valid");
    }

    // Windows all-drives (issue #76): `--root /` expands to one auto-mount per
    // set bit in the GetLogicalDrives mask. Pure, so testable on every host.
    #[test]
    fn all_drive_mounts_follows_the_drive_mask() {
        // C: and D: (bits 2 and 3).
        assert_eq!(
            all_drive_mounts(0b1100),
            vec![
                ("C".to_string(), "C:\\".to_string()),
                ("D".to_string(), "D:\\".to_string()),
            ]
        );
        assert!(all_drive_mounts(0).is_empty());
        // Bit 0 is A:.
        assert_eq!(
            all_drive_mounts(1),
            vec![("A".to_string(), "A:\\".to_string())]
        );
    }

    // The expansion only triggers for the single anonymous root `/` (or `\`);
    // named mounts and ordinary paths pass through untouched.
    #[test]
    fn all_drives_requested_only_for_anonymous_slash_root() {
        let single = |p: &str| vec![("".to_string(), p.to_string())];
        assert!(all_drives_requested(&single("/")));
        assert!(all_drives_requested(&single("\\")));
        assert!(!all_drives_requested(&single("/srv")));
        assert!(!all_drives_requested(&single("C:\\")));
        assert!(!all_drives_requested(&[
            ("a".to_string(), "/".to_string()),
            ("b".to_string(), "/x".to_string()),
        ]));
    }

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

    // The raw OS error for a taken port is useless on a handheld; the message
    // must name the likely cause and the knob that changes it (issue #35).
    #[test]
    fn bind_error_for_port_in_use_suggests_another_instance() {
        let e = std::io::Error::new(std::io::ErrorKind::AddrInUse, "address in use");
        let msg = bind_error_message("0.0.0.0", 8080, &e);
        assert!(msg.starts_with("cannot listen on 0.0.0.0:8080:"), "{msg}");
        assert!(msg.contains("another instance"), "{msg}");
        assert!(msg.contains("--port"), "{msg}");
    }

    // An unparseable --bind fails the same call with a different kind; the
    // hint must point at the bind address, not at a phantom other instance.
    #[test]
    fn bind_error_for_bad_address_points_at_bind() {
        let e = std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "failed to lookup address information",
        );
        let msg = bind_error_message("not-an-ip", 8080, &e);
        assert!(msg.contains("not-an-ip:8080"), "{msg}");
        assert!(msg.contains("--bind"), "{msg}");
        assert!(!msg.contains("another instance"), "{msg}");
    }

    // End to end on a real socket: a port that is actually taken produces the
    // "another instance" wording.
    #[tokio::test]
    async fn binding_a_taken_port_yields_the_friendly_message() {
        let first = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind an ephemeral port");
        let port = first.local_addr().expect("local addr").port();
        let err = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect_err("second bind of the same port must fail");
        let msg = bind_error_message("127.0.0.1", port, &err);
        assert!(msg.contains("another instance"), "{msg}");
    }

    // First-run config handling on device builds: a failed write must be
    // *returned* (not just logged) so it reaches the device screen and the
    // Status tab via the config_error machinery (issue #35).
    #[cfg(device)]
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
