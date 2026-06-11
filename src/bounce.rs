//! DVD-bounce screensaver: drifts random images around the screen to prevent
//! burn-in on the always-on connection screen. Shared by every on-device sink
//! (framebuffer + SDL) — it operates on plain logical `(w, h)` pixel dimensions
//! and returns an RGB canvas, so it's independent of how those pixels are
//! presented. Compiled in with the `fb` or `sdl` feature (which pull the
//! `image` decoder); the `rand` crate is a base dependency.

use std::path::PathBuf;

use rand::seq::SliceRandom;
use rand::Rng;

const BLACK: [u8; 3] = [0, 0, 0];

/// How many distinct images one screensaver session rotates between. Each is
/// decoded exactly once — downscaled to ≤ ~1/3 of the canvas before pooling
/// (a couple hundred KB apiece), never kept at full size — so a wall bounce
/// is a pool swap, not another full-image decode (issue #40).
const POOL_SIZE: usize = 8;

/// Give up filling the pool after this many decode attempts, so a folder of
/// corrupt files can't stall a frame indefinitely.
const POOL_ATTEMPTS: usize = POOL_SIZE * 4;

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
    /// The session's pre-decoded sprites (see [`POOL_SIZE`]).
    pool: Vec<Sprite>,
    /// Index into `pool` of the sprite currently on screen.
    current: usize,
    /// The size cap the pool was decoded for; a canvas-dimension change
    /// invalidates the pool (different cap → different sprite sizes).
    pool_cap: u32,
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
            pool: Vec::new(),
            current: 0,
            pool_cap: 0,
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
        let Some(sprite) = self.pool.get(self.current) else {
            // Every decode failed earlier; try again before moving.
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
        if bounced && self.pool.len() > 1 {
            self.swap_image(w, h);
        }
    }

    /// First placement: fill the sprite pool, then center a sprite with a
    /// random diagonal heading.
    fn start(&mut self, w: usize, h: usize) {
        let mut rng = rand::rng();
        self.fill_pool(w, h, &mut rng);
        if let Some(sprite) = self.pool.get(self.current) {
            self.x = (w.saturating_sub(sprite.w) / 2) as i32;
            self.y = (h.saturating_sub(sprite.h) / 2) as i32;
            self.vx = if rng.random_bool(0.5) { 3 } else { -3 };
            self.vy = if rng.random_bool(0.5) { 2 } else { -2 };
        }
    }

    /// Decode the session's sprite pool: up to [`POOL_SIZE`] distinct random
    /// images, each downscaled to the canvas cap, decoded *once* up front.
    /// Decoding a large camera JPEG means materializing the full-size RGB
    /// first (tens of MB, hundreds of ms on an A53) — doing that on every
    /// wall bounce was a visible hitch each time the sprite hit an edge.
    /// After this, a bounce only swaps between already-decoded sprites
    /// (issue #40). No-op while a pool decoded for the same cap exists.
    fn fill_pool(&mut self, w: usize, h: usize, rng: &mut impl Rng) {
        let cap = sprite_cap(w, h);
        if self.pool_cap == cap && !self.pool.is_empty() {
            return;
        }
        self.pool.clear();
        self.pool_cap = cap;
        self.current = 0;
        // Shuffled indices = picks without replacement, so one session shows
        // distinct images; attempts are bounded so corrupt files can't stall.
        let mut order: Vec<usize> = (0..self.images.len()).collect();
        order.shuffle(rng);
        for idx in order.into_iter().take(POOL_ATTEMPTS) {
            if self.pool.len() >= POOL_SIZE {
                break;
            }
            if let Some(sprite) = decode_sprite(&self.images[idx], cap) {
                self.pool.push(sprite);
            }
        }
    }

    /// Swap to a *different* pooled sprite, preserving position and heading
    /// (position is clamped so a larger sprite stays on screen). No decode
    /// happens here — the pool was filled at activation (issue #40).
    fn swap_image(&mut self, w: usize, h: usize) {
        if self.pool.len() < 2 {
            return;
        }
        let mut rng = rand::rng();
        // Random index excluding `current`: draw from one fewer slot and
        // shift the values at/after `current` up by one.
        let mut idx = rng.random_range(0..self.pool.len() - 1);
        if idx >= self.current {
            idx += 1;
        }
        self.current = idx;
        let sprite = &self.pool[self.current];
        let maxx = w.saturating_sub(sprite.w) as i32;
        let maxy = h.saturating_sub(sprite.h) as i32;
        self.x = self.x.clamp(0, maxx.max(0));
        self.y = self.y.clamp(0, maxy.max(0));
    }

    /// Render the current sprite onto a black `w`x`h` canvas, reusing the
    /// caller's buffer — at 12.5 fps a fresh ~900 KB allocation per frame is
    /// pure churn (issue #40). The buffer is (re)sized and cleared in place;
    /// the caller keeps it alive across frames.
    pub fn render(&self, w: usize, h: usize, canvas: &mut Vec<[u8; 3]>) {
        if canvas.len() == w * h {
            canvas.fill(BLACK);
        } else {
            canvas.clear();
            canvas.resize(w * h, BLACK);
        }
        if let Some(s) = self.pool.get(self.current) {
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
    }
}

