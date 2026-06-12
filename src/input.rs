//! Gamepad/button input. On device builds we read `/dev/input/event*` via
//! evdev and publish every event to a broadcast channel for the live web view.
//! Without either the `fb` or `sdl` feature (desktop/server builds), or off
//! Linux (where evdev does not exist — macOS/Windows dev machines building the
//! device features), this compiles to a no-op stub.
//!
//! Devices are discovered by capability (reports keys or absolute axes), not
//! by hard-coded paths, and `/dev/input` is re-enumerated every few seconds so
//! a controller connected *after* launch — a Bluetooth pad paired on a Steam
//! Deck, a USB pad on a desktop — is picked up too (issue #49).
//!
//! Buttons/keys arrive as `EV_KEY`; the d-pad and analog sticks arrive as
//! `EV_ABS` absolute axes (e.g. ABS_HAT0X for the d-pad, ABS_X/Y for sticks),
//! so both kinds are forwarded.

use std::collections::HashSet;

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
#[cfg_attr(not(all(target_os = "linux", device)), allow(dead_code))]
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
#[cfg_attr(not(all(target_os = "linux", device)), allow(dead_code))]
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

/// Whether a discovered `/dev/input` node should get a reader task: it must
/// look like a gamepad — it reports keys/buttons or absolute axes; this
/// capability filter is the single gate for what counts as one — and it must
/// not already have a live reader. The kernel reuses event-node numbers, so
/// "active" is tracked by path: a reader removes its path from the set when
/// its device goes away, after which a node reappearing at the same path is
/// claimed again on a later scan (issue #49). Pure so the gate is
/// host-testable (the evdev scan that calls it only compiles on the devices).
#[cfg_attr(not(all(target_os = "linux", device)), allow(dead_code))]
pub fn should_spawn_reader(
    path: &str,
    has_keys: bool,
    has_abs_axes: bool,
    active: &HashSet<String>,
) -> bool {
    (has_keys || has_abs_axes) && !active.contains(path)
}

/// How often `/dev/input` is re-enumerated for hotplugged controllers
/// (issue #49). Chosen over an inotify watch deliberately: re-enumeration
/// needs no new dependency or raw-syscall handling, reuses the exact
/// capability-based discovery the startup scan already had, and a
/// once-per-few-seconds directory scan is negligible next to the device
/// screen's repaint cadence. Latency of up to one interval before a
/// just-paired pad responds is fine for this use.
#[cfg(all(target_os = "linux", device))]
const RESCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(all(target_os = "linux", device))]
pub fn spawn(
    tx: broadcast::Sender<InputUpdate>,
    mode: crate::screen::ModeHandle,
    keys: InputKeys,
    shutdown: CancellationToken,
) {
    use std::sync::{Arc, Mutex};

    // Paths with a live reader task; readers remove themselves on exit (see
    // `should_spawn_reader` for why this is keyed by path).
    let active: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    // Paths that failed to open, so the rescan warns once per path instead of
    // every few seconds (e.g. permission-denied nodes).
    let mut warned: HashSet<String> = HashSet::new();

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(RESCAN_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // The interval's first tick fires immediately, so the boot-time
            // devices attach right away; later ticks pick up hotplugged pads.
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            scan(&tx, &mode, &keys, &shutdown, &active, &mut warned);
        }
    });
}

