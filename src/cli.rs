//! Command-line interface and settings resolution.
//!
//! Every setting is reachable three ways; this module merges them with a clear
//! precedence (highest → lowest):
//!
//! 1. **CLI flags** — explicit, per-run (`--root`, `--port`, …, or the
//!    positional `root`/`port` aliases).
//! 2. **`AMBERDAV_*` env vars** — deployment/container configuration.
//! 3. **Config file** — persisted user preferences ([`crate::config`]).
//! 4. **Compiled-in defaults** — the `Settings::default()` values.
//!
//! `clap` parses the CLI (and gives `--help`/`--version` for free); the env
//! layer is resolved by hand so the precedence stays predictable even for the
//! positional `root`/`port` args, which `clap`'s native `env` can't order
//! against a separate `--root`/`--port` flag.

use clap::{Parser, ValueEnum};

use crate::config::{Permission, Settings};

/// A tiny WebDAV file server + live gamepad button viewer.
///
/// Serves a directory over WebDAV and a web UI. On Anbernic handhelds (built
/// with `--features handheld`) it also paints connection info to the screen and
/// shows live gamepad input.
#[derive(Parser, Debug)]
#[command(name = "amber-dav", version)]
pub struct Cli {
    /// Directory to serve (positional alias for --root).
    #[arg(value_name = "ROOT")]
    root_pos: Option<String>,
    /// Port to listen on (positional alias for --port).
    #[arg(value_name = "PORT")]
    port_pos: Option<u16>,

    /// Directory to serve. [env: AMBERDAV_ROOT] [default: .]
    #[arg(long, value_name = "PATH")]
    root: Option<String>,
    /// Port to listen on. [env: AMBERDAV_PORT (or PORT)] [default: 8080]
    #[arg(long, value_name = "N")]
    port: Option<u16>,
    /// Address to bind. [env: AMBERDAV_BIND] [default: 0.0.0.0]
    #[arg(long, value_name = "ADDR")]
    bind: Option<String>,
    /// Fixed login password. [env: AMBERDAV_PASSWORD] [default: random per boot]
    #[arg(long, value_name = "PW")]
    password: Option<String>,
    /// Show the password on the device screen. [env: AMBERDAV_DISPLAY_PASSWORD]
    #[arg(long = "display-password", overrides_with = "no_display_password")]
    display_password: bool,
    /// Hide the password on the device screen.
    #[arg(long = "no-display-password", overrides_with = "display_password")]
    no_display_password: bool,
    /// Folder (relative to root) to open after login. [env: AMBERDAV_DEFAULT_FOLDER]
    #[arg(long, value_name = "PATH")]
    default_folder: Option<String>,
    /// Allowed file operations. [env: AMBERDAV_PERMISSION]
    #[arg(long, value_enum, value_name = "LEVEL")]
    permission: Option<Permission>,
    /// Enable the DVD-bounce screensaver. [env: AMBERDAV_BOUNCE_SCREEN]
    #[arg(long = "bounce-screen", overrides_with = "no_bounce_screen")]
    bounce_screen: bool,
    /// Disable the DVD-bounce screensaver.
    #[arg(long = "no-bounce-screen", overrides_with = "bounce_screen")]
    no_bounce_screen: bool,
    /// Bounce-screensaver image folders/files, comma-separated.
    /// [env: AMBERDAV_BOUNCE_FOLDERS]
    #[arg(long = "bounce-folders", value_name = "PATHS", value_delimiter = ',')]
    bounce_folders: Option<Vec<String>>,

    /// Write the fully-resolved configuration to the config file, then exit.
    #[arg(long)]
    pub save: bool,
}

impl Cli {
    /// Merge CLI args and `AMBERDAV_*` env vars on top of the loaded config
    /// file, honouring CLI > env > file > compiled default. A field left unset
    /// at every layer keeps the value already on `s` (the loaded file, or the
    /// compiled default behind it).
    pub fn resolve(&self, mut s: Settings) -> Settings {
        if let Some(v) = self
            .root
            .clone()
            .or_else(|| self.root_pos.clone())
            .or_else(|| env_str("AMBERDAV_ROOT"))
        {
            s.root = Some(v);
        }
        if let Some(v) = self
            .port
            .or(self.port_pos)
            .or_else(|| env_parse("AMBERDAV_PORT"))
            .or_else(|| env_parse("PORT"))
        {
            s.port = Some(v);
        }
        if let Some(v) = self.bind.clone().or_else(|| env_str("AMBERDAV_BIND")) {
            s.bind = Some(v);
        }
        if let Some(v) = self
            .password
            .clone()
            .or_else(|| env_str("AMBERDAV_PASSWORD"))
        {
            s.password = Some(v);
        }
        if let Some(v) = flag_tristate(self.display_password, self.no_display_password)
            .or_else(|| env_bool("AMBERDAV_DISPLAY_PASSWORD"))
        {
            s.display_password = v;
        }
        if let Some(v) = self
            .default_folder
            .clone()
            .or_else(|| env_str("AMBERDAV_DEFAULT_FOLDER"))
        {
            s.default_folder = v;
        }
        if let Some(v) = self
            .permission
            .or_else(|| env_permission("AMBERDAV_PERMISSION"))
        {
            s.permission = v;
        }
        if let Some(v) = flag_tristate(self.bounce_screen, self.no_bounce_screen)
            .or_else(|| env_bool("AMBERDAV_BOUNCE_SCREEN"))
        {
            s.bounce_screen.enabled = v;
        }
        if let Some(v) = self
            .bounce_folders
            .clone()
            .or_else(|| env_list("AMBERDAV_BOUNCE_FOLDERS"))
        {
            s.bounce_screen.folders = v;
        }
        s
    }
}

/// Collapse a `--flag`/`--no-flag` pair (mutually exclusive via `overrides_with`)
/// into a tri-state: `Some(true)`/`Some(false)` if either was given, else `None`.
fn flag_tristate(on: bool, off: bool) -> Option<bool> {
    if on {
        Some(true)
    } else if off {
        Some(false)
    } else {
        None
    }
}

/// A non-empty env var, or `None` (unset or empty so it can't mask a config value).
fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    env_str(key).and_then(|v| v.parse().ok())
}

/// Parse a boolean env var. Accepts the usual on/off spellings; anything else
/// is ignored (treated as unset) rather than silently meaning `false`.
fn env_bool(key: &str) -> Option<bool> {
    match env_str(key)?.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn env_list(key: &str) -> Option<Vec<String>> {
    env_str(key).map(|v| {
        v.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn env_permission(key: &str) -> Option<Permission> {
    Permission::from_str(&env_str(key)?, true).ok()
}
