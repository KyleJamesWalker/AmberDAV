//! On-device screen output. Draws the connection info (IP, password) and a QR
//! code straight to the Linux framebuffer (`/dev/fb0`) so the handheld is
//! usable without already knowing its IP. Compiled in with the `fb` or `sdl`
//! feature; a no-op stub otherwise (desktop/server builds).
//!
//! The displayed mode is shared with the input thread so the gamepad can drive
//! it: the A button blanks the screen, and the X button starts a "DVD bounce"
//! screensaver that drifts random images around to prevent burn-in.
//!
//! If the image comes out rotated on a given panel, set the env var
//! `AMBERDAV_FB_ROTATE` to 90, 180, or 270.

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

fn set(status: &Status, msg: String) {
    if let Ok(mut s) = status.lock() {
        *s = msg;
    }
}

/// SDL build: always use the SDL sink (it auto-selects the video driver).
#[cfg(feature = "sdl")]
pub fn show(
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    bounce_paths: Vec<std::path::PathBuf>,
    config_error: Option<String>,
) {
    set(&status, "sdl: starting…".to_string());
    std::thread::spawn(move || {
        if let Err(e) = crate::sdl::run(
            port,
            password,
            status.clone(),
            mode,
            bounce_paths,
            config_error,
        ) {
            set(&status, format!("sdl failed: {e}"));
            eprintln!("screen: sdl sink failed ({e}); connection info is in the log only");
        }
    });
}

/// Pick the active display sink (Wayland in Game Mode, framebuffer on the
/// Anbernic/TTY/Desktop Mode, else headless) and start painting connection
/// info. Returns immediately; the chosen sink runs in a background thread.
#[cfg(all(feature = "fb", not(feature = "sdl")))]
pub fn show(
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    bounce_paths: Vec<std::path::PathBuf>,
    config_error: Option<String>,
) {
    use crate::display::{detect, DisplayKind};
    match detect() {
        DisplayKind::Wayland => {
            let socket = crate::display::wayland_socket();
            set(&status, "wayland: starting…".to_string());
            std::thread::spawn(move || {
                if let Err(e) =
                    crate::wayland::run(port, password, status.clone(), mode, socket, config_error)
                {
                    set(&status, format!("wayland failed: {e}"));
                    eprintln!(
                        "screen: wayland sink failed ({e}); connection info is in the log only"
                    );
                }
            });
        }
        DisplayKind::Framebuffer => {
            show_framebuffer(port, password, status, mode, bounce_paths, config_error)
        }
        DisplayKind::Headless => {
            set(&status, "disabled (no display detected)".to_string());
            eprintln!(
                "screen: no /dev/fb0 and no Wayland display; connection info is in the log only"
            );
        }
    }
}

/// Paint connection info to `/dev/fb0` on a background thread. Returns
/// immediately after spawning it.
#[cfg(all(feature = "fb", not(feature = "sdl")))]
fn show_framebuffer(
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    bounce_paths: Vec<std::path::PathBuf>,
    config_error: Option<String>,
) {
    use std::{thread, time::Duration};

    let pw = password;
    // Background thread: keep the screen painted in case the text console
    // cursor (or anything else) overwrites the framebuffer.
    thread::spawn(move || match framebuffer::Framebuffer::new("/dev/fb0") {
        Ok(mut fb) => {
            let geom = match imp::Geom::probe(&fb) {
                Ok(g) => g,
                Err(e) => {
                    set(&status, format!("geometry failed: {e}"));
                    eprintln!("screen: {e}; connection info is in the log only");
                    return;
                }
            };

            let mut page = 0usize;
            let mut frame = 0u64;
            let mut last_mode: Option<Mode> = None;
            let mut bounce = crate::bounce::Bounce::new(bounce_paths);

            loop {
                let mode = mode.lock().map(|m| *m).unwrap_or(Mode::Info);
                let changed = last_mode != Some(mode);

                // Info/Black are static: only repaint on a mode change or every
                // couple of seconds (to re-latch). Bounce animates every frame.
                let render = match mode {
                    Mode::Bounce => true,
                    _ => changed || frame.is_multiple_of(40),
                };

                if render {
                    let canvas = match mode {
                        Mode::Info => {
                            // Re-query the IP each paint so the screen recovers
                            // once Wi-Fi connects after launch.
                            let ip = crate::state::current_ip();
                            crate::canvas::info_canvas(
                                geom.lw,
                                geom.lh,
                                ip,
                                port,
                                pw.as_deref(),
                                config_error.as_deref(),
                            )
                            .px
                        }
                        Mode::Black => crate::canvas::black_canvas(geom.lw, geom.lh).px,
                        Mode::Bounce => {
                            bounce.step(geom.lw, geom.lh);
                            bounce.canvas(geom.lw, geom.lh)
                        }
                    };
                    match imp::commit(&mut fb, &geom, &canvas, page) {
                        Ok(info) => set(&status, format!("ok ({info}) mode={mode:?}")),
                        Err(e) => {
                            set(&status, format!("render failed: {e}"));
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
            set(&status, format!("cannot open /dev/fb0: {e:?}"));
            eprintln!("screen: cannot open /dev/fb0 ({e:?}); connection info is in the log only");
        }
    });
}

#[cfg(not(any(feature = "fb", feature = "sdl")))]
pub fn show(
    _port: u16,
    _password: Option<String>,
    status: Status,
    _mode: ModeHandle,
    _bounce_paths: Vec<std::path::PathBuf>,
    _config_error: Option<String>,
) {
    set(&status, "disabled (headless build)".to_string());
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

#[cfg(all(feature = "fb", not(feature = "sdl")))]
mod imp {
    use framebuffer::{Bitfield, Framebuffer, VarScreeninfo};

    fn rotation() -> u32 {
        std::env::var("AMBERDAV_FB_ROTATE")
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(|d| (d / 90 % 4) * 90)
            .unwrap_or(0)
    }

    /// Framebuffer geometry, probed once and reused for every frame.
    pub struct Geom {
        pub xres: usize,
        pub yres: usize,
        pub line: usize,
        pub bytespp: usize,
        pub xoff: usize,
        pub pages: usize,
        pub rot: u32,
        /// Logical canvas dimensions (landscape-as-authored).
        pub lw: usize,
        pub lh: usize,
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
            let rot = rotation();
            let (lw, lh) = super::logical_dims(rot, xres, yres);
            Ok(Geom {
                xres,
                yres,
                line,
                bytespp,
                xoff,
                pages,
                rot,
                lw,
                lh,
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
                let color = pack(r, gg, b, &g.var);
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

        Ok(format!(
            "{}x{} {}bpp rot={} pages={} virt={} pan={}",
            g.xres, g.yres, g.var.bits_per_pixel, g.rot, g.pages, g.var.yres_virtual, panned
        ))
    }

    /// Pack an 8-bit RGB triple into the framebuffer's native pixel value.
    fn pack(r: u8, g: u8, b: u8, var: &VarScreeninfo) -> u32 {
        let chan = |v: u8, bf: &Bitfield| -> u32 {
            let len = bf.length;
            if len == 0 {
                return 0;
            }
            let scaled = if len >= 8 {
                (v as u32) << (len - 8)
            } else {
                (v as u32) >> (8 - len)
            };
            (scaled & ((1u32 << len) - 1)) << bf.offset
        };
        chan(r, &var.red) | chan(g, &var.green) | chan(b, &var.blue)
    }
}
