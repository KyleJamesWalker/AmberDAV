//! Which sink paints the device screen, decided once at startup. Game Mode runs
//! under Gamescope (a Wayland compositor that owns DRM, so `/dev/fb0` is
//! invisible) — there we must be a Wayland client. The Anbernic, a raw TTY, and
//! Desktop Mode use the framebuffer. Everything else is headless (banner only).
//!
//! `AMBERDAV_DISPLAY` (`wayland` | `fb` | `headless`) forces a sink for testing.

// `DisplayKind` and `select` are wired up in the screen-dispatch task; until
// then they are unused on the headless build.
#[cfg_attr(not(feature = "handheld"), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DisplayKind {
    Wayland,
    Framebuffer,
    Headless,
}

/// Pure selection logic. `wayland_display` is `$WAYLAND_DISPLAY`, `fb0_exists`
/// is whether `/dev/fb0` is present, `override_` is `$AMBERDAV_DISPLAY`. The
/// override is matched case-insensitively with surrounding whitespace trimmed.
#[cfg_attr(not(feature = "handheld"), allow(dead_code))]
pub fn select(
    wayland_display: Option<&str>,
    fb0_exists: bool,
    override_: Option<&str>,
) -> DisplayKind {
    match override_.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("wayland") => return DisplayKind::Wayland,
        Some("fb") => return DisplayKind::Framebuffer,
        Some("headless") => return DisplayKind::Headless,
        _ => {}
    }
    let has_wayland = wayland_display.map(|s| !s.is_empty()).unwrap_or(false);
    if has_wayland {
        DisplayKind::Wayland
    } else if fb0_exists {
        DisplayKind::Framebuffer
    } else {
        DisplayKind::Headless
    }
}

/// Pick the Wayland display socket to use: an explicit `$WAYLAND_DISPLAY` wins,
/// otherwise discover the best compositor socket among `$XDG_RUNTIME_DIR`'s
/// entries. This is what lets Game Mode work even when Steam launches us with
/// no `$WAYLAND_DISPLAY` set — a plain GUI toolkit auto-discovers the socket the
/// same way, which is why other apps "just work" there.
///
/// Prefers a standard `wayland-N` (desktops) over a `gamescope-N` (Steam Game
/// Mode), lowest index first. Auxiliary sockets (`.lock`, gamescope's `-ei`
/// emulated-input, etc.) are skipped — only `<prefix>-<number>` exactly.
#[cfg_attr(not(feature = "handheld"), allow(dead_code))]
pub fn pick_wayland_socket(
    wayland_display: Option<&str>,
    runtime_entries: &[String],
) -> Option<String> {
    if let Some(wd) = wayland_display {
        if !wd.is_empty() {
            return Some(wd.to_string());
        }
    }
    // Exact `<prefix><number>` only (e.g. "wayland-0"); rejects "gamescope-0-ei"
    // and "gamescope-0.lock", whose trailing text fails to parse as a number.
    fn index(name: &str, prefix: &str) -> Option<u32> {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.parse::<u32>().ok())
    }
    let lowest = |prefix: &str| -> Option<String> {
        runtime_entries
            .iter()
            .filter_map(|e| index(e, prefix).map(|n| (n, e.clone())))
            .min_by_key(|(n, _)| *n)
            .map(|(_, name)| name)
    };
    lowest("wayland-").or_else(|| lowest("gamescope-"))
}

/// Resolve the live sink from the process environment + filesystem.
#[cfg(feature = "handheld")]
pub fn detect() -> DisplayKind {
    let socket = wayland_socket();
    let override_ = std::env::var("AMBERDAV_DISPLAY").ok();
    let fb0 = std::path::Path::new("/dev/fb0").exists();
    let kind = select(socket.as_deref(), fb0, override_.as_deref());
    // Log the decision: the device screen is headless from a terminal's view, so
    // this is how the sink choice is diagnosed (especially under Steam Game Mode).
    eprintln!(
        "display: sink={kind:?} (wayland_socket={socket:?}, fb0={fb0}, override={override_:?})"
    );
    kind
}

/// The resolved Wayland socket (env or discovered), for the sink to connect to.
#[cfg(feature = "handheld")]
pub fn wayland_socket() -> Option<String> {
    let wd = std::env::var("WAYLAND_DISPLAY").ok();
    let entries: Vec<String> = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    pick_wayland_socket(wd.as_deref(), &entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wayland_chosen_when_compositor_present() {
        let k = select(Some("gamescope-0"), true, None);
        assert_eq!(k, DisplayKind::Wayland);
    }

    #[test]
    fn framebuffer_chosen_when_no_compositor_but_fb0_exists() {
        let k = select(None, true, None);
        assert_eq!(k, DisplayKind::Framebuffer);
    }

    #[test]
    fn headless_when_no_compositor_and_no_fb0() {
        let k = select(None, false, None);
        assert_eq!(k, DisplayKind::Headless);
    }

    #[test]
    fn empty_wayland_display_is_ignored() {
        assert_eq!(select(Some(""), true, None), DisplayKind::Framebuffer);
    }

    #[test]
    fn override_forces_a_specific_sink() {
        assert_eq!(select(None, false, Some("wayland")), DisplayKind::Wayland);
        assert_eq!(
            select(Some("gamescope-0"), true, Some("fb")),
            DisplayKind::Framebuffer
        );
        assert_eq!(
            select(Some("gamescope-0"), true, Some("headless")),
            DisplayKind::Headless
        );
        assert_eq!(
            select(Some("gamescope-0"), true, Some("nonsense")),
            DisplayKind::Wayland
        );
    }

    #[test]
    fn override_is_case_insensitive_and_trimmed() {
        assert_eq!(
            select(None, false, Some("  WAYLAND ")),
            DisplayKind::Wayland
        );
        assert_eq!(select(None, true, Some("FB")), DisplayKind::Framebuffer);
    }

    fn entries(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn explicit_wayland_display_wins_over_discovery() {
        let e = entries(&["gamescope-0", "wayland-1"]);
        assert_eq!(
            pick_wayland_socket(Some("wayland-0"), &e).as_deref(),
            Some("wayland-0")
        );
    }

    #[test]
    fn discovers_gamescope_socket_in_game_mode() {
        // Mirrors a real Steam Deck Game Mode runtime dir: no WAYLAND_DISPLAY,
        // only gamescope sockets plus aux/lock files that must be ignored.
        let e = entries(&[
            "gamescope-0",
            "gamescope-0-ei",
            "gamescope-0.lock",
            "gamescope-1",
            "gamescope-stats",
            "kwin-xwayland-eis-socket.24856.lock",
        ]);
        assert_eq!(
            pick_wayland_socket(None, &e).as_deref(),
            Some("gamescope-0")
        );
    }

    #[test]
    fn prefers_plain_wayland_over_gamescope() {
        let e = entries(&["gamescope-0", "wayland-2"]);
        assert_eq!(pick_wayland_socket(None, &e).as_deref(), Some("wayland-2"));
    }

    #[test]
    fn no_socket_when_nothing_present() {
        assert_eq!(pick_wayland_socket(None, &entries(&["pulse", "bus"])), None);
        assert_eq!(pick_wayland_socket(Some(""), &[]), None);
    }
}
