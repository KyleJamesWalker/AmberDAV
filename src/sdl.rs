//! SDL2 display sink: shows the connection-info canvas in a fullscreen SDL
//! window. This is the on-screen path for the Steam Deck in Game Mode (where a
//! native-Wayland surface is never foregrounded by Steam) and for Anbernic
//! handhelds. The SDL video driver is auto-selected so one binary works on both
//! — `x11` (Xwayland, which Steam foregrounds) on the Deck, the `mali` vendor
//! driver on Anbernic. Compiled only with the `sdl` feature; links the system
//! libSDL2 dynamically so each device's own driver is available at runtime.

/// Video drivers to try, in order, when `SDL_VIDEODRIVER` isn't forced. `x11`
/// is first so the Steam Deck gets an Xwayland window (the surface Steam
/// foregrounds); `mali` covers Anbernic. `wayland` is tried before raw `kmsdrm`
/// so a Wayland compositor (without Xwayland) is used rather than fighting it
/// for the DRM device. `dummy` is never chosen (no output).
const DRIVER_PREFERENCE: &[&str] = &["x11", "mali", "wayland", "kmsdrm", "fbcon"];

/// The ordered list of SDL video drivers to attempt. A non-empty forced value
/// (e.g. `$SDL_VIDEODRIVER`) is the sole candidate; otherwise the preference
/// list. Pure so it can be unit-tested without a display.
// used by run() in the next task
#[cfg_attr(not(test), allow(dead_code))]
pub fn driver_candidates(forced: Option<&str>) -> Vec<String> {
    match forced.map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => vec![d.to_string()],
        None => DRIVER_PREFERENCE.iter().map(|s| s.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_driver_is_the_only_candidate() {
        assert_eq!(driver_candidates(Some("mali")), vec!["mali".to_string()]);
        assert_eq!(driver_candidates(Some("  x11 ")), vec!["x11".to_string()]);
    }

    #[test]
    fn unset_yields_the_preference_list_x11_first() {
        let c = driver_candidates(None);
        assert_eq!(c.first().map(String::as_str), Some("x11"));
        assert!(c.iter().any(|d| d == "mali"));
        assert!(!c.iter().any(|d| d == "dummy"));
        assert_eq!(
            c,
            DRIVER_PREFERENCE
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn empty_forced_is_treated_as_unset() {
        assert_eq!(driver_candidates(Some("")), driver_candidates(None));
        assert_eq!(driver_candidates(Some("   ")), driver_candidates(None));
    }
}
