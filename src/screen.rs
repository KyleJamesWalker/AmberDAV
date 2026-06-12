//! On-device screen output. Draws the connection info (IP, password) and a QR
//! code straight to the Linux framebuffer (`/dev/fb0`) so the handheld is
//! usable without already knowing its IP. The real sinks compile with the `fb`
//! or `sdl` feature on Linux only; everywhere else (desktop/server builds, and
//! non-Linux hosts building the device features) `show` is a no-op stub.
//!
//! The displayed mode is shared with the input thread so the gamepad can drive
//! it: the A button blanks the screen, and the X button starts a "DVD bounce"
//! screensaver that drifts random images around to prevent burn-in.
//!
//! Rotation: portrait-mounted panels (the framebuffer reports taller than
//! wide, e.g. the RG34XXSP under the stock OS) automatically get the
//! landscape-authored canvas turned 90°; the env var `AMBERDAV_FB_ROTATE`
//! (0/90/180/270) overrides the choice if the guess is wrong for a panel.
//!
//! See also — content: `canvas.rs` → choice: `display.rs` → sinks:
//! `screen.rs`/`sdl.rs`/`wayland.rs` → state: `screen::Mode`.

use std::sync::{Arc, Mutex};

/// Shared, human-readable framebuffer status (surfaced on the web status page
/// so the screen can be diagnosed remotely without looking at the panel).
pub type Status = Arc<Mutex<String>>;

/// What the screen is currently showing. Toggled live from the input thread.
// The Black/Bounce variants are only constructed on handheld builds (and in
// tests); the headless stub never leaves Info.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Connection info + QR code (the default).
    Info,
    /// All pixels black (A button) — quick blank for sleeping the panel.
    Black,
    /// Bouncing-image screensaver (X button).
    Bounce,
}

/// Live screen mode, shared between the render thread and the input thread.
pub type ModeHandle = Arc<Mutex<Mode>>;

/// Create the shared mode handle (starts on [`Mode::Info`]).
pub fn mode_handle() -> ModeHandle {
    Arc::new(Mutex::new(Mode::Info))
}

/// Toggle between `target` and [`Mode::Info`]: pressing the button again (or
/// the other mode's button) returns to the info screen.
#[allow(dead_code)] // driven by the handheld input thread; also exercised in tests
pub fn toggle(handle: &ModeHandle, target: Mode) {
    if let Ok(mut m) = handle.lock() {
        *m = if *m == target { Mode::Info } else { target };
    }
}

/// Update the shared screen status. One copy for every sink — `sdl.rs` and
/// `wayland.rs` carried their own identical helpers (issue #39).
pub fn set_status(status: &Status, msg: String) {
    if let Ok(mut s) = status.lock() {
        *s = msg;
    }
}

/// SDL build: always use the SDL sink (it auto-selects the video driver).
/// `shutdown` flows into the sink: closing the window cancels it (so the
/// server drains instead of dying mid-write), and a cancellation from
/// anywhere else stops the sink's render loop.
#[cfg(all(target_os = "linux", feature = "sdl"))]
pub fn show(
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    bounce_paths: Vec<std::path::PathBuf>,
    startup_error: Option<String>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    set_status(&status, "sdl: starting…".to_string());
    std::thread::spawn(move || {
        if let Err(e) = crate::sdl::run(
            port,
            password,
            status.clone(),
            mode,
            bounce_paths,
            startup_error,
            shutdown,
        ) {
            set_status(&status, format!("sdl failed: {e}"));
            eprintln!("screen: sdl sink failed ({e}); connection info is in the log only");
        }
    });
}

