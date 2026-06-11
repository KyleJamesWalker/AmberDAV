//! Connection-info canvas: the IP, credentials, and QR rendered into a plain
//! RGB pixel buffer. This is the device-screen *content*, kept independent of
//! how it reaches a panel — the framebuffer sink (Anbernic/TTY/Desktop Mode)
//! and the Wayland sink (Steam Deck) both present the same `Canvas`.
//! Pure and platform-independent, so it is unit-tested on every host.
//!
//! See also — content: `canvas.rs` → choice: `display.rs` → sinks:
//! `screen.rs`/`sdl.rs`/`wayland.rs` → state: `screen::Mode`.

// On headless builds — and on non-Linux hosts, whatever the features — the
// canvas API is not called (the framebuffer/Wayland and SDL sinks that consume
// it are only compiled with the `fb`/`sdl` features on Linux). Suppress
// dead-code lints so that `cargo clippy -- -D warnings` stays clean on every
// host without hiding real dead code on device builds.
#![cfg_attr(
    not(all(target_os = "linux", any(feature = "fb", feature = "sdl"))),
    allow(dead_code)
)]

use std::net::IpAddr;

use font8x8::legacy::BASIC_LEGACY;
use qrcode::{Color, QrCode};

const BLACK: [u8; 3] = [0, 0, 0];

// Web-UI palette (kept in sync with src/web/*.html `:root`) so the on-screen
// info matches the browser theme instead of being a jarring white page.
const BG: [u8; 3] = [0x0e, 0x0f, 0x13]; // --bg
const TEXT: [u8; 3] = [0xd8, 0xdd, 0xe5]; // --text
const MUTED: [u8; 3] = [0x8b, 0x93, 0xa3]; // --muted
const AMBER: [u8; 3] = [0xff, 0xb4, 0x54]; // --amber (accent)
const DANGER: [u8; 3] = [0xff, 0x6b, 0x6b]; // --danger

/// An RGB pixel buffer, `w * h` pixels in row-major order.
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    pub px: Vec<[u8; 3]>,
}

impl Canvas {
    fn filled(w: usize, h: usize, color: [u8; 3]) -> Canvas {
        Canvas {
            w,
            h,
            px: vec![color; w * h],
        }
    }
}

/// A solid black canvas (the "blank the panel" mode).
pub fn black_canvas(w: usize, h: usize) -> Canvas {
    Canvas::filled(w, h, BLACK)
}

/// Build the connection-info canvas: IP, credentials, and a centered QR code.
/// `w`/`h` are the logical (landscape-as-authored) dimensions.
///
/// `startup_error` surfaces a startup problem (a broken config, issue #19; a
/// failed bind, issue #35) — on a handheld, stderr is invisible, so this
/// screen is where it must show up. The message's first line is the red
/// headline (e.g. "Config error - using defaults"); any remaining text is the
/// detail, wrapped below it in the muted colour.
pub fn info_canvas(
    w: usize,
    h: usize,
    ip: IpAddr,
    port: u16,
    password: Option<&str>,
    startup_error: Option<&str>,
) -> Canvas {
    let mut c = Canvas::filled(w, h, BG);
    let scale = (w / 240).max(2);
    let line_h = 8 * scale + scale * 2;
    let margin = scale * 3;

    // App version, pinned bottom-right. Drawn first so it appears on both the
    // "waiting for Wi-Fi" and full info screens, and the later (solid-filled) QR
    // can never paint over it.
    draw_version(&mut c, margin, scale);

    let mut y = margin;
    draw_text(&mut c, margin, y, "amber-dav  file access", scale, AMBER);
    y += line_h;

    // Drawn before the Wi-Fi early-return so a startup problem is diagnosable
    // even with no network. The detail carries e.g. the config parser's
    // line/column or the OS bind error.
    if let Some(err) = startup_error {
        let (head, detail) = err.split_once('\n').unwrap_or((err, ""));
        draw_text(&mut c, margin, y, head, scale, DANGER);
        y += line_h;
        let cols = (w.saturating_sub(margin * 2) / (8 * scale)).max(8);
        // The 8x8 font has no newline glyph; flatten any further line breaks.
        let detail = detail.replace('\n', " ");
        for line in wrap_chars(&detail, cols).into_iter().take(2) {
            draw_text(&mut c, margin, y, &line, scale, MUTED);
            y += line_h;
        }
    }

    if ip.is_unspecified() {
        draw_text(&mut c, margin, y, "Waiting for Wi-Fi…", scale, TEXT);
        return c;
    }

    draw_text(
        &mut c,
        margin,
        y,
        &format!("IP:   {ip}:{port}"),
        scale,
        TEXT,
    );
    y += line_h;
    draw_text(&mut c, margin, y, "User: anything", scale, TEXT);
    y += line_h;
    let pass_line = match password {
        Some(p) => format!("Pass: {p}"),
        None => "Pass: (hidden)".to_string(),
    };
    draw_text(&mut c, margin, y, &pass_line, scale, TEXT);
    y += line_h + scale * 2;

    let url = format!("http://{ip}:{port}/");
    if let Ok(code) = QrCode::new(url.as_bytes()) {
        let qw = code.width();
        let modules = code.to_colors();
        let quiet = 4usize;
        let total = qw + quiet * 2;
        let avail_w = w.saturating_sub(margin * 2);
        let avail_h = h.saturating_sub(y + line_h + margin);
        let qs = (avail_w.min(avail_h) / total).max(1);
        let qpix = total * qs;
        let qx = w.saturating_sub(qpix) / 2;
        let qy = y;
        // No card: the QR sits straight on the dark background, with its data
        // modules drawn in the amber accent. The surrounding (and inter-module)
        // background is the screen's dark colour, so the code reads inverted —
        // amber-on-dark — which current phone cameras scan fine.
        for my in 0..qw {
            for mx in 0..qw {
                if modules[my * qw + mx] == Color::Dark {
                    fill_rect(
                        &mut c,
                        qx + (mx + quiet) * qs,
                        qy + (my + quiet) * qs,
                        qs,
                        qs,
                        AMBER,
                    );
                }
            }
        }
        // Label just below the QR, on the dark background, in body text.
        draw_text(&mut c, qx, qy + qpix, "Scan to connect", scale, TEXT);
    }
    c
}

