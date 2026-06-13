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

use std::collections::BTreeMap;

use clap::{Parser, ValueEnum};

use crate::config::{Permission, Settings};

/// A tiny WebDAV file server + live gamepad button viewer.
///
/// Serves a directory over WebDAV and a web UI. On Anbernic handhelds (built
/// with `--features fb` or `--features sdl`) it also paints connection info to
/// the screen and shows live gamepad input.
#[derive(Parser, Debug)]
#[command(name = "amber-dav", version = crate::version::VERSION)]
pub struct Cli {
    /// Directory to serve (positional alias for --root, single root only).
    #[arg(value_name = "ROOT")]
    root_pos: Option<String>,
    /// Port to listen on (positional alias for --port).
    #[arg(value_name = "PORT")]
    port_pos: Option<u16>,

    /// Directory to serve. May be repeated for named mounts:
    ///   --root /path         (single root, today's behavior)
    ///   --root name=/path    (named mount)
    ///   --root name=/path --root other=/path2   (multi-root)
    /// [env: AMBERDAV_ROOT] [default: .]
    #[arg(long, value_name = "PATH")]
    root: Vec<String>,
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
    /// Write a connection.json sidecar here (IP/port/password/URL). [env: AMBERDAV_CONNECTION_FILE]
    #[arg(long, value_name = "PATH")]
    connection_file: Option<String>,
    /// evdev key codes that quit the app, comma-separated. [env: AMBERDAV_EXIT_KEYS]
    #[arg(long = "exit-keys", value_name = "CODES", value_delimiter = ',')]
    exit_keys: Option<Vec<u16>>,
    /// evdev key codes that blank the screen, comma-separated. [env: AMBERDAV_BLANK_KEYS]
    #[arg(long = "blank-keys", value_name = "CODES", value_delimiter = ',')]
    blank_keys: Option<Vec<u16>>,
    /// evdev key codes that toggle the bounce screensaver, comma-separated. [env: AMBERDAV_BOUNCE_KEYS]
    #[arg(long = "bounce-keys", value_name = "CODES", value_delimiter = ',')]
    bounce_keys: Option<Vec<u16>>,

    /// Write the fully-resolved configuration to the config file, then exit.
    #[arg(long)]
    pub save: bool,

    /// Human-readable device name shown in the browser tab title.
    /// [env: AMBERDAV_NAME] [default: (none — subtitle is "web access")]
    #[arg(long, value_name = "NAME")]
    name: Option<String>,

    /// Verbose (debug-level) logging. [env: AMBERDAV_LOG=error|warn|info|debug|trace]
    #[arg(short, long)]
    pub verbose: bool,
}

impl Cli {
    /// Merge CLI args and `AMBERDAV_*` env vars on top of the loaded config
    /// file, honouring CLI > env > file > compiled default. A field left unset
    /// at every layer keeps the value already on `s` (the loaded file, or the
    /// compiled default behind it).
    ///
    /// `Err` carries a user-facing message for invalid root lists (duplicate
    /// mount names) — a startup error, not something to fall back from.
    pub fn resolve(&self, s: Settings) -> Result<Settings, String> {
        self.resolve_with(s, |key| std::env::var(key).ok())
    }

