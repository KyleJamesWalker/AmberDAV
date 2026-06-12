//! Shared frame production for the device-screen sinks (issue #39).
//!
//! All three sinks (framebuffer, SDL, Wayland) show the same three screens —
//! connection info, blank, bounce screensaver — but each used to carry its own
//! copy of the `Mode -> canvas` selection, and each rebuilt the *static*
//! screens (a `QrCode::new`, a netlink IP lookup, a full-canvas render) on
//! every repaint. [`FrameSource`] is the one shared producer: a sink asks it
//! for the current frame and pixels are only re-rendered when they would
//! actually differ — a static screen re-renders when the mode, the canvas
//! size, or the device IP changes; the bounce screensaver re-renders every
//! step, because it animates.
//!
//! The decision/cache pieces ([`effective_mode`], [`FrameKey`],
//! [`StaticCache`]) are pure and host-testable; only [`FrameSource`] (which
//! owns the device-only bounce engine and queries the live IP) is gated to
//! the Linux device builds.
//!
//! See also — content: `canvas.rs` → choice: `display.rs` → sinks:
//! `screen.rs`/`sdl.rs`/`wayland.rs` → state: `screen::Mode`.

// On non-device builds only the unit tests exercise this module (same
// pattern as canvas.rs).
#![cfg_attr(not(all(target_os = "linux", device)), allow(dead_code))]

use std::net::IpAddr;

use crate::screen::Mode;

/// The mode a sink will actually draw. The Wayland sink (Steam Deck Game
/// Mode) has no bounce support — see the screensaver note in the README — so
/// it passes `bounce_supported = false` and Bounce falls back to the info
/// screen explicitly, instead of by silent omission in a duplicated match.
pub fn effective_mode(mode: Mode, bounce_supported: bool) -> Mode {
    match mode {
        // Bounce not supported by this sink (Wayland): show Info instead.
        Mode::Bounce if !bounce_supported => Mode::Info,
        m => m,
    }
}

/// Everything the *pixels* of a static screen depend on. A cached canvas is
/// valid exactly as long as this key matches; any change — a mode toggle, a
/// surface resize, Wi-Fi (re)connecting and changing the IP — forces exactly
/// one re-render.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrameKey {
    pub mode: Mode,
    pub w: usize,
    pub h: usize,
    /// The IP shown on the info screen. [`Mode::Black`] does not depend on
    /// one; it uses [`IpAddr::is_unspecified`]'s all-zeros address.
    pub ip: IpAddr,
}

/// The last rendered static canvas plus the key it was rendered for.
/// Animated frames are stored keyless, so they never count as a future hit.
#[derive(Default)]
pub struct StaticCache {
    key: Option<FrameKey>,
    px: Vec<[u8; 3]>,
}

impl StaticCache {
    /// Whether the cached pixels are still valid for `key`.
    pub fn matches(&self, key: &FrameKey) -> bool {
        self.key.as_ref() == Some(key)
    }

    /// The cached pixels (empty before the first `put`).
    pub fn px(&self) -> &[[u8; 3]] {
        &self.px
    }

    /// Store a fresh static render.
    pub fn put(&mut self, key: FrameKey, px: Vec<[u8; 3]>) -> &[[u8; 3]] {
        self.key = Some(key);
        self.px = px;
        &self.px
    }

    /// Hand the internal buffer to an animated renderer: the static key is
    /// cleared (an animated frame is never a future hit) and the buffer is
    /// reused across frames instead of reallocated (issue #40).
    pub fn render_animated(&mut self, draw: impl FnOnce(&mut Vec<[u8; 3]>)) -> &[[u8; 3]] {
        self.key = None;
        draw(&mut self.px);
        &self.px
    }
}

/// Per-sink frame producer: owns the static inputs (port / password /
/// startup error), the bounce engine when the sink supports one, and the
/// render cache.
#[cfg(all(target_os = "linux", device))]
pub struct FrameSource {
    port: u16,
    password: Option<String>,
    startup_error: Option<String>,
    /// `None` = this sink cannot animate bounce (the Wayland sink).
    bounce: Option<crate::bounce::Bounce>,
    cache: StaticCache,
}

#[cfg(all(target_os = "linux", device))]
impl FrameSource {
    pub fn new(
        port: u16,
        password: Option<String>,
        startup_error: Option<String>,
        bounce: Option<crate::bounce::Bounce>,
    ) -> FrameSource {
        FrameSource {
            port,
            password,
            startup_error,
            bounce,
            cache: StaticCache::default(),
        }
    }

