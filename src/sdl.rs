//! SDL2 display sink: shows the connection-info canvas in a fullscreen SDL
//! window. This is the on-screen path for the Steam Deck in Game Mode (where a
//! native-Wayland surface is never foregrounded by Steam) and for Anbernic
//! handhelds. The SDL video driver is auto-selected so one binary works on both
//! — `x11` (Xwayland, which Steam foregrounds) on the Deck, the `mali` vendor
//! driver on Anbernic. Compiled only with the `sdl` feature; links the system
//! libSDL2 dynamically so each device's own driver is available at runtime.
//!
//! See also — content: `canvas.rs` → choice: `display.rs` → sinks:
//! `screen.rs`/`sdl.rs`/`wayland.rs` → state: `screen::Mode`.

#[cfg(feature = "sdl")]
use crate::screen::{set_status, Mode, ModeHandle, Status};

#[cfg(feature = "sdl")]
use sdl2::event::Event;
#[cfg(feature = "sdl")]
use sdl2::pixels::PixelFormatEnum;
#[cfg(feature = "sdl")]
use tokio_util::sync::CancellationToken;

/// Video drivers to try, in order, when `SDL_VIDEODRIVER` isn't forced. `x11`
/// is first so the Steam Deck gets an Xwayland window (the surface Steam
/// foregrounds); `mali` covers Anbernic. `wayland` is tried before raw `kmsdrm`
/// so a Wayland compositor (without Xwayland) is used rather than fighting it
/// for the DRM device. `dummy` is never chosen (no output).
const DRIVER_PREFERENCE: &[&str] = &["x11", "mali", "wayland", "kmsdrm", "fbcon"];

/// The ordered list of SDL video drivers to attempt. A non-empty forced value
/// (e.g. `$SDL_VIDEODRIVER`) is the sole candidate; otherwise the preference
/// list. Pure so it can be unit-tested without a display.
pub fn driver_candidates(forced: Option<&str>) -> Vec<String> {
    match forced.map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => vec![d.to_string()],
        None => DRIVER_PREFERENCE.iter().map(|s| s.to_string()).collect(),
    }
}

/// Open a fullscreen SDL window and paint the connection-info canvas, trying
/// each candidate video driver until one initializes. Returns on error or once
/// `shutdown` is cancelled (the window closing cancels it too); blocks the
/// calling thread.
#[cfg(feature = "sdl")]
pub fn run(
    port: u16,
    password: Option<String>,
    status: Status,
    mode: ModeHandle,
    bounce_paths: Vec<std::path::PathBuf>,
    startup_error: Option<String>,
    shutdown: CancellationToken,
) -> Result<(), String> {
    let forced = std::env::var("SDL_VIDEODRIVER").ok();
    let candidates = driver_candidates(forced.as_deref());
    let mut last_err = String::from("no candidates");
    for driver in &candidates {
        // SDL selects the driver from this env var; set it per attempt. (Done in
        // this sink thread at startup; the brief window before the input thread
        // reads env is acceptable.)
        std::env::set_var("SDL_VIDEODRIVER", driver);
        match run_with_driver(
            port,
            password.clone(),
            &status,
            &mode,
            bounce_paths.clone(),
            startup_error.as_deref(),
            &shutdown,
        ) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = format!("{driver}: {e}");
                eprintln!("sdl: driver {driver} unavailable ({e}); trying next");
            }
        }
    }
    Err(format!(
        "no usable SDL video driver (tried {candidates:?}): {last_err}"
    ))
}

#[cfg(feature = "sdl")]
#[allow(clippy::too_many_arguments)]
fn run_with_driver(
    port: u16,
    password: Option<String>,
    status: &Status,
    mode: &ModeHandle,
    bounce_paths: Vec<std::path::PathBuf>,
    startup_error: Option<&str>,
    shutdown: &CancellationToken,
) -> Result<(), String> {
    let sdl = sdl2::init().map_err(|e| e.to_string())?;
    let video = sdl.video().map_err(|e| e.to_string())?;
    let driver = video.current_video_driver().to_string();

    let window = video
        .window("amber-dav", 1280, 800)
        .fullscreen_desktop()
        .build()
        .map_err(|e| e.to_string())?;
    let mut canvas = window
        .into_canvas()
        .present_vsync()
        .build()
        .map_err(|e| e.to_string())?;
    let (w, h) = canvas.output_size().map_err(|e| e.to_string())?;

    let creator = canvas.texture_creator();
    let mut texture = creator
        .create_texture_streaming(PixelFormatEnum::RGB24, w, h)
        .map_err(|e| e.to_string())?;

    let mut pump = sdl.event_pump().map_err(|e| e.to_string())?;
    eprintln!("sdl: using driver {driver} at {w}x{h}");

    let (wu, hu) = (w as usize, h as usize);
    let mut source = crate::render::FrameSource::new(
        port,
        password,
        startup_error.map(String::from),
        Some(crate::bounce::Bounce::new(bounce_paths)),
    );
    let mut last_mode: Option<Mode> = None;
    let mut frame: u64 = 0;
    loop {
        // Shutdown requested anywhere (window close below, the gamepad exit
        // key, Ctrl+C/SIGTERM): stop painting and let the sink thread end while
        // the server drains its connections (issue #34).
        if shutdown.is_cancelled() {
            set_status(status, "sdl: stopped (shutting down)".to_string());
            return Ok(());
        }

        for ev in pump.poll_iter() {
            if let Event::Quit { .. } = ev {
                // Window closed (e.g. Steam stopped the game) — request app
                // shutdown, mirroring the input thread's exit-key behaviour.
                // Cancelling (not exiting) lets in-flight uploads drain.
                eprintln!("sdl: quit event; shutting down");
                shutdown.cancel();
            }
        }

        let cur = mode.lock().map(|m| *m).unwrap_or(Mode::Info);
        let changed = last_mode != Some(cur);
        // Per-mode cadence (issue #39): Bounce animates — ~80ms steps on 16ms
        // ticks, so quit/shutdown polling stays snappy. Info/Black are static:
        // tick at ~10/s and re-check the content every ~3s (matching the fb
        // sink); the FrameSource cache turns those checks into no-ops unless
        // the mode, dims, or IP actually changed.
        let (tick_ms, check_every) = match cur {
            Mode::Bounce => (16u64, 5u64),
            _ => (100, 30),
        };
        let mut updated = false;
        if changed || frame.is_multiple_of(check_every) {
            let (fresh, px) = source.frame(cur, wu, hu);
            if fresh {
                // px is a contiguous &[[u8;3]] == w*h*3 bytes of RGB24.
                let bytes: &[u8] =
                    unsafe { std::slice::from_raw_parts(px.as_ptr() as *const u8, px.len() * 3) };
                texture
                    .update(None, bytes, wu * 3)
                    .map_err(|e| e.to_string())?;
                set_status(
                    status,
                    format!("ok (sdl {driver} {w}x{h}) frame={frame} mode={cur:?}"),
                );
                updated = true;
            }
            last_mode = Some(cur);
        }

        // Present only when the texture changed, plus an occasional re-present
        // so a driver that drops the front buffer (VT switch, compositor
        // restart) recovers — not a clear/copy/present at ~60fps for a screen
        // that isn't changing.
        if updated || frame.is_multiple_of(60) {
            canvas.clear();
            canvas.copy(&texture, None, None)?;
            canvas.present();
        }
        frame = frame.wrapping_add(1);
        // Floor the loop rate so we never busy-spin when a driver ignores vsync
        // (kmsdrm/fbcon often do). A static info screen doesn't need high fps.
        std::thread::sleep(std::time::Duration::from_millis(tick_ms));
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
