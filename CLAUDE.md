# CLAUDE.md

Orientation for AI agents (and new contributors) working on AmberDAV: a tiny,
single-binary WebDAV server + web file manager, originally for Anbernic
handhelds, that also runs headless on desktops/servers. Read this before
"fixing" anything — several deliberate choices look like bugs (see Gotchas).

## Build & test

Mirror CI (`.github/workflows/ci.yml`) exactly before pushing:

```sh
cargo fmt --all -- --check

# Headless (default features) — what the desktop/server release assets ship.
cargo clippy --all-targets -- -D warnings
cargo test --all-targets

# fb feature — the static framebuffer/Wayland device build.
cargo clippy --all-targets --features fb -- -D warnings
cargo test --all-targets --features fb

# sdl feature — on Linux this links the system libSDL2
# (CI: apt-get install libsdl2-dev; macOS: brew install sdl2).
cargo clippy --all-targets --features sdl -- -D warnings
cargo test --all-targets --features sdl
```

The device-only dependencies (evdev/framebuffer/smithay-client-toolkit/sdl2)
are gated to `cfg(target_os = "linux")`, and the sink modules
(`sdl.rs`/`wayland.rs`) are too — so on macOS/Windows the `fb`/`sdl` feature
runs compile only the cross-platform parts and stubs. The real device code
builds on Linux only; CI covers all three feature sets there.

Device cross-build (static aarch64 musl; Zig is the cross-linker):

```sh
rustup target add aarch64-unknown-linux-musl   # once
cargo install cargo-zigbuild                   # once; Zig must be on PATH
cargo zigbuild --release --target aarch64-unknown-linux-musl --features fb
```

## Feature matrix

| Build    | Features         | Display sink                                                     | Linking                          |
|----------|------------------|------------------------------------------------------------------|----------------------------------|
| headless | *(none, default)* | none — banner + QR on stdout only                                | static-friendly, no device deps  |
| fb       | `--features fb`  | Wayland (Gamescope) or `/dev/fb0`, chosen at runtime by `display::detect()` | fully static (musl)              |
| sdl      | `--features sdl` | fullscreen SDL2 window (driver auto-selected)                    | dynamic — needs system libSDL2   |

- If both `fb` and `sdl` are enabled, **`sdl` wins** — in the sink selection
  and in the self-update asset mapping (`update::asset_for`).
- `fb`/`sdl` only take real effect on **Linux** (the handheld targets); on
  other hosts the flags compile stubs.
- Release assets follow `amber-dav-<arch>-<os>[-fb|-sdl]`; a binary only ever
  self-updates to its own shape.
- The bounce screensaver runs on the framebuffer and SDL sinks only; the
  **Wayland** sink (the fb build under Gamescope / Steam Deck Game Mode) has
  no bounce support and falls back to the info screen
  (`render::effective_mode`).

## Module map

