//! DVD-bounce screensaver: drifts random images around the screen to prevent
//! burn-in on the always-on connection screen. Shared by every on-device sink
//! (framebuffer + SDL) — it operates on plain logical `(w, h)` pixel dimensions
//! and returns an RGB canvas, so it's independent of how those pixels are
//! presented. Compiled in with the `handheld` feature (which pulls the `image`
//! decoder); the `rand` crate is a base dependency.

use std::path::PathBuf;

use rand::Rng;

const BLACK: [u8; 3] = [0, 0, 0];

/// A decoded image scaled to a sprite (the moving picture).
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

    /// Advance the animation one frame on a `w`x`h` canvas: move the sprite, and
    /// on hitting an edge reflect its heading and swap to a new image — keeping
    /// position.
    pub fn step(&mut self, w: usize, h: usize) {
        if !self.scanned {
            self.images = scan_images(&self.roots);
            self.scanned = true;
        }
        if self.images.is_empty() {
            return; // No images → black screen, which still prevents burn-in.
        }
        // First activation: place a centered sprite with a random heading.
        if !self.started {
            self.start(w, h);
            self.started = true;
            return;
        }
        let Some(sprite) = &self.sprite else {
            // Decode kept failing earlier; try again before moving.
            self.start(w, h);
            return;
        };

        let maxx = w.saturating_sub(sprite.w) as i32;
        let maxy = h.saturating_sub(sprite.h) as i32;
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
        // Classic DVD behaviour: swap the image on each edge bounce. Keep the
        // current position and the just-reflected heading.
        if bounced && self.images.len() > 1 {
            self.swap_image(w, h);
        }
    }

    /// First placement: a centered sprite with a random diagonal heading.
    fn start(&mut self, w: usize, h: usize) {
        let mut rng = rand::rng();
        if let Some(sprite) = self.pick_sprite(w, h, &mut rng) {
            self.x = (w.saturating_sub(sprite.w) / 2) as i32;
            self.y = (h.saturating_sub(sprite.h) / 2) as i32;
            self.vx = if rng.random_bool(0.5) { 3 } else { -3 };
            self.vy = if rng.random_bool(0.5) { 2 } else { -2 };
            self.sprite = Some(sprite);
        }
    }

    /// Replace the sprite with a new random image, preserving position and
    /// heading. Position is clamped so a larger image stays on screen.
    fn swap_image(&mut self, w: usize, h: usize) {
        let mut rng = rand::rng();
        if let Some(sprite) = self.pick_sprite(w, h, &mut rng) {
            let maxx = w.saturating_sub(sprite.w) as i32;
            let maxy = h.saturating_sub(sprite.h) as i32;
            self.x = self.x.clamp(0, maxx.max(0));
            self.y = self.y.clamp(0, maxy.max(0));
            self.sprite = Some(sprite);
        }
    }

    /// Pick and decode a random image, sized to ~1/3 of the canvas. Tries a
    /// handful in case some fail to decode (e.g. a corrupt file).
    fn pick_sprite(&self, w: usize, h: usize, rng: &mut impl Rng) -> Option<Sprite> {
        let cap = (w.min(h) / 3).max(32) as u32;
        for _ in 0..self.images.len().min(8) {
            let idx = rng.random_range(0..self.images.len());
            if let Some(sprite) = decode_sprite(&self.images[idx], cap) {
                return Some(sprite);
            }
        }
        None
    }

    /// Render the current sprite onto a black `w`x`h` canvas.
    pub fn canvas(&self, w: usize, h: usize) -> Vec<[u8; 3]> {
        let mut canvas = vec![BLACK; w * h];
        if let Some(s) = &self.sprite {
            for sy in 0..s.h {
                let ty = self.y + sy as i32;
                if ty < 0 || ty >= h as i32 {
                    continue;
                }
                for sx in 0..s.w {
                    let tx = self.x + sx as i32;
                    if tx < 0 || tx >= w as i32 {
                        continue;
                    }
                    canvas[ty as usize * w + tx as usize] = s.px[sy * s.w + sx];
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

#[cfg(test)]
mod tests {
    use super::*;

    // With no images configured, the canvas is fully black (still prevents
    // burn-in) and the right size — and stepping never panics.
    #[test]
    fn empty_bounce_is_all_black_and_inert() {
        let mut b = Bounce::new(Vec::new());
        b.step(64, 48);
        let cv = b.canvas(64, 48);
        assert_eq!(cv.len(), 64 * 48);
        assert!(cv.iter().all(|&p| p == BLACK));
    }

    // A placed sprite renders inside the canvas at its position and nowhere
    // outside it, regardless of where the sprite sits.
    #[test]
    fn sprite_renders_in_bounds() {
        let mut b = Bounce::new(Vec::new());
        let red = [255u8, 0, 0];
        b.sprite = Some(Sprite {
            w: 4,
            h: 4,
            px: vec![red; 16],
        });
        b.x = 2;
        b.y = 3;
        let (w, h) = (16usize, 12usize);
        let cv = b.canvas(w, h);
        assert_eq!(cv.len(), w * h);
        let mut painted = 0;
        for y in 0..h {
            for x in 0..w {
                if cv[y * w + x] == red {
                    assert!((2..6).contains(&x) && (3..7).contains(&y), "({x},{y}) oob");
                    painted += 1;
                }
            }
        }
        assert_eq!(painted, 16, "the whole 4x4 sprite should be drawn");
    }

    // A sprite partly past the right/bottom edge clips instead of panicking.
    #[test]
    fn sprite_clips_at_edges() {
        let mut b = Bounce::new(Vec::new());
        let red = [255u8, 0, 0];
        b.sprite = Some(Sprite {
            w: 4,
            h: 4,
            px: vec![red; 16],
        });
        b.x = 14; // 14..18 on a width-16 canvas → 2 columns visible
        b.y = 10; // 10..14 on a height-12 canvas → 2 rows visible
        let (w, h) = (16usize, 12usize);
        let cv = b.canvas(w, h);
        let painted = cv.iter().filter(|&&p| p == red).count();
        assert_eq!(painted, 4, "only the 2x2 overlapping corner is visible");
    }
}