/// Pick the active display sink (Wayland in Game Mode, framebuffer on the
/// Anbernic/TTY/Desktop Mode, else headless) and start painting connection
/// info. Returns immediately; the chosen sink runs in a background thread.
// The fb/Wayland sinks have no exit event of their own (quitting comes from
// the gamepad exit key, which `input::spawn` routes through the token), so
// this variant does not consume `shutdown`.
#[cfg(all(target_os = "linux", feature = "fb", not(feature = "sdl")))]
pub fn show(
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    bounce_paths: Vec<std::path::PathBuf>,
    startup_error: Option<String>,
    _shutdown: tokio_util::sync::CancellationToken,
) {
    use crate::display::{detect, DisplayKind};
    match detect() {
        DisplayKind::Wayland => {
            let socket = crate::display::wayland_socket();
            set_status(&status, "wayland: starting…".to_string());
            std::thread::spawn(move || {
                if let Err(e) =
                    crate::wayland::run(port, password, status.clone(), mode, socket, startup_error)
                {
                    set_status(&status, format!("wayland failed: {e}"));
                    eprintln!(
                        "screen: wayland sink failed ({e}); connection info is in the log only"
                    );
                }
            });
        }
        DisplayKind::Framebuffer => {
            show_framebuffer(port, password, status, mode, bounce_paths, startup_error)
        }
        DisplayKind::Headless => {
            set_status(&status, "disabled (no display detected)".to_string());
            eprintln!(
                "screen: no /dev/fb0 and no Wayland display; connection info is in the log only"
            );
        }
    }
}

/// Paint connection info to `/dev/fb0` on a background thread. Returns
/// immediately after spawning it.
#[cfg(all(target_os = "linux", feature = "fb", not(feature = "sdl")))]
fn show_framebuffer(
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    bounce_paths: Vec<std::path::PathBuf>,
    startup_error: Option<String>,
) {
    use std::{thread, time::Duration};

    // Background thread: keep the screen painted in case the text console
    // cursor (or anything else) overwrites the framebuffer.
    thread::spawn(move || match framebuffer::Framebuffer::new("/dev/fb0") {
        Ok(mut fb) => {
            let geom = match imp::Geom::probe(&fb) {
                Ok(g) => g,
                Err(e) => {
                    set_status(&status, format!("geometry failed: {e}"));
                    eprintln!("screen: {e}; connection info is in the log only");
                    return;
                }
            };

            let mut page = 0usize;
            let mut frame = 0u64;
            let mut last_mode: Option<Mode> = None;
            let mut source = crate::render::FrameSource::new(
                port,
                password,
                startup_error,
                Some(crate::bounce::Bounce::new(bounce_paths)),
            );

            loop {
                let mode = mode.lock().map(|m| *m).unwrap_or(Mode::Info);
                let changed = last_mode != Some(mode);

                // Info/Black are static: only re-blit on a mode change or every
                // couple of seconds (deliberate — the console cursor stomps the
                // fb, so the periodic re-latch stays). Bounce animates every
                // frame. The blit reuses the FrameSource's cached canvas; the
                // QR/info render itself only re-runs when the mode, dims, or
                // IP actually changed (issue #39).
                let render = match mode {
                    Mode::Bounce => true,
                    _ => changed || frame.is_multiple_of(40),
                };

                if render {
                    let (_, canvas) = source.frame(mode, geom.lw, geom.lh);
                    match imp::commit(&mut fb, &geom, canvas, page) {
                        Ok(info) => set_status(&status, format!("ok ({info}) mode={mode:?}")),
                        Err(e) => {
                            set_status(&status, format!("render failed: {e}"));
                            eprintln!("screen: render failed ({e}); info is in the log only");
                            break;
                        }
                    }
                    // Alternate the page each paint so the pan ioctl always
                    // changes the offset and the display engine re-latches.
                    page ^= 1;
                }

                last_mode = Some(mode);
                frame = frame.wrapping_add(1);
                // ~12.5 fps while bouncing; idle modes wake to check the mode.
                thread::sleep(Duration::from_millis(80));
            }
        }
        Err(e) => {
            set_status(&status, format!("cannot open /dev/fb0: {e:?}"));
            eprintln!("screen: cannot open /dev/fb0 ({e:?}); connection info is in the log only");
        }
    });
}

#[cfg(not(all(target_os = "linux", any(feature = "fb", feature = "sdl"))))]
pub fn show(
    _port: u16,
    _password: Option<String>,
    status: Status,
    _mode: ModeHandle,
    _bounce_paths: Vec<std::path::PathBuf>,
    _startup_error: Option<String>,
    _shutdown: tokio_util::sync::CancellationToken,
) {
    set_status(&status, "disabled (headless build)".to_string());
}

