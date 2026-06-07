//! On-device screen output. Draws the connection info (IP, password) and a QR
//! code straight to the Linux framebuffer (`/dev/fb0`) so the handheld is
//! usable without already knowing its IP. Linux-only; a no-op elsewhere.
//!
//! The displayed mode is shared with the input thread so the gamepad can drive
//! it: the A button blanks the screen, and the X button starts a "DVD bounce"
//! screensaver that drifts random images around to prevent burn-in.
//!
//! If the image comes out rotated on a given panel, set the env var
//! `AMBERDAV_FB_ROTATE` to 90, 180, or 270.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Shared, human-readable framebuffer status (surfaced on the web status page
/// so the screen can be diagnosed remotely without looking at the panel).
pub type Status = Arc<Mutex<String>>;

/// What the screen is currently showing. Toggled live from the input thread.
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

/// evdev key codes for the face buttons that drive the screen.
pub const BTN_SOUTH: u16 = 304; // "A" → blank
pub const BTN_NORTH: u16 = 307; // "X" → bounce screensaver

/// Create the shared mode handle (starts on [`Mode::Info`]).
pub fn mode_handle() -> ModeHandle {
    Arc::new(Mutex::new(Mode::Info))
}

/// Toggle between `target` and [`Mode::Info`]: pressing the button again (or
/// the other mode's button) returns to the info screen.
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