    /// Produce the frame for `mode` at `w`x`h`. Returns `(updated, pixels)`:
    /// `updated` is `true` when the pixels differ from the previous call, so
    /// a sink can skip its upload/present entirely on a cache hit. Info mode
    /// re-queries the device IP (cheap next to a render, but the reason
    /// callers throttle how often they ask); Bounce steps the animation and
    /// is always an update.
    pub fn frame(&mut self, mode: Mode, w: usize, h: usize) -> (bool, &[[u8; 3]]) {
        match effective_mode(mode, self.bounce.is_some()) {
            Mode::Bounce => {
                let b = self.bounce.as_mut().expect("gated by effective_mode");
                b.step(w, h);
                // Draw into the cache's buffer in place — no per-frame canvas
                // allocation (issue #40).
                (true, self.cache.render_animated(|buf| b.render(w, h, buf)))
            }
            Mode::Info => {
                // Re-query the IP each time so the screen recovers once Wi-Fi
                // connects after launch; the render is skipped unless it (or
                // anything else in the key) changed.
                let ip = crate::state::current_ip();
                let key = FrameKey {
                    mode: Mode::Info,
                    w,
                    h,
                    ip,
                };
                if self.cache.matches(&key) {
                    (false, self.cache.px())
                } else {
                    let px = crate::canvas::info_canvas(
                        w,
                        h,
                        ip,
                        self.port,
                        self.password.as_deref(),
                        self.startup_error.as_deref(),
                    )
                    .px;
                    (true, self.cache.put(key, px))
                }
            }
            Mode::Black => {
                let key = FrameKey {
                    mode: Mode::Black,
                    w,
                    h,
                    ip: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                };
                if self.cache.matches(&key) {
                    (false, self.cache.px())
                } else {
                    let px = crate::canvas::black_canvas(w, h).px;
                    (true, self.cache.put(key, px))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn bounce_falls_back_to_info_where_unsupported() {
        assert_eq!(effective_mode(Mode::Bounce, false), Mode::Info);
        assert_eq!(effective_mode(Mode::Bounce, true), Mode::Bounce);
        assert_eq!(effective_mode(Mode::Info, false), Mode::Info);
        assert_eq!(effective_mode(Mode::Black, false), Mode::Black);
    }

    fn key(ip: [u8; 4], w: usize) -> FrameKey {
        FrameKey {
            mode: Mode::Info,
            w,
            h: 100,
            ip: IpAddr::V4(Ipv4Addr::from(ip)),
        }
    }

    // The cache must hit only while every pixel-relevant input is unchanged:
    // a different IP (Wi-Fi reconnect), different dims (resize), or a
    // different mode each invalidate it.
    #[test]
    fn static_cache_hits_only_on_the_exact_key() {
        let mut c = StaticCache::default();
        let k = key([192, 168, 1, 2], 200);
        assert!(!c.matches(&k), "empty cache must miss");
        c.put(k, vec![[1, 2, 3]]);
        assert!(c.matches(&k));
        assert_eq!(c.px(), &[[1, 2, 3]]);
        assert!(!c.matches(&key([192, 168, 1, 3], 200)), "ip change");
        assert!(!c.matches(&key([192, 168, 1, 2], 300)), "resize");
        assert!(
            !c.matches(&FrameKey {
                mode: Mode::Black,
                ..k
            }),
            "mode change"
        );
    }

    #[test]
    fn animated_frames_reuse_the_buffer_and_never_hit() {
        let mut c = StaticCache::default();
        let k = key([10, 0, 0, 1], 64);
        c.put(k, vec![[9, 9, 9]]);
        let before = c.px().as_ptr();
        let px = c.render_animated(|buf| buf.fill([1, 1, 1]));
        assert_eq!(px, &[[1, 1, 1]]);
        assert_eq!(
            c.px().as_ptr(),
            before,
            "the in-place draw must reuse the buffer"
        );
        assert!(!c.matches(&k), "an animated frame clears the static key");
    }

    // Device builds: the full FrameSource over the real canvases. Static
    // frames render once and then hit the cache; bounce always updates.
    #[cfg(all(target_os = "linux", device))]
    mod device {
        use super::*;

        #[test]
        fn static_frames_render_once_then_hit_the_cache() {
            let mut s = FrameSource::new(8080, Some("pw".into()), None, None);
            let (u1, px) = s.frame(Mode::Black, 32, 16);
            assert!(u1);
            assert_eq!(px.len(), 32 * 16);
            let (u2, _) = s.frame(Mode::Black, 32, 16);
            assert!(!u2, "unchanged inputs must be a cache hit");
            let (u3, px) = s.frame(Mode::Black, 64, 16);
            assert!(u3, "a resize must re-render");
            assert_eq!(px.len(), 64 * 16);
        }

        #[test]
        fn bounce_always_updates_when_supported() {
            let bounce = crate::bounce::Bounce::new(Vec::new());
            let mut s = FrameSource::new(8080, None, None, Some(bounce));
            let (u1, _) = s.frame(Mode::Bounce, 32, 16);
            let (u2, _) = s.frame(Mode::Bounce, 32, 16);
            assert!(u1 && u2, "an animation never reports a cache hit");
        }

        #[test]
        fn bounce_without_support_serves_the_info_screen() {
            let mut s = FrameSource::new(8080, None, None, None);
            let (u1, _) = s.frame(Mode::Bounce, 32, 16);
            assert!(u1);
            // The fallback is cached as Info: asking again (same IP) hits.
            let (u2, _) = s.frame(Mode::Bounce, 32, 16);
            assert!(!u2);
        }
    }
}