/// Rotation for the landscape-authored canvas, plus whether it was chosen
/// automatically. An explicit `AMBERDAV_FB_ROTATE` value always wins, snapped
/// to 0/90/180/270 exactly as before. Without one (or with an unparseable
/// one), portrait-mounted panels — the framebuffer reports taller than wide,
/// e.g. the RG34XXSP under the stock OS — default to 90 so the info screen
/// comes up upright instead of sideways (issue #38); landscape and square
/// panels keep 0. The stock firmware offers no usable device identity
/// (`/proc/device-tree/model` is the generic "sun50iw9" on every H700
/// device), so the probed geometry *is* the detection signal. The `bool`
/// rides along so the status string can tag an auto-chosen turn.
#[allow(dead_code)] // used on framebuffer (device) builds and in tests
fn pick_rotation(explicit: Option<&str>, xres: usize, yres: usize) -> (u32, bool) {
    if let Some(d) = explicit.and_then(|s| s.trim().parse::<u32>().ok()) {
        return ((d / 90 % 4) * 90, false);
    }
    if yres > xres {
        (90, true)
    } else {
        (0, false)
    }
}

/// Logical canvas dimensions for a given rotation and physical resolution.
/// For 90/270 the canvas is authored landscape then turned to fit the panel.
#[allow(dead_code)] // used on handheld (device) builds and in tests
fn logical_dims(rot: u32, xres: usize, yres: usize) -> (usize, usize) {
    if rot == 90 || rot == 270 {
        (yres, xres)
    } else {
        (xres, yres)
    }
}

/// A packed-RGB channel layout: `(offset, length)` per channel, mirroring the
/// pieces of the fbdev `Bitfield`s that [`pack`] consumes. Kept as plain
/// tuples so the layout decision and the channel math are host-testable.
#[allow(dead_code)] // used on framebuffer (device) builds and in tests
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct RgbLayout {
    red: (u32, u32),
    green: (u32, u32),
    blue: (u32, u32),
}

/// The de-facto 16bpp layout (5-6-5, red in the high bits).
#[allow(dead_code)] // used on framebuffer (device) builds and in tests
const RGB565: RgbLayout = RgbLayout {
    red: (11, 5),
    green: (5, 6),
    blue: (0, 5),
};

/// The de-facto 32bpp layout (8-8-8 in the low 24 bits, padding on top).
#[allow(dead_code)] // used on framebuffer (device) builds and in tests
const XRGB8888: RgbLayout = RgbLayout {
    red: (16, 8),
    green: (8, 8),
    blue: (0, 8),
};

/// Some DRM-backed fbdev shims report zeroed red/green/blue bitfield lengths
/// in `VarScreeninfo`. Taken at face value, [`pack`] returns 0 for every
/// pixel — an entirely black screen whose status still reads "ok" (issue
/// #37). When **all three** lengths are zero, assume the de-facto layout for
/// the reported depth instead: 16bpp → [`RGB565`], 32bpp → [`XRGB8888`];
/// the name rides along so the sink can surface the assumption in the status
/// string. Returns `None` — keep the driver's report — when any channel has
/// a nonzero length or when the depth has no safe assumption.
#[allow(dead_code)] // used on framebuffer (device) builds and in tests
fn fallback_rgb_layout(
    reported: RgbLayout,
    bits_per_pixel: u32,
) -> Option<(RgbLayout, &'static str)> {
    let all_zero = reported.red.1 == 0 && reported.green.1 == 0 && reported.blue.1 == 0;
    if !all_zero {
        return None;
    }
    match bits_per_pixel {
        16 => Some((RGB565, "rgb565")),
        32 => Some((XRGB8888, "xrgb8888")),
        _ => None,
    }
}

