//! Connection-info canvas: the IP, credentials, and QR rendered into a plain
//! RGB pixel buffer. This is the device-screen *content*, kept independent of
//! how it reaches a panel — the framebuffer sink (Anbernic/TTY/Desktop Mode)
//! and the Wayland sink (Steam Deck) both present the same `Canvas`.
//! Pure and platform-independent, so it is unit-tested on every host.

// On headless builds the canvas API is not called (the framebuffer/Wayland and
// SDL sinks that consume it are only compiled with the `fb`/`sdl` features).
// Suppress dead-code lints so that `cargo clippy -- -D warnings` stays clean on
// every host without hiding real dead code on device builds.
#![cfg_attr(not(any(feature = "fb", feature = "sdl")), allow(dead_code))]

use std::net::IpAddr;

use font8x8::legacy::BASIC_LEGACY;
use qrcode::{Color, QrCode};

const BLACK: [u8; 3] = [0, 0, 0];
const WHITE: [u8; 3] = [255, 255, 255];

// Web-UI palette (kept in sync with src/web/*.html `:root`) so the on-screen
// info matches the browser theme instead of being a jarring white page.
const BG: [u8; 3] = [0x0e, 0x0f, 0x13]; // --bg
const TEXT: [u8; 3] = [0xd8, 0xdd, 0xe5]; // --text
const MUTED: [u8; 3] = [0x8b, 0x93, 0xa3]; // --muted
const AMBER: [u8; 3] = [0xff, 0xb4, 0x54]; // --amber (accent)

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
pub fn info_canvas(w: usize, h: usize, ip: IpAddr, port: u16, password: Option<&str>) -> Canvas {
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
        // A white "card" behind the code (its qpix square already includes the
        // 4-module quiet zone, so this is the required light border). Keeping the
        // QR dark-on-light on this card stays reliably scannable even though the
        // surrounding screen is the dark web-UI theme.
        fill_rect(&mut c, qx, qy, qpix, qpix, WHITE);
        for my in 0..qw {
            for mx in 0..qw {
                if modules[my * qw + mx] == Color::Dark {
                    fill_rect(
                        &mut c,
                        qx + (mx + quiet) * qs,
                        qy + (my + quiet) * qs,
                        qs,
                        qs,
                        BLACK,
                    );
                }
            }
        }
        // Label below the card, on the dark background (so it reads in TEXT, not
        // on the white card where a light colour would vanish).
        draw_text(
            &mut c,
            qx,
            qy + qpix + scale,
            "Scan to connect",
            scale,
            TEXT,
        );
    }
    c
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
        let c = info_canvas(480, 320, ip, 8080, Some("ab12"));
        assert_eq!(c.w, 480);
        assert_eq!(c.h, 320);
        assert_eq!(c.px.len(), 480 * 320);
        // Themed: dark background, with content (text + the white QR card) on top.
        assert!(c.px.contains(&BG), "background not the dark theme");
        assert!(c.px.contains(&WHITE), "QR card (white) not drawn");
        assert!(c.px.contains(&AMBER), "amber heading not drawn");
    }

    #[test]
    fn info_canvas_waiting_for_wifi_when_unspecified() {
        let c = info_canvas(480, 320, IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080, None);
        assert_eq!((c.w, c.h), (480, 320));
        // The heading is drawn even while waiting for Wi-Fi.
        assert!(c.px.contains(&AMBER));
        // Center stays the dark background: the QR/white card was skipped.
        let (cx, cy) = (c.w / 2, c.h / 2);
        assert_eq!(
            c.px[cy * c.w + cx],
            BG,
            "QR should not appear on unspecified IP"
        );
        // No white QR card on the waiting screen.
        assert!(
            !c.px.contains(&WHITE),
            "QR card should be absent while waiting"
        );
    }

    #[test]
    fn black_canvas_is_all_black() {
        let c = black_canvas(64, 48);
        assert_eq!(c.px.len(), 64 * 48);
        assert!(c.px.iter().all(|p| *p == [0, 0, 0]));
    }
}
