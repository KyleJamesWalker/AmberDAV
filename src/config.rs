//! Persistent settings, stored as JSONC (JSON plus comments and trailing
//! commas; written with the options documented in comments). The file lives
//! next to the binary on handheld builds and in the platform config directory
//! on desktop/server builds (see [`config_path`]); `$AMBERDAV_CONFIG`
//! overrides either. Loaded at startup; the Settings UI rewrites the file.
//!
//! `permission`, `default_folder`, and `favorites` are read live per request. `password`,
//! `display_password`, and `root` are bound at boot and need a relaunch.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

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

// evdev key-code defaults for the on-device controls. These are the raw codes
// evdev reports, so they can be retargeted per device from the config/env/CLI.
/// 354 = KEY_GOTO (the Anbernic menu/function button); 315 = BTN_START (the
/// Steam Deck's ☰ Menu button). Either quits the app.
fn default_exit_keys() -> Vec<u16> {
    vec![354, 315]
}
/// 304 = BTN_SOUTH (the "A" face button) — blanks the screen.
fn default_blank_keys() -> Vec<u16> {
    vec![304]
}
/// 307 = BTN_NORTH (the "X" face button) — toggles the bounce screensaver.
fn default_bounce_keys() -> Vec<u16> {
    vec![307]
}

/// A named folder shortcut shown in the web UI sidebar for one-click
/// navigation. Most useful on device (`fb`/`sdl`) builds browsing a larger
/// tree; CLI folder shares typically leave the list empty.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Favorite {
    /// Label shown in the sidebar.
    pub name: String,
    /// Folder to open, relative to the served root (same convention as
    /// [`Settings::default_folder`]).
    pub path: String,
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
    /// Human-readable device name shown in the browser tab title.
    /// `None`/absent → the subtitle falls back to `"web access"`.
    #[serde(default)]
    pub name: Option<String>,
    /// Fixed login password. `None`/absent → a random one is generated each boot.
    #[serde(default)]
    pub password: Option<String>,
    /// Show the password on the device screen. Forced on when the password is
    /// random (otherwise it would be impossible to discover).
    #[serde(default = "default_true")]
    pub display_password: bool,
    /// Absolute path to serve. `None`/absent → use the CLI argument / default.
    /// Mutually exclusive with `roots` — setting both is a startup error.
    #[serde(default)]
    pub root: Option<String>,
    /// Named mount points: `{"name": "/path", …}`. Mutually exclusive with
    /// `root`. Absent or `null` → use `root`/CLI/default.
    #[serde(default)]
    pub roots: Option<BTreeMap<String, String>>,
    /// Port to listen on. `None`/absent → CLI/env, else 8080.
    #[serde(default)]
    pub port: Option<u16>,
    /// Address to bind. `None`/absent → CLI/env, else `0.0.0.0` (all interfaces).
    #[serde(default)]
    pub bind: Option<String>,
    /// Folder (relative to root) to open after login.
    #[serde(default)]
    pub default_folder: String,
    /// Named folder shortcuts shown in the web UI sidebar, in order. Empty or
    /// absent → no Favorites section is rendered.
    #[serde(default)]
    pub favorites: Vec<Favorite>,
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
    /// evdev key codes that quit the app (any one of them). Lets each device
    /// use its own button; defaults cover the Anbernic and the Steam Deck.
    #[serde(default = "default_exit_keys")]
    pub exit_keys: Vec<u16>,
    /// evdev key codes that blank the screen (toggle the black screen).
    #[serde(default = "default_blank_keys")]
    pub blank_keys: Vec<u16>,
    /// evdev key codes that toggle the bounce screensaver. Only act when
    /// `bounce_screen.enabled` is set.
    #[serde(default = "default_bounce_keys")]
    pub bounce_keys: Vec<u16>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: None,
            password: None,
            display_password: true,
            root: None,
            roots: None,
            port: None,
            bind: None,
            default_folder: String::new(),
            favorites: Vec::new(),
            permission: Permission::ReadWrite,
            bounce_screen: BounceScreen::default(),
            connection_file: None,
            exit_keys: default_exit_keys(),
            blank_keys: default_blank_keys(),
            bounce_keys: default_bounce_keys(),
        }
    }
}

