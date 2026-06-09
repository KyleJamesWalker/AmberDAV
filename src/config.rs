//! Persistent settings, stored as JSON. The file lives next to the binary on
//! handheld builds and in the platform config directory on desktop/server
//! builds (see [`config_path`]); `$AMBERDAV_CONFIG` overrides either. Loaded at
//! startup; the Settings UI rewrites the file.
//!
//! `permission` and `default_folder` are read live per request. `password`,
//! `display_password`, and `root` are bound at boot and need a relaunch.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What file operations the web UI / WebDAV mount are allowed to perform.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
// The shared `Read` prefix reads naturally as an escalating capability ladder.
#[allow(clippy::enum_variant_names)]
pub enum Permission {
    #[value(name = "read_only")]
    ReadOnly,
    #[value(name = "read_write")]
    ReadWrite,
    #[value(name = "read_write_delete")]
    ReadWriteDelete,
}

impl Permission {
    /// Create/modify (mkdir, upload, rename, move, copy, WebDAV writes).
    pub fn can_write(self) -> bool {
        self != Permission::ReadOnly
    }
    /// Delete / remove.
    pub fn can_delete(self) -> bool {
        self == Permission::ReadWriteDelete
    }
}

fn default_true() -> bool {
    true
}
fn default_permission() -> Permission {
    Permission::ReadWrite
}

/// Burn-in screensaver: bounce random images (DVD-logo style) across the
/// screen. Toggled on-device with the X button.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BounceScreen {
    /// Allow toggling the bounce screensaver with the X button.
    #[serde(default)]
    pub enabled: bool,
    /// Files or folders to draw from. Folders are scanned (recursively) for
    /// images. Relative entries resolve against the served root.
    #[serde(default)]
    pub folders: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    /// Fixed login password. `None`/absent → a random one is generated each boot.
    #[serde(default)]
    pub password: Option<String>,
    /// Show the password on the device screen. Forced on when the password is
    /// random (otherwise it would be impossible to discover).
    #[serde(default = "default_true")]
    pub display_password: bool,
    /// Absolute path to serve. `None`/absent → use the CLI argument / default.
    #[serde(default)]
    pub root: Option<String>,
    /// Port to listen on. `None`/absent → CLI/env, else 8080.
    #[serde(default)]
    pub port: Option<u16>,
    /// Address to bind. `None`/absent → CLI/env, else `0.0.0.0` (all interfaces).
    #[serde(default)]
    pub bind: Option<String>,
    /// Folder (relative to root) to open after login.
    #[serde(default)]
    pub default_folder: String,
    /// Allowed file operations.
    #[serde(default = "default_permission")]
    pub permission: Permission,
    /// Burn-in "DVD bounce" screensaver configuration.
    #[serde(default)]
    pub bounce_screen: BounceScreen,
    /// Path to write a `connection.json` sidecar (IP/port/password/URL) for
    /// external launchers and Decky. Empty/unset → not written.
    #[serde(default)]
    pub connection_file: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            password: None,
            display_password: true,
            root: None,
            port: None,
            bind: None,
            default_folder: String::new(),
            permission: Permission::ReadWrite,
            bounce_screen: BounceScreen::default(),
            connection_file: None,
        }
    }
}

/// Where the config file lives.
///
/// `$AMBERDAV_CONFIG` always wins when set and non-empty. Otherwise:
///
/// - **handheld builds**: next to the binary. The Anbernic launcher requires
///   the app and its config to live in the same dedicated folder.
/// - **desktop/server builds**: the platform config directory, so a generically
///   named `config.json` never collides with other tools sharing a `bin/`:
///   - macOS: `~/Library/Application Support/amber-dav/config.json`
///   - Windows: `%APPDATA%\amber-dav\config\config.json`
///   - Linux: `$XDG_CONFIG_HOME/amber-dav/config.json` → `~/.config/amber-dav/config.json`
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("AMBERDAV_CONFIG") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }

    #[cfg(feature = "handheld")]
    {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return dir.join("config.json");
            }
        }
        PathBuf::from("config.json")
    }

    #[cfg(not(feature = "handheld"))]
    {
        if let Some(proj) = directories::ProjectDirs::from("", "", "amber-dav") {
            return proj.config_dir().join("config.json");
        }
        PathBuf::from("config.json")
    }
}

/// Load settings, falling back to defaults on a missing or invalid file.
pub fn load(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            eprintln!(
                "config: {} parse error ({e}); using defaults",
                path.display()
            );
            Settings::default()
        }),
        Err(_) => Settings::default(),
    }
}

/// Persist settings as pretty JSON, creating the parent directory if needed
/// (the platform config dir may not exist on first run).
pub fn save(path: &Path, s: &Settings) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(path, json)
}
