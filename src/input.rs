//! Gamepad/button input. On device builds we read `/dev/input/event*` via
//! evdev and publish every event to a broadcast channel for the live web view.
//! Without either the `fb` or `sdl` feature (desktop/server builds), or off
//! Linux (where evdev does not exist — macOS/Windows dev machines building the
//! device features), this compiles to a no-op stub.
//!
//! Buttons/keys arrive as `EV_KEY`; the d-pad and analog sticks arrive as
//! `EV_ABS` absolute axes (e.g. ABS_HAT0X for the d-pad, ABS_X/Y for sticks),
//! so both kinds are forwarded.

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

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
#[derive(Clone)]
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

/// What a configured key press does. A code may appear in more than one set;
/// [`key_action`] resolves the overlap (exit first, then blank, then bounce).
// Constructed by the device (`fb`/`sdl`, Linux) event loop; host builds only
// reach it through the unit tests, so allow it to be dead there.
#[cfg_attr(
    not(all(target_os = "linux", any(feature = "fb", feature = "sdl"))),
    allow(dead_code)
)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyAction {
    /// Quit the app (returning the handheld to the OS app menu).
    Exit,
    /// Toggle the blank screen (`Mode::Black`).
    Blank,
    /// Toggle the bounce screensaver (`Mode::Bounce`).
    Bounce,
}

/// Resolve a pressed key code against the configured control sets; `None`
/// means the code is not a control and the event is only forwarded. Pure so
/// the exit/blank/bounce precedence is host-testable (the evdev reader that
/// calls it only compiles on the devices).
#[cfg_attr(
    not(all(target_os = "linux", any(feature = "fb", feature = "sdl"))),
    allow(dead_code)
)]
pub fn key_action(code: u16, keys: &InputKeys) -> Option<KeyAction> {
    if keys.exit.contains(&code) {
        Some(KeyAction::Exit)
    } else if keys.blank.contains(&code) {
        Some(KeyAction::Blank)
    } else if keys.bounce_enabled && keys.bounce.contains(&code) {
        Some(KeyAction::Bounce)
    } else {
        None
    }
}

#[cfg(all(target_os = "linux", any(feature = "fb", feature = "sdl")))]
pub fn spawn(
    tx: broadcast::Sender<InputUpdate>,
    mode: crate::screen::ModeHandle,
    keys: InputKeys,
    shutdown: CancellationToken,
) {
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
        // One reader task per device; each needs its own copy of the key sets
        // and its own handle on the shutdown token.
        let keys = keys.clone();
        let shutdown = shutdown.clone();

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
                        // toggle their screen modes (precedence in `key_action`).
                        if value == 1 {
                            use crate::screen::{self, Mode};
                            match key_action(code, &keys) {
                                Some(KeyAction::Exit) => {
                                    // Cancel rather than exit: the server gets to
                                    // drain in-flight uploads/WebDAV writes before
                                    // the process ends (issue #34).
                                    eprintln!("input: exit key ({code}) pressed; shutting down");
                                    shutdown.cancel();
                                }
                                Some(KeyAction::Blank) => screen::toggle(&mode, Mode::Black),
                                Some(KeyAction::Bounce) => screen::toggle(&mode, Mode::Bounce),
                                None => {}
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

#[cfg(not(all(target_os = "linux", any(feature = "fb", feature = "sdl"))))]
pub fn spawn(
    _tx: broadcast::Sender<InputUpdate>,
    _mode: crate::screen::ModeHandle,
    _keys: InputKeys,
    _shutdown: CancellationToken,
) {
    eprintln!("input: gamepad support is a device-only feature; live input view disabled");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bounce_enabled: bool) -> InputKeys {
        InputKeys {
            exit: vec![316],
            blank: vec![304],
            bounce: vec![307],
            bounce_enabled,
        }
    }

    #[test]
    fn configured_codes_map_to_their_actions() {
        let k = keys(true);
        assert_eq!(key_action(316, &k), Some(KeyAction::Exit));
        assert_eq!(key_action(304, &k), Some(KeyAction::Blank));
        assert_eq!(key_action(307, &k), Some(KeyAction::Bounce));
    }

    #[test]
    fn unconfigured_codes_are_only_forwarded() {
        assert_eq!(key_action(999, &keys(true)), None);
    }

    // A code present in several sets must resolve deterministically: exit
    // first, then blank, then bounce — quitting can never be shadowed by a
    // screen toggle.
    #[test]
    fn exit_wins_when_a_code_is_in_multiple_sets() {
        let k = InputKeys {
            exit: vec![316],
            blank: vec![316, 304],
            bounce: vec![316, 304],
            bounce_enabled: true,
        };
        assert_eq!(key_action(316, &k), Some(KeyAction::Exit));
        assert_eq!(key_action(304, &k), Some(KeyAction::Blank));
    }

    #[test]
    fn bounce_is_inert_when_the_screensaver_is_disabled() {
        assert_eq!(key_action(307, &keys(false)), None);
    }
}