| File | Purpose |
|------|---------|
| `src/main.rs` | thin startup wiring: resolve settings → build `AppState` → `router()` → serve; banner + QR |
| `src/router.rs` | **every HTTP route in one block** (extracted from `main()`, issue #32) + end-to-end integration tests; the routes/auth table lives on `router()` |
| `src/state.rs` | `AppState`/`ServerInfo`/`SharedSettings` shared by all handlers; `current_ip()`; the `MountTable` (named mounts, issue #76) and the shared textual path-safety (`plain_segment`/`resolve_segments`) |
| `src/cli.rs` | CLI flags (clap) + CLI > env > file > default resolution (`Cli::resolve`, testable via `resolve_with`) |
| `src/config.rs` | JSONC `config.json` schema + platform location, load/save, permission levels |
| `src/auth.rs` | session-cookie login for the web UI (`Session` extractor, login/logout) |
| `src/ui.rs` | **web** handlers: landing/login pages, `/api/info`, `/api/settings`, the live-input SSE stream (despite the name, this is the browser UI — "the UI" elsewhere can mean the device screen) |
| `src/files.rs` | JSON file API (list/upload/download/zip/raw/thumb/rename/move/copy/delete) + `safe_name`/`confine` (textual segment checks live in `state.rs`), HTTP Range, thumbnail disk cache |
| `src/webdav.rs` | `dav-server` bridged into axum + HTTP Basic auth + the `method_allowed` permission gate |
| `src/update.rs` | self-update: GitHub Releases check/apply, SHA256 verification, per-shape asset mapping |
| `src/password.rs` | per-boot password generator (unambiguous charset) |
| `src/connection.rs` | optional `connection.json` sidecar for external launchers / a future Decky plugin |
| `src/input.rs` | evdev gamepad reader → broadcast channel; drives the screen modes (device builds; no-op stub otherwise) |
| `src/screen.rs` | device-screen orchestration: the `Mode` state machine (Info/Black/Bounce), sink startup, the framebuffer painter |
| `src/display.rs` | runtime **sink selection** for fb builds: Wayland vs `/dev/fb0` vs headless (`AMBERDAV_DISPLAY` override) |
| `src/canvas.rs` | connection-info pixel **content** (IP/password/QR), pure and host-testable; palette hand-synced with the web UI |
| `src/render.rs` | shared frame production for the sinks (issue #39): `FrameSource` caches the static canvas (re-renders only on mode/dims/IP change); `effective_mode` is where Wayland's missing bounce support falls back to Info |
| `src/wayland.rs` | Wayland `wl_shm` sink — Steam Deck Game Mode, where Gamescope owns DRM (fb builds, Linux) |
| `src/sdl.rs` | SDL2 sink with driver auto-selection — Steam Deck + Anbernic (sdl builds, Linux) |
| `src/bounce.rs` | DVD-bounce screensaver engine, shared by the fb and sdl sinks (device builds only) |
| `src/web/` | `login.html` + the file-manager SPA, split into `app.html` (markup) + `app.css` + `app.js`, each embedded via `include_str!` and served from `/`, `/app.css`, `/app.js` |
| `device/anbernic/` | ready-to-copy `AmberDAV.sh` launcher + SD-card layout for the Anbernic stock OS |
| `device/muos/` | muOS `.muxapp` packaging: `mux_launch.sh` + `resources/amberdav.png` glyph + `build-muxapp.sh` (local packager, mirrors the release.yml `muxapp` job); serves `/` after remounting rootfs rw |

## Entry points

- **Routing:** `src/router.rs::router()` — every route in one visible block
  (NOT in `main.rs`; it was extracted in issue #32). The doc comment there is
  the per-route auth/permission table.
- **Web UI:** `src/web/app.html`, served by `ui::index` via `include_str!`
  (auth-gated; `login.html` is the public page).
- **Config precedence:** CLI flags > `AMBERDAV_*` env vars > config file >
  compiled defaults, resolved in `src/cli.rs` (`Cli::resolve`; tests inject a
  fake environment through `resolve_with`).
- **Shared state:** `src/state.rs::AppState` — what every handler extracts.

## Gotchas — deliberate choices, do not "fix"

- **`version = "0.0.0"` in Cargo.toml is intentional.** The release workflow
  stamps the real version from the git tag at build time. Don't bump it.
- **`panic = "abort"`** (with `opt-level = "z"`, LTO, strip) is deliberate:
  small static binaries for the handhelds.
- **The generated config is JSONC on purpose** — `//` comments and trailing
  commas, parsed with `jsonc-parser`. The comments are the on-device
  documentation; don't convert it to strict JSON.
- **The canvas palette is hand-synced.** The colors in `src/canvas.rs` mirror
  the `:root` CSS variables in `src/web/app.css` and `login.html`. Change one,
  change both.
- **Device-only code paths are unreachable on a dev machine.** The
  framebuffer/Wayland/SDL/evdev paths need real hardware; the decision logic is
  extracted into pure, host-testable functions instead (`display::select`,
  `display::pick_wayland_socket`, `sdl::driver_candidates`, `canvas`,
  `bounce`). Verify via the cfg'd test suites, not by trying to run a sink.
- **Permission enforcement lives in two places** that must stay in sync:
  `files.rs` per-handler `can_write`/`can_delete` checks, and
  `webdav.rs::method_allowed` for the WebDAV methods. Adding a write surface to
  one without the other silently bypasses the permission ladder.