/// Where the config file lives.
///
/// `$AMBERDAV_CONFIG` always wins when set and non-empty. Otherwise:
///
/// - **device builds (`fb`/`sdl`)**: next to the binary. The Anbernic launcher
///   requires the app and its config to live in the same dedicated folder.
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

    #[cfg(device)]
    {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                return dir.join("config.json");
            }
        }
        PathBuf::from("config.json")
    }

    #[cfg(not(device))]
    {
        if let Some(proj) = directories::ProjectDirs::from("", "", "amber-dav") {
            return proj.config_dir().join("config.json");
        }
        PathBuf::from("config.json")
    }
}

/// Load settings. A missing file is normal (first boot) and yields defaults
/// silently; an unreadable or unparseable file also falls back to defaults but
/// returns a human-readable error so callers can surface it loudly — on a
/// handheld, stderr is invisible and a silent fallback looks like the config
/// is simply ignored (issue #19).
///
/// The file is parsed as JSONC: `//` and `/* */` comments and trailing commas
/// are accepted, matching the commented example documented in the README.
pub fn load(path: &Path) -> (Settings, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
        Err(e) => {
            let msg = format!("cannot read {}: {e}; using defaults", path.display());
            tracing::warn!("{msg}");
            return (Settings::default(), Some(msg));
        }
    };
    match parse_jsonc(&text) {
        Ok(s) => (s, None),
        Err(e) => {
            let msg = format!("{} is invalid ({e}); using defaults", path.display());
            tracing::warn!("{msg}");
            (Settings::default(), Some(msg))
        }
    }
}

/// Parse a JSONC document into [`Settings`]. A proper parser (not regex
/// comment-stripping, which would corrupt `//` inside string values) handles
/// the comments and trailing commas; an empty document yields the defaults.
fn parse_jsonc(text: &str) -> Result<Settings, String> {
    let value =
        jsonc_parser::parse_to_serde_value(text, &Default::default()).map_err(|e| e.to_string())?;
    match value {
        Some(v) => serde_json::from_value(v).map_err(|e| e.to_string()),
        None => Ok(Settings::default()),
    }
}

/// Persist settings as a commented JSONC document, creating the parent
/// directory if needed (the platform config dir may not exist on first run).
/// The auto-generated file (first boot on device, `--save`) is the primary way
/// users discover the options, so every field is written with its purpose and
/// allowed values inline — [`load`] accepts the comments back.
pub fn save(path: &Path, s: &Settings) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, to_jsonc_pretty(s))
}