/// Hard-wrap `text` into lines of at most `cols` characters (the 8x8 font is
/// monospaced, so character count maps directly to pixels).
fn wrap_chars(text: &str, cols: usize) -> Vec<String> {
    text.chars()
        .collect::<Vec<_>>()
        .chunks(cols)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn fill_rect(c: &mut Canvas, x: usize, y: usize, rw: usize, rh: usize, val: [u8; 3]) {
    for yy in y..(y + rh).min(c.h) {
        for xx in x..(x + rw).min(c.w) {
            c.px[yy * c.w + xx] = val;
        }
    }
}

/// Crate version flush against the bottom-right corner, inset by `margin`.
fn draw_version(c: &mut Canvas, margin: usize, scale: usize) {
    let text = concat!("v", env!("CARGO_PKG_VERSION"));
    let text_w = text.chars().count() * 8 * scale;
    let text_h = 8 * scale;
    let x = c.w.saturating_sub(text_w + margin);
    let y = c.h.saturating_sub(text_h + margin);
    draw_text(c, x, y, text, scale, MUTED);
}

fn draw_text(c: &mut Canvas, x: usize, y: usize, text: &str, scale: usize, color: [u8; 3]) {
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
                    fill_rect(c, cx + col * scale, y + row * scale, scale, scale, color);
                }
            }
        }
        cx += 8 * scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn info_canvas_has_requested_dims_and_is_not_blank() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        let c = info_canvas(480, 320, ip, 8080, Some("ab12"), None);
        assert_eq!(c.w, 480);
        assert_eq!(c.h, 320);
        assert_eq!(c.px.len(), 480 * 320);
        // Themed: dark background, with amber content (heading + QR) drawn on it.
        assert!(c.px.contains(&BG), "background not the dark theme");
        assert!(c.px.contains(&AMBER), "amber content not drawn");
        // No config error → no danger-coloured warning anywhere.
        assert!(!c.px.contains(&DANGER), "warning drawn without an error");
    }

    // A startup problem (broken config, failed bind) must be visible on the
    // device screen — stderr is invisible on a handheld launched from the
    // menu (issues #19, #35).
    #[test]
    fn info_canvas_shows_startup_error_warning() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        let c = info_canvas(
            480,
            320,
            ip,
            8080,
            Some("ab12"),
            Some("Config error - using defaults\nconfig.json is invalid"),
        );
        assert!(c.px.contains(&DANGER), "startup error headline not drawn");
    }

    // The first message line is the headline; the rest is detail drawn below
    // it — so a canvas with detail must differ from a headline-only one.
    #[test]
    fn info_canvas_draws_the_error_detail_below_the_headline() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50));
        let with_detail = info_canvas(
            480,
            320,
            ip,
            8080,
            Some("ab12"),
            Some("Cannot start server\nport 8080 already in use"),
        );
        let head_only = info_canvas(
            480,
            320,
            ip,
            8080,
            Some("ab12"),
            Some("Cannot start server"),
        );
        assert!(with_detail.px.contains(&DANGER));
        assert_ne!(
            with_detail.px, head_only.px,
            "the detail line should be drawn"
        );
    }

    // The warning must also show while waiting for Wi-Fi — a user diagnosing
    // their config should not need a network connection first.
    #[test]
    fn info_canvas_shows_startup_error_even_without_ip() {
        let c = info_canvas(
            480,
            320,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            8080,
            None,
            Some("Config error - using defaults\nconfig.json is invalid"),
        );
        assert!(c.px.contains(&DANGER), "startup error headline not drawn");
    }

    #[test]
    fn info_canvas_waiting_for_wifi_when_unspecified() {
        let c = info_canvas(
            480,
            320,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            8080,
            None,
            None,
        );
        assert_eq!((c.w, c.h), (480, 320));
        // The heading is drawn even while waiting for Wi-Fi.
        assert!(c.px.contains(&AMBER));
        // Center stays the dark background: the QR was skipped.
        let (cx, cy) = (c.w / 2, c.h / 2);
        assert_eq!(
            c.px[cy * c.w + cx],
            BG,
            "QR should not appear on unspecified IP"
        );
    }

    #[test]
    fn black_canvas_is_all_black() {
        let c = black_canvas(64, 48);
        assert_eq!(c.px.len(), 64 * 48);
        assert!(c.px.iter().all(|p| *p == [0, 0, 0]));
    }
}
