//! Gamepad/button input. On device builds we read `/dev/input/event*` via
//! evdev and publish every event to a broadcast channel for the live web view.
//! Without either the `fb` or `sdl` feature (desktop/server builds, dev
//! machines) this compiles to a no-op stub.
//!
//! Buttons/keys arrive as `EV_KEY`; the d-pad and analog sticks arrive as
//! `EV_ABS` absolute axes (e.g. ABS_HAT0X for the d-pad, ABS_X/Y for sticks),
//! so both kinds are forwarded.

use tokio::sync::broadcast;

/// A single input change, serialized to the browser as JSON.
#[derive(Clone, Debug, serde::Serialize)]
pub struct InputUpdate {
    /// Source device name, e.g. "Anbernic Gamepad".
    pub device: String,
    /// "button" (EV_KEY) or "axis" (EV_ABS).
    pub kind: &'static str,
    /// Readable name from evdev, e.g. "BTN_SOUTH", "ABS_HAT0X", "ABS_X".
    pub name: String,
    /// Raw evdev code, useful for mapping work.
    pub code: u16,
    /// Raw value. Buttons: 0/1/2. Axes: hat -1/0/1, sticks the analog value.
    pub value: i32,
    /// For buttons: "down"/"up"/"repeat". For axes: "".
    pub state: &'static str,
}

/// evdev key code that quits the app. Default 354 = KEY_GOTO, the Anbernic
/// menu/function button. Override with AMBERDAV_EXIT_KEY.
#[cfg(any(feature = "fb", feature = "sdl"))]
fn exit_key() -> u16 {
    std::env::var("AMBERDAV_EXIT_KEY")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(354)
}

#[cfg(any(feature = "fb", feature = "sdl"))]
pub fn spawn(
    tx: broadcast::Sender<InputUpdate>,
    mode: crate::screen::ModeHandle,
    bounce_enabled: bool,
) {
    use evdev::EventSummary;

    let exit_code = exit_key();

    for (path, dev) in evdev::enumerate() {
        // Keep devices that report buttons and/or absolute axes (gamepads).
        if dev.supported_keys().is_none() && dev.supported_absolute_axes().is_none() {
            continue;
        }

        let name = dev.name().unwrap_or("unknown").to_string();
        let path = path.to_string_lossy().into_owned();
        let tx = tx.clone();
        let mode = mode.clone();

        let mut stream = match dev.into_event_stream() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("input: cannot open {name} ({path}): {e}");
                continue;
            }
        };

        eprintln!("input: streaming from {name} ({path})");
        tokio::spawn(async move {
            loop {
                let ev = match stream.next_event().await {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!("input: {name} ({path}) read error: {e}");
                        break;
                    }
                };

                let code = ev.code();
                let update = match ev.destructure() {
                    EventSummary::Key(_, key, value) => {
                        // Menu button → quit, returning to the OS app menu.
                        if code == exit_code && value == 1 {
                            eprintln!("input: exit key ({code}) pressed; shutting down");
                            std::process::exit(0);
                        }
                        // Face buttons drive the screen on press (value == 1):
                        // A blanks it, X toggles the bounce screensaver.
                        if value == 1 {
                            use crate::screen::{self, Mode};
                            match code {
                                screen::BTN_SOUTH => screen::toggle(&mode, Mode::Black),
                                screen::BTN_NORTH if bounce_enabled => {
                                    screen::toggle(&mode, Mode::Bounce)
                                }
                                _ => {}
                            }
                        }
                        let state = match value {
                            0 => "up",
                            1 => "down",
                            _ => "repeat",
                        };
                        InputUpdate {
                            device: name.clone(),
                            kind: "button",
                            name: format!("{key:?}"),
                            code,
                            value,
                            state,
                        }
                    }
                    EventSummary::AbsoluteAxis(_, axis, value) => InputUpdate {
                        device: name.clone(),
                        kind: "axis",
                        name: format!("{axis:?}"),
                        code,
                        value,
                        state: "",
                    },
                    // Ignore sync/misc/etc.
                    _ => continue,
                };

                // Send errors only mean "no subscribers yet"; ignore them.
                let _ = tx.send(update);
            }
        });
    }
}

#[cfg(not(any(feature = "fb", feature = "sdl")))]
pub fn spawn(
    _tx: broadcast::Sender<InputUpdate>,
    _mode: crate::screen::ModeHandle,
    _bounce_enabled: bool,
) {
    eprintln!("input: gamepad support is a handheld-only feature; live input view disabled");
}