/// Render settings as self-documenting JSONC: actual values, annotated with
/// what each option does and the values it accepts. Comments aside, the output
/// is strict JSON (no trailing commas), so any JSON-with-comments tool can
/// read it back.
fn to_jsonc_pretty(s: &Settings) -> String {
    // Per-field JSON rendering (handles quoting and escaping). These are all
    // plain data types; serializing them cannot fail.
    fn json<T: Serialize>(v: &T) -> String {
        serde_json::to_string(v).expect("plain settings field serializes")
    }

    // One-line key-code list with a space after each comma ("[354, 315]") —
    // serde_json's compact form has none.
    fn keys(v: &[u16]) -> String {
        let items: Vec<String> = v.iter().map(u16::to_string).collect();
        format!("[{}]", items.join(", "))
    }

    let favorites = if s.favorites.is_empty() {
        "[]".to_string()
    } else {
        let rows: Vec<String> = s
            .favorites
            .iter()
            .map(|f| format!("    {}", json(f)))
            .collect();
        format!("[\n{}\n  ]", rows.join(",\n"))
    };

    // Emit either "root" (single path) or "roots" (named mounts), never both.
    let root_section = if let Some(roots) = &s.roots {
        let pairs: Vec<String> = roots
            .iter()
            .map(|(k, v)| format!("    {}: {}", json(k), json(v)))
            .collect();
        format!(
            "// Named mount points. Each key is the URL prefix; value is the directory.\n  // [env: AMBERDAV_ROOT=name=path;name2=path2]  [CLI: --root name=path]\n  \"roots\": {{\n{}\n  }}",
            pairs.join(",\n")
        )
    } else {
        format!(
            "// Absolute path to serve. null = the CLI argument / default (\".\").\n  \"root\": {root}",
            root = json(&s.root)
        )
    };

    format!(
        r#"{{
  // Human-readable device name shown in the browser tab title.
  // null = the subtitle falls back to "web access". Example: "Stream Deck".
  // [env: AMBERDAV_NAME]  [CLI: --name]
  "name": {name},

  // Fixed login password. null = a fresh random code is generated each boot.
  "password": {password},

  // Show the password on the device screen. Forced on for a random password
  // (otherwise it could never be discovered); false hides a fixed one.
  "display_password": {display_password},

  {root_section},

  // Port to listen on. null = CLI/env, else 8080.
  "port": {port},

  // Address to bind. null = "0.0.0.0" (all interfaces); "127.0.0.1" for
  // tunneled/proxied deployments.
  "bind": {bind},

  // Folder (relative to the served root) to open right after login. "" = root.
  "default_folder": {default_folder},

  // Named folder shortcuts shown in the web UI sidebar, in order. Each path is
  // relative to the served root (same convention as default_folder). Example:
  //   "favorites": [
  //     {{ "name": "Game Boy", "path": "Roms/GB" }},
  //     {{ "name": "Screenshots", "path": "Roms/Imgs" }}
  //   ],
  "favorites": {favorites},

  // Allowed operations: "read_only" | "read_write" | "read_write_delete".
  "permission": {permission},

  // Burn-in "DVD bounce" screensaver, toggled on-device with the X button.
  // folders: files or folders (scanned recursively) to draw images from,
  // relative to the served root; absolute paths work too.
  "bounce_screen": {{
    "enabled": {bounce_enabled},
    "folders": {bounce_folders}
  }},

  // Where to write a connection.json sidecar (IP/port/password/URL) for
  // external launchers and Decky. null = not written.
  "connection_file": {connection_file},

  // evdev key codes for the on-device controls; any listed code triggers.
  // exit_keys quit the app (354 = Anbernic menu button, 315 = Steam Deck Menu),
  // blank_keys blank the screen (304 = A), bounce_keys toggle the screensaver
  // (307 = X; only acts when bounce_screen.enabled is true).
  "exit_keys": {exit_keys},
  "blank_keys": {blank_keys},
  "bounce_keys": {bounce_keys}
}}
"#,
        name = json(&s.name),
        password = json(&s.password),
        display_password = json(&s.display_password),
        port = json(&s.port),
        bind = json(&s.bind),
        default_folder = json(&s.default_folder),
        permission = json(&s.permission),
        bounce_enabled = json(&s.bounce_screen.enabled),
        bounce_folders = json(&s.bounce_screen.folders),
        connection_file = json(&s.connection_file),
        exit_keys = keys(&s.exit_keys),
        blank_keys = keys(&s.blank_keys),
        bounce_keys = keys(&s.bounce_keys),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch config file that cleans itself up.
    struct TmpConfig(PathBuf);

    impl TmpConfig {
        fn new(name: &str, contents: &str) -> TmpConfig {
            let path = std::env::temp_dir().join(format!(
                "amberdav-config-test-{}-{name}.json",
                std::process::id()
            ));
            std::fs::write(&path, contents).unwrap();
            TmpConfig(path)
        }
    }

    impl Drop for TmpConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Settings has no PartialEq; compare through the serialized value.
    fn assert_settings_eq(a: &Settings, b: &Settings) {
        assert_eq!(
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b).unwrap()
        );
    }

    // The auto-generated config (first boot on device, `--save`) should be
    // self-documenting now that the parser accepts comments: every option
    // present, annotated, with the allowed values spelled out (issue #19
    // follow-up).
    #[test]
    fn save_writes_commented_self_documenting_config() {
        let tmp = TmpConfig::new("commented", "");
        save(&tmp.0, &Settings::default()).unwrap();
        let text = std::fs::read_to_string(&tmp.0).unwrap();
        assert!(text.contains("//"), "generated config has no comments");
        // The permission ladder is the least guessable option — its allowed
        // values must be discoverable from the file itself.
        for level in ["read_only", "read_write", "read_write_delete"] {
            assert!(
                text.contains(level),
                "permission option {level} not documented"
            );
        }
        // Every config key is present so users edit rather than guess names.
        // Default settings emit "root" (single-root form).
        for key in [
            "name",
            "password",
            "display_password",
            "root",
            "port",
            "bind",
            "default_folder",
            "favorites",
            "permission",
            "bounce_screen",
            "connection_file",
            "exit_keys",
            "blank_keys",
            "bounce_keys",
        ] {
            assert!(text.contains(&format!("\"{key}\"")), "key {key} missing");
        }
    }

    #[test]
    fn save_then_load_round_trips_defaults() {
        let tmp = TmpConfig::new("roundtrip-default", "");
        save(&tmp.0, &Settings::default()).unwrap();
        let (loaded, err) = load(&tmp.0);
        assert!(
            err.is_none(),
            "generated config must parse cleanly: {err:?}"
        );
        assert_settings_eq(&loaded, &Settings::default());
    }

    // `--save` persists fully-resolved (non-default) values; the commented
    // writer must render every field's actual value, including ones needing
    // JSON string escaping.
    #[test]
    fn save_then_load_round_trips_custom_values() {
        let custom = Settings {
            name: Some("Retro Flippy".to_string()),
            password: Some("li\"ttle\\Secr3t".to_string()),
            display_password: false,
            root: Some("/mnt/mmc".to_string()),
            roots: None,
            port: Some(9090),
            bind: Some("127.0.0.1".to_string()),
            default_folder: "Roms".to_string(),
            favorites: vec![
                Favorite {
                    name: "Game Boy".to_string(),
                    path: "Roms/GB".to_string(),
                },
                Favorite {
                    name: "Screenshots".to_string(),
                    path: "Roms/Imgs".to_string(),
                },
            ],
            permission: Permission::ReadWriteDelete,
            bounce_screen: BounceScreen {
                enabled: true,
                folders: vec!["Roms/GBA/Imgs".to_string()],
            },
            connection_file: Some("connection.json".to_string()),
            exit_keys: vec![1, 2],
            blank_keys: vec![3],
            bounce_keys: vec![4, 5],
        };
        let tmp = TmpConfig::new("roundtrip-custom", "");
        save(&tmp.0, &custom).unwrap();
        let (loaded, err) = load(&tmp.0);
        assert!(
            err.is_none(),
            "generated config must parse cleanly: {err:?}"
        );
        assert_settings_eq(&loaded, &custom);
    }

    #[test]
    fn load_missing_file_is_default_without_error() {
        let path = std::env::temp_dir().join(format!(
            "amberdav-config-test-{}-does-not-exist.json",
            std::process::id()
        ));
        let (s, err) = load(&path);
        assert!(err.is_none(), "missing file is not an error: {err:?}");
        assert!(s.password.is_none());
        assert_eq!(s.exit_keys, default_exit_keys());
    }

    #[test]
    fn load_strict_json_parses_without_error() {
        let tmp = TmpConfig::new(
            "strict",
            r#"{ "password": "pw", "default_folder": "Roms" }"#,
        );
        let (s, err) = load(&tmp.0);
        assert!(
            err.is_none(),
            "valid JSON must not report an error: {err:?}"
        );
        assert_eq!(s.password.as_deref(), Some("pw"));
        assert_eq!(s.default_folder, "Roms");
    }

    // The README documents the config as JSONC: `//` comments and a fully
    // commented example. A trailing comma is exactly what bit the issue-19
    // reporter. Both must parse (issue #19).
    #[test]
    fn load_jsonc_comments_and_trailing_commas_parse() {
        let tmp = TmpConfig::new(
            "jsonc",
            r#"{
  // Fixed login password.
  "password": "littleSecr3t",
  "default_folder": "Roms",
  "favorites": [
    { "name": "Game Boy", "path": "Roms/GB" },
    { "name": "Screenshots", "path": "Roms/Imgs" },
  ],
  "permission": "read_write_delete",
}"#,
        );
        let (s, err) = load(&tmp.0);
        assert!(err.is_none(), "JSONC must parse cleanly: {err:?}");
        assert_eq!(s.password.as_deref(), Some("littleSecr3t"));
        assert_eq!(s.favorites.len(), 2);
        assert_eq!(s.favorites[1].path, "Roms/Imgs");
        assert!(s.permission.can_delete());
    }

    // Comment markers inside string values must survive parsing (the reason a
    // regex comment-stripper is not acceptable).
    #[test]
    fn load_jsonc_preserves_comment_markers_in_strings() {
        let tmp = TmpConfig::new(
            "strings",
            r#"{ "password": "se//cret", "default_folder": "a/*b*/c" }"#,
        );
        let (s, err) = load(&tmp.0);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(s.password.as_deref(), Some("se//cret"));
        assert_eq!(s.default_folder, "a/*b*/c");
    }

    #[test]
    fn load_invalid_file_falls_back_and_reports_error() {
        let tmp = TmpConfig::new("invalid", r#"{ "password": "unterminated }"#);
        let (s, err) = load(&tmp.0);
        let err = err.expect("an unparseable config must surface an error");
        // The message must identify the file so the user knows what to fix.
        assert!(
            err.contains("config.json") || err.contains(&tmp.0.display().to_string()),
            "error should name the config file: {err}"
        );
        // Fallback settings are the compiled-in defaults.
        assert!(s.password.is_none());
        assert_eq!(s.permission, Permission::ReadWrite);
    }

    // Valid JSONC with a field of the wrong type is still an invalid config and
    // must be reported, not silently defaulted.
    #[test]
    fn load_wrong_type_reports_error() {
        let tmp = TmpConfig::new("wrongtype", r#"{ "port": "eight-thousand" }"#);
        let (s, err) = load(&tmp.0);
        assert!(err.is_some(), "type mismatch must surface an error");
        assert!(s.port.is_none());
    }

    // The "roots" object form round-trips through save + load (issue #76).
    #[test]
    fn save_then_load_round_trips_roots() {
        let mut roots_map = BTreeMap::new();
        roots_map.insert("roms".to_string(), "/mnt/sd/Roms".to_string());
        roots_map.insert("saves".to_string(), "/mnt/sd/Saves".to_string());
        let custom = Settings {
            roots: Some(roots_map),
            root: None,
            ..Settings::default()
        };
        let tmp = TmpConfig::new("roundtrip-roots", "");
        save(&tmp.0, &custom).unwrap();
        let text = std::fs::read_to_string(&tmp.0).unwrap();
        assert!(
            text.contains("\"roots\""),
            "saved config must contain roots key"
        );
        assert!(
            !text.contains("\"root\":"),
            "must not emit both root and roots"
        );
        let (loaded, err) = load(&tmp.0);
        assert!(err.is_none(), "roots config must parse cleanly: {err:?}");
        assert_settings_eq(&loaded, &custom);
    }

    // A JSONC config with the "roots" object form parses correctly.
    #[test]
    fn load_roots_object_parses() {
        let tmp = TmpConfig::new(
            "roots-obj",
            r#"{ "roots": { "one": "/path/one", "data": "/path/data" } }"#,
        );
        let (s, err) = load(&tmp.0);
        assert!(err.is_none(), "roots config must parse cleanly: {err:?}");
        let roots = s.roots.expect("roots should be present");
        assert_eq!(roots.get("one").map(|s| s.as_str()), Some("/path/one"));
        assert_eq!(roots.get("data").map(|s| s.as_str()), Some("/path/data"));
    }
}