    /// [`resolve`](Cli::resolve) with the environment lookup injected, so the
    /// precedence rules are testable without mutating the process environment
    /// (`std::env::set_var` is unsafe under parallel tests).
    fn resolve_with(
        &self,
        mut s: Settings,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Settings, String> {
        // A non-empty env var, or `None` (unset or empty so it can't mask a
        // config value).
        let env_str = |key: &str| env(key).filter(|v| !v.is_empty());
        let env_u16 = |key: &str| -> Option<u16> { env_str(key)?.parse().ok() };
        let env_bool = |key: &str| -> Option<bool> { parse_bool(&env_str(key)?) };
        let env_list = |key: &str| -> Option<Vec<String>> {
            Some(
                env_str(key)?
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        };
        // A comma-separated `u16` env var (e.g. evdev key codes). A value with
        // no usable codes is treated as unset, so it can't blank a config list.
        let env_u16_list = |key: &str| -> Option<Vec<u16>> {
            let codes = parse_u16_list(&env_str(key)?);
            (!codes.is_empty()).then_some(codes)
        };
        let env_permission =
            |key: &str| -> Option<Permission> { Permission::from_str(&env_str(key)?, true).ok() };

        // Root resolution: --root (repeatable) > root_pos > AMBERDAV_ROOT env.
        // Any CLI --root entries replace the entire root/roots config; same for
        // env. The positional alias only applies when no --root flags were given
        // and only sets a single root.
        let cli_roots: Vec<String> = if !self.root.is_empty() {
            self.root.clone()
        } else if let Some(pos) = self.root_pos.clone() {
            vec![pos]
        } else {
            vec![]
        };

        if !cli_roots.is_empty() {
            apply_root_entries(&mut s, cli_roots)?;
        } else if let Some(env_val) = env_str("AMBERDAV_ROOT") {
            // `;`-separated list of [NAME=]PATH entries. A single bare path keeps
            // today's single-root behavior unchanged.
            let entries: Vec<String> = env_val
                .split(';')
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .collect();
            if !entries.is_empty() {
                apply_root_entries(&mut s, entries)?;
            }
        }
        if let Some(v) = self
            .port
            .or(self.port_pos)
            .or_else(|| env_u16("AMBERDAV_PORT"))
            .or_else(|| env_u16("PORT"))
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
        if let Some(v) = self
            .connection_file
            .clone()
            .or_else(|| env_str("AMBERDAV_CONNECTION_FILE"))
        {
            s.connection_file = Some(v);
        }
        if let Some(v) = self
            .exit_keys
            .clone()
            .or_else(|| env_u16_list("AMBERDAV_EXIT_KEYS"))
            // Back-compat: honour the old singular AMBERDAV_EXIT_KEY too.
            .or_else(|| env_u16("AMBERDAV_EXIT_KEY").map(|k| vec![k]))
        {
            s.exit_keys = v;
        }
        if let Some(v) = self
            .blank_keys
            .clone()
            .or_else(|| env_u16_list("AMBERDAV_BLANK_KEYS"))
        {
            s.blank_keys = v;
        }
        if let Some(v) = self
            .bounce_keys
            .clone()
            .or_else(|| env_u16_list("AMBERDAV_BOUNCE_KEYS"))
        {
            s.bounce_keys = v;
        }
        if let Some(v) = self.name.clone().or_else(|| env_str("AMBERDAV_NAME")) {
            s.name = Some(v);
        }
        Ok(s)
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

/// Parse a boolean setting. Accepts the usual on/off spellings; anything else
/// is ignored (treated as unset) rather than silently meaning `false`.
fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Parse a comma-separated list of `u16` (e.g. evdev key codes). Surrounding
/// whitespace is trimmed and unparseable entries are skipped.
fn parse_u16_list(s: &str) -> Vec<u16> {
    s.split(',').filter_map(|c| c.trim().parse().ok()).collect()
}

/// Apply a list of `[NAME=]PATH` root entries to settings, replacing whatever
/// `root`/`roots` was previously set at that layer.
///
/// One entry → single-root (`settings.root`), `roots` cleared.
/// Two or more → `settings.roots` map, `root` cleared.
///
/// Splitting on the **first** `=` only lets Windows absolute paths through:
/// `C=C:\Users\me` → name `C`, path `C:\Users\me`.
///
/// Two entries resolving to the same mount name (a bare-path basename
/// collision, or a repeated `NAME=`) are a startup error — the map would
/// silently keep only one of them, and mount names are the stable URLs DAV
/// clients bookmark, so the user must pick names explicitly.
/// Expand a leading `~` to the current user's home directory. The shell only
/// performs this expansion at the start of a word or in shell-assignment
/// context; inside a command argument like `NAME=~/path`, the `~` is literal.
/// This makes `--root PDOG=~/Personal` work the same as `--root ~/Personal`.
fn expand_home(path: &str) -> String {
    // Accept both `~/` (Unix) and `~\` (Windows) — the backslash form never
    // appears in practice on non-Windows but is harmless to recognize.
    let tail: Option<&str> = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
    };
    if let Some(tail) = tail {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from);
        if let Some(mut h) = home {
            if !tail.is_empty() {
                h.push(tail);
            }
            return h.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

fn apply_root_entries(s: &mut Settings, entries: Vec<String>) -> Result<(), String> {
    if entries.len() == 1 {
        // Single entry: bare PATH or NAME=PATH. Both map to single-root.
        let entry = &entries[0];
        // Split on first `=` to separate an optional name from the path.
        let path = match entry.split_once('=') {
            Some((name, p)) => {
                tracing::info!("single root: mount name \"{name}\" ignored; content served at /");
                p
            }
            None => entry.as_str(),
        };
        s.root = Some(expand_home(path));
        s.roots = None;
    } else {
        // Multiple entries: build a named-mount map.
        let mut map = BTreeMap::new();
        for entry in &entries {
            let (name, path) = entry
                .split_once('=')
                .map(|(n, p)| (n.to_string(), expand_home(p)))
                .unwrap_or_else(|| {
                    // Bare path: use the last component as the mount name.
                    let expanded = expand_home(entry.as_str());
                    let name = std::path::Path::new(expanded.as_str())
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| expanded.clone());
                    (name, expanded)
                });
            if let Some(prev) = map.insert(name.clone(), path.clone()) {
                return Err(format!(
                    "config error: mounts \"{prev}\" and \"{path}\" both get the name \
                     \"{name}\"; give one an explicit name with --root NAME=PATH"
                ));
            }
        }
        s.roots = Some(map);
        s.root = None;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_u16_list_trims_and_skips_garbage() {
        assert_eq!(parse_u16_list("354,315"), vec![354, 315]);
        assert_eq!(parse_u16_list(" 354 , 315 "), vec![354, 315]);
        assert_eq!(parse_u16_list("354,nope,307"), vec![354, 307]);
        assert!(parse_u16_list("").is_empty());
        assert!(parse_u16_list("abc").is_empty());
    }

    /// Parse a CLI invocation (argv[0] included automatically).
    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("amber-dav").chain(args.iter().copied()))
    }

    /// A fake process environment backed by a slice of pairs.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    // The documented ladder, per field: CLI flag > env var > config file >
    // compiled default.
    #[test]
    fn precedence_is_cli_env_file_default() {
        let file = Settings {
            root: Some("file-root".to_string()),
            port: Some(1111),
            bind: Some("10.0.0.1".to_string()),
            ..Settings::default()
        };
        let env = [("AMBERDAV_ROOT", "env-root"), ("AMBERDAV_PORT", "2222")];

        // CLI beats env (root); env beats file (port); file survives when
        // neither CLI nor env supplies a value (bind).
        let s = cli(&["--root", "cli-root"])
            .resolve_with(file.clone(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.root.as_deref(), Some("cli-root"));
        assert_eq!(s.port, Some(2222));
        assert_eq!(s.bind.as_deref(), Some("10.0.0.1"));

        // No CLI: env wins over the file.
        let s = cli(&[])
            .resolve_with(file.clone(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.root.as_deref(), Some("env-root"));

        // Nothing set anywhere: the compiled default (None here) holds.
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&[]))
            .expect("resolve");
        assert_eq!(s.root, None);
        assert_eq!(s.port, None);
        assert_eq!(s.permission, Permission::ReadWrite);
    }

    // The positional `root`/`port` aliases participate in the CLI layer, but
    // the named flags outrank them.
    #[test]
    fn positional_args_count_as_cli_but_flags_win() {
        let s = cli(&["pos-root", "9000"])
            .resolve_with(Settings::default(), env_of(&[]))
            .expect("resolve");
        assert_eq!(s.root.as_deref(), Some("pos-root"));
        assert_eq!(s.port, Some(9000));

        let s = cli(&["pos-root", "9000", "--root", "flag-root", "--port", "9001"])
            .resolve_with(Settings::default(), env_of(&[]))
            .expect("resolve");
        assert_eq!(s.root.as_deref(), Some("flag-root"));
        assert_eq!(s.port, Some(9001));

        // Positionals still beat the environment.
        let s = cli(&["pos-root"])
            .resolve_with(
                Settings::default(),
                env_of(&[("AMBERDAV_ROOT", "env-root")]),
            )
            .expect("resolve");
        assert_eq!(s.root.as_deref(), Some("pos-root"));
    }

    // AMBERDAV_PORT outranks the generic PORT, which is honoured as the
    // container-friendly fallback.
    #[test]
    fn amberdav_port_beats_generic_port() {
        let both = [("AMBERDAV_PORT", "2222"), ("PORT", "3333")];
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&both))
            .expect("resolve");
        assert_eq!(s.port, Some(2222));

        let generic = [("PORT", "3333")];
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&generic))
            .expect("resolve");
        assert_eq!(s.port, Some(3333));
    }

    // An env var that is set but empty (or unparseable) must not mask the
    // config file's value.
    #[test]
    fn empty_or_garbage_env_values_are_ignored() {
        let file = Settings {
            root: Some("file-root".to_string()),
            port: Some(1111),
            ..Settings::default()
        };
        let env = [
            ("AMBERDAV_ROOT", ""),
            ("AMBERDAV_PORT", "not-a-port"),
            ("AMBERDAV_DISPLAY_PASSWORD", "maybe"),
            ("AMBERDAV_BLANK_KEYS", "abc,def"),
        ];
        let s = cli(&[]).resolve_with(file, env_of(&env)).expect("resolve");
        assert_eq!(s.root.as_deref(), Some("file-root"));
        assert_eq!(s.port, Some(1111));
        // Unparseable bool/list values are treated as unset, not as false/empty.
        assert!(s.display_password);
        assert_eq!(s.blank_keys, Settings::default().blank_keys);
    }

    // Boolean env spellings, and the --flag/--no-flag pair outranking them.
    #[test]
    fn bool_env_spellings_and_cli_override() {
        for (val, want) in [("1", true), ("on", true), ("FALSE", false), ("no", false)] {
            let env = [("AMBERDAV_DISPLAY_PASSWORD", val)];
            let s = cli(&[])
                .resolve_with(Settings::default(), env_of(&env))
                .expect("resolve");
            assert_eq!(s.display_password, want, "value {val:?}");
        }

        // The CLI flag wins over a contradicting env var.
        let env = [("AMBERDAV_DISPLAY_PASSWORD", "true")];
        let s = cli(&["--no-display-password"])
            .resolve_with(Settings::default(), env_of(&env))
            .expect("resolve");
        assert!(!s.display_password);
    }

    // The permission ladder resolves through every layer, including the
    // value-enum spelling used by both the env var and the flag.
    #[test]
    fn permission_resolves_through_the_layers() {
        let env = [("AMBERDAV_PERMISSION", "read_only")];
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.permission, Permission::ReadOnly);

        let s = cli(&["--permission", "read_write_delete"])
            .resolve_with(Settings::default(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.permission, Permission::ReadWriteDelete);

        // Garbage env spelling: ignored, file/default holds.
        let env = [("AMBERDAV_PERMISSION", "rwx")];
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.permission, Permission::ReadWrite);
    }

    // Device name follows the same CLI > env > file > default precedence.
    #[test]
    fn name_resolves_through_the_layers() {
        let file = Settings {
            name: Some("file-name".to_string()),
            ..Settings::default()
        };
        let env = [("AMBERDAV_NAME", "env-name")];

        // CLI beats env.
        let s = cli(&["--name", "cli-name"])
            .resolve_with(file.clone(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.name.as_deref(), Some("cli-name"));

        // Env beats file.
        let s = cli(&[])
            .resolve_with(file.clone(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.name.as_deref(), Some("env-name"));

        // File survives when neither CLI nor env supplies a value.
        let s = cli(&[]).resolve_with(file, env_of(&[])).expect("resolve");
        assert_eq!(s.name.as_deref(), Some("file-name"));

        // Nothing set → None (falls back to "web access" in the UI).
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&[]))
            .expect("resolve");
        assert_eq!(s.name, None);
    }

    // Key-code lists: the plural var wins, the legacy singular AMBERDAV_EXIT_KEY
    // is still honoured, and a no-usable-codes value cannot blank a config list.
    #[test]
    fn exit_key_lists_and_legacy_singular() {
        let env = [("AMBERDAV_EXIT_KEYS", "1, 2"), ("AMBERDAV_EXIT_KEY", "9")];
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.exit_keys, vec![1, 2]);

        let env = [("AMBERDAV_EXIT_KEY", "9")];
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.exit_keys, vec![9]);

        let env = [("AMBERDAV_EXIT_KEYS", "nope")];
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.exit_keys, Settings::default().exit_keys);
    }

    // Two bare paths with the same basename would silently collapse to one
    // BTreeMap key — the spec requires a startup error naming both roots and
    // showing the NAME= fix instead (issue #76).
    #[test]
    fn multi_root_duplicate_basenames_is_an_error() {
        let err = cli(&["--root", "/a/data", "--root", "/b/data"])
            .resolve_with(Settings::default(), env_of(&[]))
            .expect_err("colliding basenames must be a startup error");
        assert!(err.contains("/a/data"), "must name the first root: {err}");
        assert!(err.contains("/b/data"), "must name the second root: {err}");
        assert!(err.contains("NAME="), "must show the NAME= fix: {err}");

        // Same via the env list.
        let err = cli(&[])
            .resolve_with(
                Settings::default(),
                env_of(&[("AMBERDAV_ROOT", "/a/data;/b/data")]),
            )
            .expect_err("env basename collision must error too");
        assert!(err.contains("data"), "{err}");

        // Explicit names dedupe the same way: repeating a NAME is an error.
        cli(&["--root", "x=/a", "--root", "x=/b"])
            .resolve_with(Settings::default(), env_of(&[]))
            .expect_err("duplicate explicit names must be a startup error");
    }

    // A single --root value is treated as single-root (sets settings.root,
    // clears settings.roots). Multi --root values build a named-mount map.
    #[test]
    fn multi_root_cli_flag() {
        // Single bare path → single root.
        let s = cli(&["--root", "/srv/files"])
            .resolve_with(Settings::default(), env_of(&[]))
            .expect("resolve");
        assert_eq!(s.root.as_deref(), Some("/srv/files"));
        assert!(s.roots.is_none());

        // Named single path → still single root (name stripped, path kept).
        let s = cli(&["--root", "data=/srv/files"])
            .resolve_with(Settings::default(), env_of(&[]))
            .expect("resolve");
        assert_eq!(s.root.as_deref(), Some("/srv/files"));
        assert!(s.roots.is_none());

        // Two --root flags → roots map.
        let s = cli(&["--root", "one=/srv/one", "--root", "two=/srv/two"])
            .resolve_with(Settings::default(), env_of(&[]))
            .expect("resolve");
        assert!(s.root.is_none());
        let roots = s.roots.expect("two --root flags must produce roots map");
        assert_eq!(roots.get("one").map(String::as_str), Some("/srv/one"));
        assert_eq!(roots.get("two").map(String::as_str), Some("/srv/two"));
    }

    // AMBERDAV_ROOT with `;` as the separator for multi-root env configuration.
    #[test]
    fn multi_root_env_semicolon_separated() {
        // Single entry (no semicolon): single root.
        let env = [("AMBERDAV_ROOT", "/srv/files")];
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&env))
            .expect("resolve");
        assert_eq!(s.root.as_deref(), Some("/srv/files"));
        assert!(s.roots.is_none());

        // Two entries: roots map.
        let env = [("AMBERDAV_ROOT", "one=/srv/one;two=/srv/two")];
        let s = cli(&[])
            .resolve_with(Settings::default(), env_of(&env))
            .expect("resolve");
        assert!(s.root.is_none());
        let roots = s.roots.expect("two entries must produce roots map");
        assert_eq!(roots.get("one").map(String::as_str), Some("/srv/one"));
        assert_eq!(roots.get("two").map(String::as_str), Some("/srv/two"));

        // Empty string does not mask the file value.
        let env = [("AMBERDAV_ROOT", "")];
        let file = Settings {
            root: Some("file-root".to_string()),
            ..Settings::default()
        };
        let s = cli(&[]).resolve_with(file, env_of(&env)).expect("resolve");
        assert_eq!(s.root.as_deref(), Some("file-root"));
    }

    // `~` in a NAME=PATH argument is not expanded by the shell (tilde expansion
    // only fires at the start of a word, not after `=` in a command argument).
    // The parser must expand it so `PDOG=~/Personal` reaches the same real path
    // as `~/Personal` alone (issue #76).
    #[test]
    fn tilde_in_named_mount_path_is_expanded() {
        use super::expand_home;

        // expand_home leaves non-tilde paths alone.
        assert_eq!(expand_home("/absolute"), "/absolute");
        assert_eq!(expand_home("relative/path"), "relative/path");

        // If HOME is set, ~ expands to it.
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(expand_home("~"), home, "bare ~ is the home dir");
            assert_eq!(
                expand_home("~/Personal"),
                format!("{home}/Personal"),
                "~/sub expands correctly"
            );
        }

        // CLI: PDOG=~/Personal stores the expanded path, not the literal ~.
        if let Ok(home) = std::env::var("HOME") {
            let s = cli(&["--root", "/srv/a", "--root", "PDOG=~/Personal"])
                .resolve_with(Settings::default(), env_of(&[]))
                .expect("resolve");
            let roots = s.roots.expect("two --root flags must produce roots map");
            assert_eq!(
                roots.get("PDOG").map(String::as_str),
                Some(format!("{home}/Personal").as_str()),
                "PDOG path must be tilde-expanded"
            );
        }
    }
}
