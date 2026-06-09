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

/// The evdev key codes that drive the on-device screen, resolved from
/// config/env/CLI. Each control is a *set* of codes so a button can differ per
/// device (e.g. the Anbernic menu key vs. the Steam Deck ☰ button both quit).
// Fields are read only by the device (`fb`/`sdl`) `spawn`; on headless builds
// the struct is still constructed at the call site, so allow the dead fields.
#[allow(dead_code)]
pub struct InputKeys {
    /// Any of these quits the app.
    pub exit: Vec<u16>,
    /// Any of these blanks the screen (toggles `Mode::Black`).
    pub blank: Vec<u16>,
    /// Any of these toggles the bounce screensaver (`Mode::Bounce`).
    pub bounce: Vec<u16>,
    /// Whether the bounce screensaver may be toggled at all.
    pub bounce_enabled: bool,
}

#[cfg(any(feature = "fb", feature = "sdl"))]
pub fn spawn(tx: broadcast::Sender<InputUpdate>, mode: crate::screen::ModeHandle, keys: InputKeys) {
    use evdev::EventSummary;

    for (path, dev) in evdev::enumerate() {
        // Keep devices that report buttons and/or absolute axes (gamepads).
        if dev.supported_keys().is_none() && dev.supported_absolute_axes().is_none() {
            continue;
        }

        let name = dev.name().unwrap_or("unknown").to_string();
        let path = path.to_string_lossy().into_owned();
        let tx = tx.clone();
        let mode = mode.clone();
        // One reader task per device; each needs its own copy of the key sets.
        let exit = keys.exit.clone();
        let blank = keys.blank.clone();
        let bounce = keys.bounce.clone();
        let bounce_enabled = keys.bounce_enabled;

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
                        // Act on press (value == 1). A configured exit key quits
                        // (returning to the OS app menu); the blank/bounce keys
                        // toggle their screen modes. A code may appear in only one
                        // set; exit is checked first, then blank, then bounce.
                        if value == 1 {
                            use crate::screen::{self, Mode};
                            if exit.contains(&code) {
                                eprintln!("input: exit key ({code}) pressed; shutting down");
                                std::process::exit(0);
                            } else if blank.contains(&code) {
                                screen::toggle(&mode, Mode::Black);
                            } else if bounce_enabled && bounce.contains(&code) {
                                screen::toggle(&mode, Mode::Bounce);
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
    _keys: InputKeys,
) {
    eprintln!("input: gamepad support is a device-only feature; live input view disabled");
}