/// Largest sprite side for a `w`x`h` canvas: ~1/3 of the smaller dimension.
fn sprite_cap(w: usize, h: usize) -> u32 {
    (w.min(h) / 3).max(32) as u32
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

    const RED: [u8; 3] = [255, 0, 0];

    /// A Bounce showing one synthetic 4x4 red sprite (no disk involved).
    fn with_red_sprite() -> Bounce {
        let mut b = Bounce::new(Vec::new());
        b.pool = vec![Sprite {
            w: 4,
            h: 4,
            px: vec![RED; 16],
        }];
        b.current = 0;
        b
    }

    /// One-shot render into a fresh buffer (the sinks reuse theirs).
    fn rendered(b: &Bounce, w: usize, h: usize) -> Vec<[u8; 3]> {
        let mut buf = Vec::new();
        b.render(w, h, &mut buf);
        buf
    }

    // With no images configured, the canvas is fully black (still prevents
    // burn-in) and the right size — and stepping never panics.
    #[test]
    fn empty_bounce_is_all_black_and_inert() {
        let mut b = Bounce::new(Vec::new());
        b.step(64, 48);
        let cv = rendered(&b, 64, 48);
        assert_eq!(cv.len(), 64 * 48);
        assert!(cv.iter().all(|&p| p == BLACK));
    }

    // A placed sprite renders inside the canvas at its position and nowhere
    // outside it, regardless of where the sprite sits.
    #[test]
    fn sprite_renders_in_bounds() {
        let mut b = with_red_sprite();
        b.x = 2;
        b.y = 3;
        let (w, h) = (16usize, 12usize);
        let cv = rendered(&b, w, h);
        assert_eq!(cv.len(), w * h);
        let mut painted = 0;
        for y in 0..h {
            for x in 0..w {
                if cv[y * w + x] == RED {
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
        let mut b = with_red_sprite();
        b.x = 14; // 14..18 on a width-16 canvas → 2 columns visible
        b.y = 10; // 10..14 on a height-12 canvas → 2 rows visible
        let (w, h) = (16usize, 12usize);
        let cv = rendered(&b, w, h);
        let painted = cv.iter().filter(|&&p| p == RED).count();
        assert_eq!(painted, 4, "only the 2x2 overlapping corner is visible");
    }

    // The buffer is reused across same-size frames (no per-frame allocation)
    // and a moved sprite leaves no ghost pixels behind; a dimension change
    // resizes it correctly.
    #[test]
    fn render_reuses_the_buffer_without_ghosting() {
        let mut b = with_red_sprite();
        b.x = 2;
        b.y = 3;
        let (w, h) = (16usize, 12usize);
        let mut buf = Vec::new();
        b.render(w, h, &mut buf);
        let ptr = buf.as_ptr();

        b.x = 10;
        b.y = 5;
        b.render(w, h, &mut buf);
        assert_eq!(buf.as_ptr(), ptr, "same-size render must not reallocate");
        assert_eq!(
            buf.iter().filter(|&&p| p == RED).count(),
            16,
            "exactly one sprite on screen"
        );
        // The old position is black again — nothing of the previous frame
        // bleeds through the reused buffer.
        for y in 3..7 {
            for x in 2..6 {
                if !(10..14).contains(&x) || !(5..9).contains(&y) {
                    assert_eq!(buf[y * w + x], BLACK, "ghost pixel at ({x},{y})");
                }
            }
        }

        b.render(8, 6, &mut buf);
        assert_eq!(buf.len(), 8 * 6, "a dimension change resizes the buffer");
    }

    /// Scratch image dir that cleans itself up.
    struct TmpImages(PathBuf);
    impl TmpImages {
        fn new(name: &str, count: u32) -> TmpImages {
            let dir = std::env::temp_dir().join(format!(
                "amberdav-bounce-test-{}-{name}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..count {
                // Tiny solid-color PNGs; color varies so sprites are distinct.
                let img =
                    image::RgbImage::from_pixel(8, 8, image::Rgb([(i * 40 % 256) as u8, 64, 32]));
                img.save(dir.join(format!("img-{i}.png"))).unwrap();
            }
            TmpImages(dir)
        }
    }
    impl Drop for TmpImages {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // The whole point of the pool (issue #40): after the first activation,
    // wall bounces swap between already-decoded sprites and never read the
    // disk again. Deleting the source files mid-session proves it — any
    // re-decode attempt would come up empty and drop the sprite.
    #[test]
    fn bounces_after_activation_never_touch_the_disk() {
        let imgs = TmpImages::new("no-disk", 4);
        let mut b = Bounce::new(vec![imgs.0.clone()]);
        b.step(96, 96); // scan + fill the pool + first placement
        assert!(
            b.pool.len() >= 2,
            "expected several pooled sprites, got {}",
            b.pool.len()
        );
        let pooled = b.pool.len();

        std::fs::remove_dir_all(&imgs.0).unwrap();
        // Hundreds of steps on a small canvas → plenty of edge bounces.
        for _ in 0..500 {
            b.step(96, 96);
        }
        assert_eq!(b.pool.len(), pooled, "a bounce must not re-fill the pool");
        assert!(
            b.pool.get(b.current).is_some(),
            "the sprite must survive the source files vanishing"
        );
    }

    // The pool is bounded: a big library still decodes at most POOL_SIZE
    // sprites (the RAM budget), and they are downscaled to the canvas cap.
    #[test]
    fn pool_is_bounded_and_downscaled() {
        let imgs = TmpImages::new("bounded", (POOL_SIZE as u32) + 4);
        let mut b = Bounce::new(vec![imgs.0.clone()]);
        b.step(300, 300);
        assert_eq!(b.pool.len(), POOL_SIZE);
        let cap = sprite_cap(300, 300) as usize;
        assert!(b
            .pool
            .iter()
            .all(|s| s.w <= cap && s.h <= cap && !s.px.is_empty()));
    }

    #[test]
    fn sprite_cap_is_a_third_of_the_short_side_with_a_floor() {
        assert_eq!(sprite_cap(1280, 800), 266);
        assert_eq!(sprite_cap(480, 640), 160);
        assert_eq!(sprite_cap(60, 60), 32, "tiny canvases keep a 32px floor");
    }
}
