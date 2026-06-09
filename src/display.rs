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

/// Resolve the live sink from the process environment + filesystem.
#[cfg(feature = "handheld")]
pub fn detect() -> DisplayKind {
    let wayland = std::env::var("WAYLAND_DISPLAY").ok();
    let override_ = std::env::var("AMBERDAV_DISPLAY").ok();
    let fb0 = std::path::Path::new("/dev/fb0").exists();
    select(wayland.as_deref(), fb0, override_.as_deref())
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
        assert_eq!(select(Some("gamescope-0"), true, Some("fb")), DisplayKind::Framebuffer);
        assert_eq!(select(Some("gamescope-0"), true, Some("headless")), DisplayKind::Headless);
        assert_eq!(select(Some("gamescope-0"), true, Some("nonsense")), DisplayKind::Wayland);
    }

    #[test]
    fn override_is_case_insensitive_and_trimmed() {
        assert_eq!(select(None, false, Some("  WAYLAND ")), DisplayKind::Wayland);
        assert_eq!(select(None, true, Some("FB")), DisplayKind::Framebuffer);
    }
}