#[cfg(target_os = "linux")]
pub fn show(
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    bounce_paths: Vec<PathBuf>,
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
            let mut bounce = imp::Bounce::new(bounce_paths);

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
                            let ip = crate::current_ip();
                            imp::info_canvas(&geom, ip, port, pw.as_deref())
                        }
                        Mode::Black => vec![[0u8; 3]; geom.lw * geom.lh],
                        Mode::Bounce => {
                            bounce.step(&geom);
                            bounce.canvas(&geom)
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

#[cfg(not(target_os = "linux"))]
pub fn show(
    _port: u16,
    _password: Option<String>,
    status: Status,
    _mode: ModeHandle,
    _bounce_paths: Vec<PathBuf>,
) {
    set(&status, "disabled (non-Linux build)".to_string());
}

/// Logical canvas dimensions for a given rotation and physical resolution.
/// For 90/270 the canvas is authored landscape then turned to fit the panel.
#[allow(dead_code)] // used on Linux (device) builds and in tests
fn logical_dims(rot: u32, xres: usize, yres: usize) -> (usize, usize) {
    if rot == 90 || rot == 270 {
        (yres, xres)
    } else {
        (xres, yres)
    }
}

/// Inverse rotation map: physical pixel (px,py) -> source logical pixel (lx,ly).
#[allow(dead_code)] // used on Linux (device) builds and in tests
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

#[cfg(target_os = "linux")]
mod imp {
    use std::net::IpAddr;
    use std::path::PathBuf;

    use font8x8::legacy::BASIC_LEGACY;
    use framebuffer::{Bitfield, Framebuffer, VarScreeninfo};
    use qrcode::{Color, QrCode};
    use rand::Rng;

    const BLACK: [u8; 3] = [0, 0, 0];
    const WHITE: [u8; 3] = [255, 255, 255];

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

    /// Build the connection-info canvas: IP, credentials, and a QR code.
    pub fn info_canvas(g: &Geom, ip: IpAddr, port: u16, password: Option<&str>) -> Vec<[u8; 3]> {
        let (lw, lh) = (g.lw, g.lh);
        let mut canvas = vec![WHITE; lw * lh];

        let scale = (lw / 240).max(2);
        let line_h = 8 * scale + scale * 2;
        let margin = scale * 3;

        // App version, pinned to the bottom-right corner. Drawn first so it is
        // present on both the "waiting for Wi-Fi" and full info screens (the
        // QR/info view), and the centered QR never lands on top of it.
        draw_version(&mut canvas, lw, lh, margin, scale);

        let mut y = margin;

        draw_text(
            &mut canvas,
            lw,
            lh,
            margin,
            y,
            "amber-dav  file access",
            scale,
        );
        y += line_h;

        // No network yet (0.0.0.0): a QR to http://0.0.0.0/ is useless, so just
        // ask the user to wait. The render loop repaints every ~2s, so once
        // Wi-Fi connects this recovers into the full info screen on its own.
        if ip.is_unspecified() {
            draw_text(&mut canvas, lw, lh, margin, y, "Waiting for Wi-Fi…", scale);
            return canvas;
        }

        draw_text(
            &mut canvas,
            lw,
            lh,
            margin,
            y,
            &format!("IP:   {ip}:{port}"),
            scale,
        );
        y += line_h;
        draw_text(&mut canvas, lw, lh, margin, y, "User: anything", scale);
        y += line_h;
        let pass_line = match password {
            Some(p) => format!("Pass: {p}"),
            None => "Pass: (hidden)".to_string(),
        };
        draw_text(&mut canvas, lw, lh, margin, y, &pass_line, scale);
        y += line_h + scale * 2;

        // QR of the status page URL, centered below the text.
        let url = format!("http://{ip}:{port}/");
        if let Ok(code) = QrCode::new(url.as_bytes()) {
            let w = code.width();
            let modules = code.to_colors();
            let quiet = 4usize;
            let total = w + quiet * 2;
            let avail_w = lw.saturating_sub(margin * 2);
            let avail_h = lh.saturating_sub(y + line_h + margin);
            let qs = (avail_w.min(avail_h) / total).max(1);
            let qpix = total * qs;
            let qx = lw.saturating_sub(qpix) / 2;
            let qy = y;

            for my in 0..w {
                for mx in 0..w {
                    if modules[my * w + mx] == Color::Dark {
                        fill_rect(
                            &mut canvas,
                            lw,
                            lh,
                            qx + (mx + quiet) * qs,
                            qy + (my + quiet) * qs,
                            qs,
                            qs,
                            BLACK,
                        );
                    }
                }
            }
            draw_text(&mut canvas, lw, lh, qx, qy + qpix, "Scan to connect", scale);
        }

        canvas
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

    /// A decoded image scaled to a sprite, plus the live bounce position.
    struct Sprite {
        w: usize,
        h: usize,
        px: Vec<[u8; 3]>,
    }

    /// State for the DVD-bounce screensaver.
    pub struct Bounce {
        /// Files/dirs from config; expanded into `images` on first activation.
        roots: Vec<PathBuf>,
        images: Vec<PathBuf>,
        scanned: bool,
        started: bool,
        sprite: Option<Sprite>,
        x: i32,
        y: i32,
        vx: i32,
        vy: i32,
    }

    impl Bounce {
        pub fn new(roots: Vec<PathBuf>) -> Bounce {
            Bounce {
                roots,
                images: Vec::new(),
                scanned: false,
                started: false,
                sprite: None,
                x: 0,
                y: 0,
                vx: 3,
                vy: 2,
            }
        }

        /// Advance the animation one frame: move the sprite, and on hitting an
        /// edge reflect its heading and swap to a new image — keeping position.
        pub fn step(&mut self, g: &Geom) {
            if !self.scanned {
                self.images = scan_images(&self.roots);
                self.scanned = true;
            }
            if self.images.is_empty() {
                return; // No images → black screen, which still prevents burn-in.
            }
            // First activation: place a centered sprite with a random heading.
            if !self.started {
                self.start(g);
                self.started = true;
                return;
            }
            let Some(sprite) = &self.sprite else {
                // Decode kept failing earlier; try again before moving.
                self.start(g);
                return;
            };

            let maxx = g.lw.saturating_sub(sprite.w) as i32;
            let maxy = g.lh.saturating_sub(sprite.h) as i32;
            self.x += self.vx;
            self.y += self.vy;
            let mut bounced = false;
            if self.x <= 0 {
                self.x = 0;
                self.vx = self.vx.abs();
                bounced = true;
            } else if self.x >= maxx {
                self.x = maxx;
                self.vx = -self.vx.abs();
                bounced = true;
            }
            if self.y <= 0 {
                self.y = 0;
                self.vy = self.vy.abs();
                bounced = true;
            } else if self.y >= maxy {
                self.y = maxy;
                self.vy = -self.vy.abs();
                bounced = true;
            }
            // Classic DVD behaviour: swap the image on each edge bounce. Keep
            // the current position and the just-reflected heading.
            if bounced && self.images.len() > 1 {
                self.swap_image(g);
            }
        }

        /// First placement: a centered sprite with a random diagonal heading.
        fn start(&mut self, g: &Geom) {
            let mut rng = rand::rng();
            if let Some(sprite) = self.pick_sprite(g, &mut rng) {
                self.x = (g.lw.saturating_sub(sprite.w) / 2) as i32;
                self.y = (g.lh.saturating_sub(sprite.h) / 2) as i32;
                self.vx = if rng.random_bool(0.5) { 3 } else { -3 };
                self.vy = if rng.random_bool(0.5) { 2 } else { -2 };
                self.sprite = Some(sprite);
            }
        }

        /// Replace the sprite with a new random image, preserving position and
        /// heading. Position is clamped so a larger image stays on screen.
        fn swap_image(&mut self, g: &Geom) {
            let mut rng = rand::rng();
            if let Some(sprite) = self.pick_sprite(g, &mut rng) {
                let maxx = g.lw.saturating_sub(sprite.w) as i32;
                let maxy = g.lh.saturating_sub(sprite.h) as i32;
                self.x = self.x.clamp(0, maxx.max(0));
                self.y = self.y.clamp(0, maxy.max(0));
                self.sprite = Some(sprite);
            }
        }

        /// Pick and decode a random image, sized to ~1/3 of the canvas. Tries a
        /// handful in case some fail to decode (e.g. a corrupt file).
        fn pick_sprite(&self, g: &Geom, rng: &mut impl Rng) -> Option<Sprite> {
            let cap = (g.lw.min(g.lh) / 3).max(32) as u32;
            for _ in 0..self.images.len().min(8) {
                let idx = rng.random_range(0..self.images.len());
                if let Some(sprite) = decode_sprite(&self.images[idx], cap) {
                    return Some(sprite);
                }
            }
            None
        }

        /// Render the current sprite onto a black canvas.
        pub fn canvas(&self, g: &Geom) -> Vec<[u8; 3]> {
            let mut canvas = vec![BLACK; g.lw * g.lh];
            if let Some(s) = &self.sprite {
                for sy in 0..s.h {
                    let ty = self.y + sy as i32;
                    if ty < 0 || ty >= g.lh as i32 {
                        continue;
                    }
                    for sx in 0..s.w {
                        let tx = self.x + sx as i32;
                        if tx < 0 || tx >= g.lw as i32 {
                            continue;
                        }
                        canvas[ty as usize * g.lw + tx as usize] = s.px[sy * s.w + sx];
                    }
                }
            }
            canvas
        }
    }

    /// Extensions we can decode for the screensaver.
    const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp"];

    fn is_image(path: &std::path::Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(false)
    }

    /// Expand the configured files/folders into a flat list of image files.
    /// Folders are walked recursively (bounded, to stay responsive).
    fn scan_images(roots: &[PathBuf]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack: Vec<PathBuf> = roots.to_vec();
        let cap = 5000;
        while let Some(p) = stack.pop() {
            if out.len() >= cap {
                break;
            }
            if p.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&p) {
                    for entry in entries.flatten() {
                        stack.push(entry.path());
                    }
                }
            } else if p.is_file() && is_image(&p) {
                out.push(p);
            }
        }
        eprintln!("screen: bounce screensaver found {} image(s)", out.len());
        out
    }

    /// Decode `path` and downscale so its largest side is at most `cap` pixels.
    fn decode_sprite(path: &std::path::Path, cap: u32) -> Option<Sprite> {
        let img = image::open(path).ok()?;
        let img = img.thumbnail(cap, cap).to_rgb8();
        let (w, h) = (img.width() as usize, img.height() as usize);
        if w == 0 || h == 0 {
            return None;
        }
        let px = img.pixels().map(|p| [p[0], p[1], p[2]]).collect();
        Some(Sprite { w, h, px })
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

    // Low-level blit helpers: explicit canvas dims + position read clearly at
    // the call sites, so the positional argument count is intentional.
    #[allow(clippy::too_many_arguments)]
    fn fill_rect(
        buf: &mut [[u8; 3]],
        w: usize,
        h: usize,
        x: usize,
        y: usize,
        rw: usize,
        rh: usize,
        val: [u8; 3],
    ) {
        for yy in y..(y + rh).min(h) {
            for xx in x..(x + rw).min(w) {
                buf[yy * w + xx] = val;
            }
        }
    }

    /// Draw the crate version (e.g. `v0.1.0`) flush against the bottom-right
    /// corner, inset by `margin`. The string is short enough to never reach the
    /// centered QR, so it shares the info screen without overlapping anything.
    fn draw_version(buf: &mut [[u8; 3]], w: usize, h: usize, margin: usize, scale: usize) {
        let text = concat!("v", env!("CARGO_PKG_VERSION"));
        let text_w = text.chars().count() * 8 * scale;
        let text_h = 8 * scale;
        let x = w.saturating_sub(text_w + margin);
        let y = h.saturating_sub(text_h + margin);
        draw_text(buf, w, h, x, y, text, scale);
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        buf: &mut [[u8; 3]],
        w: usize,
        h: usize,
        x: usize,
        y: usize,
        text: &str,
        scale: usize,
    ) {
        let mut cx = x;
        for ch in text.chars() {
            let glyph = BASIC_LEGACY
                .get(ch as usize)
                .copied()
                .unwrap_or(BASIC_LEGACY[' ' as usize]);
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8 {
                    // font8x8 is LSB-first: bit 0 is the leftmost column.
                    if bits & (1 << col) != 0 {
                        fill_rect(
                            buf,
                            w,
                            h,
                            cx + col * scale,
                            y + row * scale,
                            scale,
                            scale,
                            BLACK,
                        );
                    }
                }
            }
            cx += 8 * scale;
        }
    }
}
