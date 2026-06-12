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
/// with `--features fb` or `--features sdl`) it also paints connection info to
/// the screen and shows live gamepad input.
#[derive(Parser, Debug)]
#[command(name = "amber-dav", version = crate::version::VERSION)]
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
}

impl Cli {
    /// Merge CLI args and `AMBERDAV_*` env vars on top of the loaded config
    /// file, honouring CLI > env > file > compiled default. A field left unset
    /// at every layer keeps the value already on `s` (the loaded file, or the
    /// compiled default behind it).
    pub fn resolve(&self, s: Settings) -> Settings {
        self.resolve_with(s, |key| std::env::var(key).ok())
    }

    /// [`resolve`](Cli::resolve) with the environment lookup injected, so the
    /// precedence rules are testable without mutating the process environment
    /// (`std::env::set_var` is unsafe under parallel tests).
    fn resolve_with(&self, mut s: Settings, env: impl Fn(&str) -> Option<String>) -> Settings {
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
        let s = cli(&["--root", "cli-root"]).resolve_with(file.clone(), env_of(&env));
        assert_eq!(s.root.as_deref(), Some("cli-root"));
        assert_eq!(s.port, Some(2222));
        assert_eq!(s.bind.as_deref(), Some("10.0.0.1"));

        // No CLI: env wins over the file.
        let s = cli(&[]).resolve_with(file.clone(), env_of(&env));
        assert_eq!(s.root.as_deref(), Some("env-root"));

        // Nothing set anywhere: the compiled default (None here) holds.
        let s = cli(&[]).resolve_with(Settings::default(), env_of(&[]));
        assert_eq!(s.root, None);
        assert_eq!(s.port, None);
        assert_eq!(s.permission, Permission::ReadWrite);
    }

    // The positional `root`/`port` aliases participate in the CLI layer, but
    // the named flags outrank them.
    #[test]
    fn positional_args_count_as_cli_but_flags_win() {
        let s = cli(&["pos-root", "9000"]).resolve_with(Settings::default(), env_of(&[]));
        assert_eq!(s.root.as_deref(), Some("pos-root"));
        assert_eq!(s.port, Some(9000));

        let s = cli(&["pos-root", "9000", "--root", "flag-root", "--port", "9001"])
            .resolve_with(Settings::default(), env_of(&[]));
        assert_eq!(s.root.as_deref(), Some("flag-root"));
        assert_eq!(s.port, Some(9001));

        // Positionals still beat the environment.
        let s = cli(&["pos-root"]).resolve_with(
            Settings::default(),
            env_of(&[("AMBERDAV_ROOT", "env-root")]),
        );
        assert_eq!(s.root.as_deref(), Some("pos-root"));
    }

    // AMBERDAV_PORT outranks the generic PORT, which is honoured as the
    // container-friendly fallback.
    #[test]
    fn amberdav_port_beats_generic_port() {
        let both = [("AMBERDAV_PORT", "2222"), ("PORT", "3333")];
        let s = cli(&[]).resolve_with(Settings::default(), env_of(&both));
        assert_eq!(s.port, Some(2222));

        let generic = [("PORT", "3333")];
        let s = cli(&[]).resolve_with(Settings::default(), env_of(&generic));
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
        let s = cli(&[]).resolve_with(file, env_of(&env));
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
            let s = cli(&[]).resolve_with(Settings::default(), env_of(&env));
            assert_eq!(s.display_password, want, "value {val:?}");
        }

        // The CLI flag wins over a contradicting env var.
        let env = [("AMBERDAV_DISPLAY_PASSWORD", "true")];
        let s = cli(&["--no-display-password"]).resolve_with(Settings::default(), env_of(&env));
        assert!(!s.display_password);
    }

    // The permission ladder resolves through every layer, including the
    // value-enum spelling used by both the env var and the flag.
    #[test]
    fn permission_resolves_through_the_layers() {
        let env = [("AMBERDAV_PERMISSION", "read_only")];
        let s = cli(&[]).resolve_with(Settings::default(), env_of(&env));
        assert_eq!(s.permission, Permission::ReadOnly);

        let s = cli(&["--permission", "read_write_delete"])
            .resolve_with(Settings::default(), env_of(&env));
        assert_eq!(s.permission, Permission::ReadWriteDelete);

        // Garbage env spelling: ignored, file/default holds.
        let env = [("AMBERDAV_PERMISSION", "rwx")];
        let s = cli(&[]).resolve_with(Settings::default(), env_of(&env));
        assert_eq!(s.permission, Permission::ReadWrite);
    }

    // Key-code lists: the plural var wins, the legacy singular AMBERDAV_EXIT_KEY
    // is still honoured, and a no-usable-codes value cannot blank a config list.
    #[test]
    fn exit_key_lists_and_legacy_singular() {
        let env = [("AMBERDAV_EXIT_KEYS", "1, 2"), ("AMBERDAV_EXIT_KEY", "9")];
        let s = cli(&[]).resolve_with(Settings::default(), env_of(&env));
        assert_eq!(s.exit_keys, vec![1, 2]);

        let env = [("AMBERDAV_EXIT_KEY", "9")];
        let s = cli(&[]).resolve_with(Settings::default(), env_of(&env));
        assert_eq!(s.exit_keys, vec![9]);

        let env = [("AMBERDAV_EXIT_KEYS", "nope")];
        let s = cli(&[]).resolve_with(Settings::default(), env_of(&env));
        assert_eq!(s.exit_keys, Settings::default().exit_keys);
    }
}