/// Pack an 8-bit RGB triple into the framebuffer's native pixel value
/// according to `layout`. Pure (and host-tested) so the fallback layouts
/// above are provably non-black.
#[allow(dead_code)] // used on framebuffer (device) builds and in tests
fn pack(r: u8, g: u8, b: u8, layout: &RgbLayout) -> u32 {
    let chan = |v: u8, (offset, len): (u32, u32)| -> u32 {
        if len == 0 {
            return 0;
        }
        let scaled = if len >= 8 {
            (v as u32) << (len - 8)
        } else {
            (v as u32) >> (8 - len)
        };
        (scaled & ((1u32 << len) - 1)) << offset
    };
    chan(r, layout.red) | chan(g, layout.green) | chan(b, layout.blue)
}

/// Inverse rotation map: physical pixel (px,py) -> source logical pixel (lx,ly).
#[allow(dead_code)] // used on handheld (device) builds and in tests
fn logical_coords(
    rot: u32,
    px: usize,
    py: usize,
    xres: usize,
    lw: usize,
    lh: usize,
) -> (usize, usize) {
    match rot {
        90 => (py, xres - 1 - px),
        180 => (lw - 1 - px, lh - 1 - py),
        270 => (lw - 1 - py, px),
        _ => (px, py),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every physical pixel must map to an in-bounds logical pixel for all
    // rotations, so the blit can never panic regardless of panel orientation.
    #[test]
    fn rotation_maps_stay_in_bounds() {
        for &(xres, yres) in &[(640usize, 480usize), (720, 480), (480, 640)] {
            for &rot in &[0u32, 90, 180, 270] {
                let (lw, lh) = logical_dims(rot, xres, yres);
                for py in 0..yres {
                    for px in 0..xres {
                        let (lx, ly) = logical_coords(rot, px, py, xres, lw, lh);
                        assert!(
                            lx < lw && ly < lh,
                            "rot={rot} ({px},{py}) -> ({lx},{ly}) oob ({lw}x{lh})"
                        );
                        assert!(ly * lw + lx < lw * lh);
                    }
                }
            }
        }
    }

    // An explicit AMBERDAV_FB_ROTATE always wins (snapped to a multiple of
    // 90, exactly the pre-existing parse), on any panel shape.
    #[test]
    fn explicit_rotation_always_wins() {
        // Portrait panel, every explicit value — including 0 to pin the old
        // default on a panel the heuristic would otherwise turn.
        assert_eq!(pick_rotation(Some("0"), 480, 720), (0, false));
        assert_eq!(pick_rotation(Some("90"), 480, 720), (90, false));
        assert_eq!(pick_rotation(Some("180"), 480, 720), (180, false));
        assert_eq!(pick_rotation(Some("270"), 480, 720), (270, false));
        // Snapping and whitespace handling are unchanged.
        assert_eq!(pick_rotation(Some("450"), 640, 480), (90, false));
        assert_eq!(pick_rotation(Some(" 270 "), 640, 480), (270, false));
    }

    // Without an override the probed geometry is the detection signal: the
    // canvas is authored landscape, so a portrait framebuffer (RG34XXSP under
    // the stock OS) is exactly the renders-sideways signature — default to a
    // 90° turn there, and to 0 on landscape/square panels (RG35XX Pro,
    // RGcubeXX) so working devices are untouched.
    #[test]
    fn portrait_panels_default_to_a_quarter_turn() {
        assert_eq!(pick_rotation(None, 480, 720), (90, true));
        assert_eq!(pick_rotation(None, 480, 640), (90, true));
        assert_eq!(pick_rotation(None, 640, 480), (0, false));
        assert_eq!(pick_rotation(None, 720, 480), (0, false));
        assert_eq!(pick_rotation(None, 720, 720), (0, false));
        // An unparseable override falls back to the heuristic.
        assert_eq!(pick_rotation(Some("sideways"), 480, 720), (90, true));
        assert_eq!(pick_rotation(Some(""), 640, 480), (0, false));
    }

    // The shim signature this guards against: ALL THREE bitfield lengths
    // zero. 16/32bpp get the de-facto layout (named, so the status string
    // can say so); unusual depths have no safe guess and keep the report.
    #[test]
    fn zeroed_bitfields_fall_back_by_depth() {
        let zero = RgbLayout {
            red: (0, 0),
            green: (0, 0),
            blue: (0, 0),
        };
        assert_eq!(fallback_rgb_layout(zero, 16), Some((RGB565, "rgb565")));
        assert_eq!(fallback_rgb_layout(zero, 32), Some((XRGB8888, "xrgb8888")));
        assert_eq!(fallback_rgb_layout(zero, 24), None);
        assert_eq!(fallback_rgb_layout(zero, 8), None);
        assert_eq!(fallback_rgb_layout(zero, 0), None);
    }

    // A driver that reports real bitfields — even unusual ones like BGR, and
    // even with a single nonzero channel — is trusted as-is; the fallback
    // must never override a usable report.
    #[test]
    fn reported_bitfields_are_left_untouched() {
        assert_eq!(fallback_rgb_layout(RGB565, 16), None);
        assert_eq!(fallback_rgb_layout(XRGB8888, 32), None);
        let bgr565 = RgbLayout {
            red: (0, 5),
            green: (5, 6),
            blue: (11, 5),
        };
        assert_eq!(fallback_rgb_layout(bgr565, 16), None);
        let partial = RgbLayout {
            red: (0, 0),
            green: (5, 6),
            blue: (0, 0),
        };
        assert_eq!(fallback_rgb_layout(partial, 16), None);
    }

    // Channel math under the fallback layouts: known colors land in the
    // right bits, and the all-zero layout reproduces the original bug
    // (everything packs to 0 — black) so the fallback provably matters.
    #[test]
    fn pack_is_nonblack_under_the_fallback_layouts() {
        assert_eq!(pack(255, 255, 255, &RGB565), 0xFFFF);
        assert_eq!(pack(255, 0, 0, &RGB565), 0xF800);
        assert_eq!(pack(0, 255, 0, &RGB565), 0x07E0);
        assert_eq!(pack(0, 0, 255, &RGB565), 0x001F);
        assert_eq!(pack(255, 255, 255, &XRGB8888), 0x00FF_FFFF);
        assert_eq!(pack(0xAB, 0xCD, 0xEF, &XRGB8888), 0x00AB_CDEF);
        let zero = RgbLayout {
            red: (0, 0),
            green: (0, 0),
            blue: (0, 0),
        };
        assert_eq!(pack(255, 255, 255, &zero), 0);
    }

    // Info <-> mode <-> Info round-trips through the same button.
    #[test]
    fn toggle_round_trips() {
        let h = mode_handle();
        assert_eq!(*h.lock().unwrap(), Mode::Info);
        toggle(&h, Mode::Black);
        assert_eq!(*h.lock().unwrap(), Mode::Black);
        toggle(&h, Mode::Black);
        assert_eq!(*h.lock().unwrap(), Mode::Info);
        // The other button switches modes directly.
        toggle(&h, Mode::Black);
        toggle(&h, Mode::Bounce);
        assert_eq!(*h.lock().unwrap(), Mode::Bounce);
    }
}

#[cfg(all(target_os = "linux", feature = "fb", not(feature = "sdl")))]
mod imp {
    use framebuffer::{Framebuffer, VarScreeninfo};

    /// Framebuffer geometry, probed once and reused for every frame.
    pub struct Geom {
        pub xres: usize,
        pub yres: usize,
        pub line: usize,
        pub bytespp: usize,
        pub xoff: usize,
        pub pages: usize,
        pub rot: u32,
        /// True when `rot` came from the portrait-panel heuristic rather than
        /// an explicit `AMBERDAV_FB_ROTATE` (tagged in the status string).
        rot_auto: bool,
        /// Logical canvas dimensions (landscape-as-authored).
        pub lw: usize,
        pub lh: usize,
        /// Effective channel layout for `pack` — the driver's bitfields, or
        /// the bpp-derived fallback when the driver reported all-zero ones.
        layout: super::RgbLayout,
        /// Fallback layout name when one was assumed (surfaced in the status
        /// string so a black-but-"ok" screen is diagnosable), else `None`.
        assumed: Option<&'static str>,
        var: VarScreeninfo,
    }

    impl Geom {
        pub fn probe(fb: &Framebuffer) -> Result<Geom, String> {
            let var = fb.var_screen_info.clone();
            let xres = var.xres as usize;
            let yres = var.yres as usize;
            if xres == 0 || yres == 0 {
                return Err("framebuffer reports zero size".into());
            }
            let bytespp = (var.bits_per_pixel / 8).max(1) as usize;
            let line = fb.fix_screen_info.line_length as usize;
            let xoff = var.xoffset as usize;
            // Panel may be double/triple-buffered (virtual height > visible).
            let pages = (var.yres_virtual as usize / yres).max(1);
            // Explicit env override wins; otherwise a portrait-mounted panel
            // (e.g. RG34XXSP) gets the canvas auto-turned 90° (issue #38).
            let explicit = std::env::var("AMBERDAV_FB_ROTATE").ok();
            let (rot, rot_auto) = super::pick_rotation(explicit.as_deref(), xres, yres);
            let (lw, lh) = super::logical_dims(rot, xres, yres);
            // Some DRM-backed fbdev shims report all-zero RGB bitfields,
            // which would pack every pixel to 0 (black screen, "ok" status).
            // Fall back to the de-facto layout for the depth (issue #37).
            let reported = super::RgbLayout {
                red: (var.red.offset, var.red.length),
                green: (var.green.offset, var.green.length),
                blue: (var.blue.offset, var.blue.length),
            };
            let (layout, assumed) = match super::fallback_rgb_layout(reported, var.bits_per_pixel) {
                Some((fallback, name)) => (fallback, Some(name)),
                None => (reported, None),
            };
            Ok(Geom {
                xres,
                yres,
                line,
                bytespp,
                xoff,
                pages,
                rot,
                rot_auto,
                lw,
                lh,
                layout,
                assumed,
                var,
            })
        }
    }

    /// Blit a logical RGB canvas to the framebuffer (applying rotation + the
    /// device pixel format) and commit it via FBIOPAN_DISPLAY. Returns a short
    /// geometry description on success.
    pub fn commit(
        fb: &mut Framebuffer,
        g: &Geom,
        canvas: &[[u8; 3]],
        want_page: usize,
    ) -> Result<String, String> {
        let frame: &mut [u8] = &mut fb.frame;
        for py in 0..g.yres {
            for px in 0..g.xres {
                let (lx, ly) = super::logical_coords(g.rot, px, py, g.xres, g.lw, g.lh);
                let [r, gg, b] = canvas[ly * g.lw + lx];
                let color = super::pack(r, gg, b, &g.layout);
                // Paint every page — we don't know which is being scanned out.
                for page in 0..g.pages {
                    let idx = (page * g.yres + py) * g.line + (px + g.xoff) * g.bytespp;
                    if idx + g.bytespp <= frame.len() {
                        for k in 0..g.bytespp {
                            frame[idx + k] = ((color >> (8 * k)) & 0xff) as u8;
                        }
                    }
                }
            }
        }

        // Commit: pan to the chosen page so the display engine fetches our
        // buffer. Many Allwinner fb drivers only latch on FBIOPAN_DISPLAY.
        let target = want_page % g.pages;
        let mut pan = g.var.clone();
        pan.xoffset = 0;
        pan.yoffset = (target * g.yres) as u32;
        let panned = Framebuffer::pan_display(&fb.device, &pan).is_ok();

        // `fmt=…(assumed)` appears only when the driver reported all-zero
        // bitfields and the layout was derived from the depth — so a wrong
        // guess is visible in the Status tab instead of masquerading as ok.
        let fmt = match g.assumed {
            Some(name) => format!(" fmt={name}(assumed)"),
            None => String::new(),
        };
        // "(auto)" marks the portrait-panel default, so a wrongly-guessed
        // turn is identifiable from the Status tab (override with
        // AMBERDAV_FB_ROTATE).
        let auto = if g.rot_auto { "(auto)" } else { "" };
        Ok(format!(
            "{}x{} {}bpp rot={}{auto} pages={} virt={} pan={}{fmt}",
            g.xres, g.yres, g.var.bits_per_pixel, g.rot, g.pages, g.var.yres_virtual, panned
        ))
    }
}