/// One enumeration pass: spawn a reader task for every gamepad-looking device
/// node that doesn't already have one. Re-runs cheaply — nodes with an active
/// reader are skipped via `should_spawn_reader`, so repeated scans only ever
/// add readers for newly appeared devices.
#[cfg(all(target_os = "linux", device))]
fn scan(
    tx: &broadcast::Sender<InputUpdate>,
    mode: &crate::screen::ModeHandle,
    keys: &InputKeys,
    shutdown: &CancellationToken,
    active: &std::sync::Arc<std::sync::Mutex<HashSet<String>>>,
    warned: &mut HashSet<String>,
) {
    use evdev::EventSummary;

    for (path, dev) in evdev::enumerate() {
        let path = path.to_string_lossy().into_owned();
        // Keep devices that report buttons and/or absolute axes (gamepads),
        // unless a reader already owns the path. A reused event number whose
        // old reader hasn't noticed the disconnect yet stays skipped this
        // pass and is claimed on the next one.
        let spawn_reader = active
            .lock()
            .map(|set| {
                should_spawn_reader(
                    &path,
                    dev.supported_keys().is_some(),
                    dev.supported_absolute_axes().is_some(),
                    &set,
                )
            })
            .unwrap_or(false);
        if !spawn_reader {
            continue;
        }

        let name = dev.name().unwrap_or("unknown").to_string();
        let tx = tx.clone();
        let mode = mode.clone();
        // One reader task per device; each needs its own copy of the key sets
        // and its own handle on the shutdown token.
        let keys = keys.clone();
        let shutdown = shutdown.clone();
        let active = active.clone();

        let mut stream = match dev.into_event_stream() {
            Ok(s) => {
                warned.remove(&path);
                s
            }
            Err(e) => {
                // Warn once per path: unlike the old boot-only scan, this
                // runs every few seconds and would otherwise repeat forever
                // for a node we can never open.
                if warned.insert(path.clone()) {
                    tracing::warn!("cannot open {name} ({path}): {e}");
                }
                continue;
            }
        };

        if let Ok(mut set) = active.lock() {
            set.insert(path.clone());
        }
        tracing::info!("streaming from {name} ({path})");
        tokio::spawn(async move {
            loop {
                // Stop with the app: a reader must not outlive shutdown, and
                // a disconnect (read error) releases the path below so the
                // rescan can re-attach the device if it comes back.
                let ev = tokio::select! {
                    _ = shutdown.cancelled() => break,
                    ev = stream.next_event() => match ev {
                        Ok(ev) => ev,
                        Err(e) => {
                            tracing::info!("{name} ({path}) read error ({e}); stopping this reader — a reconnect re-attaches it");
                            break;
                        }
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
                                    tracing::info!("exit key ({code}) pressed; shutting down");
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
            // Reader ended (disconnect, read error, or shutdown): release the
            // path so a future scan may claim a device reappearing there.
            if let Ok(mut set) = active.lock() {
                set.remove(&path);
            }
        });
    }
}

#[cfg(not(all(target_os = "linux", device)))]
pub fn spawn(
    _tx: broadcast::Sender<InputUpdate>,
    _mode: crate::screen::ModeHandle,
    _keys: InputKeys,
    _shutdown: CancellationToken,
) {
    tracing::info!("gamepad support is a device-only feature; live input view disabled");
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

    // The hotplug gate (issue #49): only gamepad-looking devices (keys and/or
    // absolute axes) get a reader, and only when no reader owns the path yet.
    #[test]
    fn only_unclaimed_gamepads_get_a_reader() {
        let active = HashSet::new();
        // Capability filter: keys, axes, or both qualify; neither does not.
        assert!(should_spawn_reader(
            "/dev/input/event0",
            true,
            false,
            &active
        ));
        assert!(should_spawn_reader(
            "/dev/input/event0",
            false,
            true,
            &active
        ));
        assert!(should_spawn_reader(
            "/dev/input/event0",
            true,
            true,
            &active
        ));
        assert!(!should_spawn_reader(
            "/dev/input/event0",
            false,
            false,
            &active
        ));
    }

    #[test]
    fn claimed_paths_are_skipped_even_for_gamepads() {
        let mut active = HashSet::new();
        active.insert("/dev/input/event3".to_string());
        assert!(!should_spawn_reader(
            "/dev/input/event3",
            true,
            true,
            &active
        ));
        // Other paths are unaffected by the claim.
        assert!(should_spawn_reader(
            "/dev/input/event4",
            true,
            true,
            &active
        ));
    }

    // The reconnect cycle: a reader releasing its path (what the reader task
    // does when its device disconnects) lets the same path — the kernel
    // reuses event numbers — be claimed again on the next scan.
    #[test]
    fn a_released_path_can_be_claimed_again() {
        let mut active = HashSet::new();
        active.insert("/dev/input/event3".to_string());
        assert!(!should_spawn_reader(
            "/dev/input/event3",
            true,
            true,
            &active
        ));
        active.remove("/dev/input/event3"); // reader noticed the disconnect
        assert!(should_spawn_reader(
            "/dev/input/event3",
            true,
            true,
            &active
        ));
    }
}
