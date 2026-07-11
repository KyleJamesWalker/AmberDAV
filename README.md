# AmberDAV

A tiny, single-binary file server and utility originally designed for Anbernic
handhelds, but expanded to Windows and macOS. Point a browser at the device
and you get a full file manager; mount it as a network drive over WebDAV;
and read the device's IP, password, and a scannable QR code straight off
the device's screen, or terminal.

Built and tested on the **RG35XX Pro** and **RG34XXSP** (Allwinner H700 /
aarch64, stock Anbernic OS) and the **Steam Deck** (Game Mode, via the SDL
build). The framebuffer and headless builds are a single statically-linked
binary with no runtime dependencies; the SDL on-screen build dynamically links
the system `libSDL2` (present on SteamOS and the Anbernic stock OS).

## Features

- **File-manager web UI** — a single-page app served at `http://<device-ip>:8080/`:
  list/grid views, image thumbnails, a hidden-files toggle (dotfiles only — the
  Windows hidden attribute and macOS `UF_HIDDEN` flag are not consulted), in-browser
  preview (images, video, audio, text) with arrow-key gallery navigation, an
  in-browser text editor to edit files in place (e.g. `config.json`), breadcrumbs,
  sortable columns, shift-click range select, right-click context menu (new
  folder, upload, download, cut/copy/paste, delete, rename), drag-and-drop upload
  with a progress bar, and zip download of folders or multi-selections.
- **WebDAV server** — mount the device as a network drive from any WebDAV
  client (Finder, Windows Explorer, `rclone`, etc.) at
  `http://<device-ip>:8080/dav` (also browsable directly in a web browser).
- **On-screen connection info** — the device's IP, login password, and a QR
  code (linking to the web UI) are drawn directly to the framebuffer
  (`/dev/fb0`), so a headless handheld is usable the moment it boots the app.
- **Live gamepad button viewer** — the Status tab streams every button/axis
  event (name + raw evdev code + up/down) live over SSE; handy for discovering
  button mappings.
- **Unified settings** — every setting (root, port, bind address, password,
  permission level, default folder, screensaver) is reachable three ways with a
  clear precedence: **CLI flags → `AMBERDAV_*` env vars → config file →
  defaults**. The web UI displays settings read-only; it never edits the file.

## Screenshots

| Login | File manager | Context menu |
|:---:|:---:|:---:|
| ![Login page](assets/login.png) | ![File manager](assets/file-manager.png) | ![Right-click context menu](assets/context-menu.png) |

## Build

Cross-compiled from any host (macOS/Linux/Windows) to a static aarch64 musl
binary using [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild)
(Zig is the cross-linker; the whole dependency tree is pure Rust).

```sh
# One-time setup
rustup target add aarch64-unknown-linux-musl
cargo install cargo-zigbuild
# Zig must be installed and on PATH — e.g. `brew install zig`,
# `pip install ziglang`, or from https://ziglang.org/download/

# Device build — `--features fb` pulls in the on-device screen + gamepad UI.
cargo zigbuild --release --target aarch64-unknown-linux-musl --features fb
```

The **`fb`** and **`sdl`** features each gate the device-only code (gamepad
input, DVD-bounce screensaver, the on-screen connection canvas) and its
dependencies; they differ only in the display sink — `fb` is the static
framebuffer/Wayland sink, `sdl` links the system libSDL2. Both are **opt-in**:
with neither you get a smaller, headless WebDAV/file-server binary suited to
desktops and servers. Build a headless binary for any target by simply omitting
the features, e.g. `cargo build --release` or
`cargo zigbuild --release --target x86_64-unknown-linux-musl`.

Output: `target/aarch64-unknown-linux-musl/release/amber-dav` (~2 MB, static):

```
$ file target/aarch64-unknown-linux-musl/release/amber-dav
ELF 64-bit LSB executable, ARM aarch64, statically linked, stripped
```

The same aarch64 binary runs on both the RG35XX Pro and the RG34XXSP.

### Prebuilt binaries

Each [GitHub Release](../../releases) ships a prebuilt binary per platform:

Assets follow `amber-dav-<arch>-<os>[-fb|-sdl]`: plain `<arch>-<os>` is the
standard headless build, the `-fb` suffix marks the static build with the
on-device framebuffer/Wayland UI compiled in, and `-sdl` is the dynamic
on-screen build (links the system libSDL2).

<!-- MATRIX SYNC (issue #51): this table mirrors the build matrices in
     .github/workflows/ci.yml + release.yml (kept identical to each other by
     CI's matrix-sync job) and src/update.rs::asset_for — change one, change
     all four. -->
| Asset | Platform | Build |
| --- | --- | --- |
| `amber-dav-aarch64-linux` | ARM Linux servers/Raspberry Pi/NAS/Graviton (static musl) | headless |
| `amber-dav-aarch64-linux-fb` | the Anbernic device (static musl) | fb |
| `amber-dav-aarch64-linux-sdl` | Anbernic (SDL/`mali`) | sdl (dynamic, needs libSDL2) |
| `amber-dav-x86_64-linux` | x86 Linux servers/NAS/Docker (static musl) | headless |
| `amber-dav-x86_64-linux-fb` | x86 Linux handhelds without libSDL2 | fb |
| `amber-dav-x86_64-linux-sdl` | Steam Deck | sdl (dynamic, needs libSDL2) |
| `amber-dav-aarch64-macos` | macOS, Apple Silicon | headless |
| `amber-dav-x86_64-macos` | macOS, Intel | headless |
| `amber-dav-aarch64-windows` | Windows on ARM (Snapdragon/Copilot+ PCs) | headless |
| `amber-dav-x86_64-windows` | Windows | headless |

The `-fb` and `-sdl` assets are built with `--features fb` and `--features sdl`
respectively: both include the on-device screen and gamepad viewer (differing
only in the display sink). Every other asset is a
**headless** WebDAV/file-server CLI that writes to stdout — a quick way to host
a folder from a desktop or server:

```sh
amber-dav /path/to/folder 8080          # positional root + port
amber-dav --root /path/to/folder --port 8080 --bind 127.0.0.1
amber-dav --help                        # full flag/env list
```

### Running in Docker

A multi-platform headless Docker image (supporting both `amd64` and `arm64`) is automatically built and published on every release to the GitHub Container Registry.

```sh
# Run the container, mounting the local directory to serve at /data
docker run -d \
  --name amber-dav \
  -p 8080:8080 \
  -v /path/to/local/folder:/data \
  ghcr.io/kylejameswalker/amberdav:latest
```

The container runs as a non-root user (`amberdav`) and exposes port `8080`. You can configure the container using environment variables or CLI arguments:

```sh
docker run -d \
  --name amber-dav \
  -p 8080:8080 \
  -v /path/to/local/folder:/data \
  -e AMBERDAV_PERMISSION=read_only \
  -e AMBERDAV_PASSWORD_HASH='$argon2id$v=19$m=19456,t=2,p=1$...' \
  ghcr.io/kylejameswalker/amberdav:latest
```

## Install on Anbernic device

The stock OS lists `*.sh` launchers from `Roms/APPS/` on the SD card. Copy the
ready-made launcher + binary layout from [`device/anbernic/`](device/anbernic)
and launch **AmberDAV** from the Apps menu — it serves the whole SD card on port
`8080`.

→ **[`device/anbernic/README.md`](device/anbernic/README.md)** for the SD-card
layout, where to drop the binary, and on-device controls.

> The binary takes optional `[ROOT] [PORT]` positional arguments (defaults:
> current dir, `8080`). CLI flags and `AMBERDAV_*` env vars take precedence over
> `config.json`, so a launcher can override the file without editing it.

## Install on muOS

muOS installs from a single `AmberDAV-<version>.muxapp` (a ZIP its Archive
Manager extracts) — no separate binary to copy. Drop it into `/mnt/mmc/ARCHIVE`,
install via **Applications → Archive Manager**, then launch **AmberDAV** from the
**Applications** menu. It serves the whole OS filesystem (`/`) on port `8080`.

→ **[`device/muos/README.md`](device/muos/README.md)** for the `.muxapp` install,
the alternate Ports method, and how to build a `.muxapp` from a dev build.

## Install on Steam Deck

### Steam Deck — on-screen QR via SDL

Use the **`amber-dav-x86_64-linux-sdl`** asset — add it as a Non-Steam Game and
launch it.

The SDL build links the system `libSDL2` (present on SteamOS) and auto-selects
the video driver (`x11` on the Deck). Force one with `SDL_VIDEODRIVER` if needed.

1. In Desktop Mode, copy the binary somewhere persistent (e.g. `~/Applications`).
2. Add it to Steam as a **Non-Steam Game**, then switch to Game Mode and launch it.
3. The connection screen appears; scan the QR or read the IP/password.
4. Quit with the configured exit button (the **☰ Menu** button by default; see
   `exit_keys`) or by closing the window from the Steam overlay.

Optionally point `--connection-file` at a path a launcher script or a Decky
plugin can read (e.g. `--connection-file ~/.local/share/amber-dav/connection.json`).

> A Decky Loader plugin would live in its own repo and bundle this same binary;
> amber-dav needs no Decky-specific code.

### Anbernic — SDL on-screen QR (optional)

The **`amber-dav-aarch64-linux-sdl`** asset renders the same screen via SDL's
`mali` vendor driver (what the stock emulators use), as an alternative to the
static framebuffer build. Just launch it from an APPS-menu `*.sh` script like
any other binary — the SDL driver is auto-selected (`mali` on the Anbernic, once
`x11` is found unavailable), so no `SDL_VIDEODRIVER` is needed. Set
`SDL_VIDEODRIVER=mali` only to force it and skip the auto-detection.

## First launch

On startup the device screen and `log.txt` show the connection details:

```
  amber-dav
  serving:  /mnt/sdcard
  status:   http://192.168.1.42:8080/
  webdav:   http://192.168.1.42:8080/dav
  password: 9vqcm4xt   (user: anything)
  <QR code to the status page>
```

- **Web UI:** open `http://<device-ip>:8080/` (or scan the QR), log in with the
  password shown on the device.
- **WebDAV:** point a client at `http://<device-ip>:8080/dav`. The username is
  ignored; the password is the one on screen.

By default the password is a fresh random 8-character code each launch. Set a
fixed `password` in `config.json` (below) so you don't have to re-read it every
time.

### WebDAV client notes

- **Windows Explorer / "Map network drive" does not work out of the box.** The
  built-in WebClient service refuses Basic auth over plain HTTP (amber-dav has
  no TLS) and caps downloads at ~50 MB. To use it anyway, set two values under
  `HKLM\SYSTEM\CurrentControlSet\Services\WebClient\Parameters`:
  - `BasicAuthLevel` (DWORD) = `2` — allow Basic auth over HTTP
  - `FileSizeLimitInBytes` (DWORD) — default `50000000` (~50 MB); raise it up
    to `4294967295` (~4 GB)

  then restart the service (`net stop webclient && net start webclient`, as
  administrator). The simpler path on Windows is a real WebDAV client —
  [rclone](https://rclone.org/webdav/), WinSCP, or Cyberduck — all of which
  connect without registry changes.
- **macOS Finder** (Go ▸ Connect to Server, `http://<device-ip>:8080/dav`)
  works, but Finder issues a flood of PROPFIND requests and can be slow on
  large folders. `rclone` or Cyberduck are faster.
- **Locks are advisory.** The server answers `LOCK`/`UNLOCK` (some clients,
  e.g. the Windows mini-redirector and Office, insist on them) but does not
  enforce them — fine for a single-user LAN tool, just don't expect two
  writers to be protected from each other.

## Configuration

Every setting can be supplied three ways. When a setting is given in more than
one place, the highest layer wins:

1. **CLI flags** — `--root`, `--port`, … (run `amber-dav --help` for the list)
2. **`AMBERDAV_*` env vars** — for containers/deployment
3. **`config.json`** — persisted preferences
4. **compiled-in defaults**

| Setting | CLI flag | Env var | Config key |
|---|---|---|---|
| Served root | `--root <path>` (or positional 1) | `AMBERDAV_ROOT` | `root` |
| Port | `--port <n>` (or positional 2) | `AMBERDAV_PORT` (or `PORT`) | `port` |
| Bind address | `--bind <addr>` | `AMBERDAV_BIND` | `bind` |
| Password | `--password <pw>` | `AMBERDAV_PASSWORD` | `password` |
| Show password | `--display-password` / `--no-display-password` | `AMBERDAV_DISPLAY_PASSWORD` | `display_password` |
| Default folder | `--default-folder <path>` | `AMBERDAV_DEFAULT_FOLDER` | `default_folder` |
| Sidebar favorites | — | — | `favorites` |
| Permission | `--permission <level>` | `AMBERDAV_PERMISSION` | `permission` |
| Screensaver on | `--bounce-screen` / `--no-bounce-screen` | `AMBERDAV_BOUNCE_SCREEN` | `bounce_screen.enabled` |
| Screensaver folders | `--bounce-folders <a,b,…>` | `AMBERDAV_BOUNCE_FOLDERS` | `bounce_screen.folders` |
| Connection file | `--connection-file <path>` | `AMBERDAV_CONNECTION_FILE` | `connection_file` |
| Exit key codes | `--exit-keys <a,b,…>` | `AMBERDAV_EXIT_KEYS` | `exit_keys` |
| Blank-screen key codes | `--blank-keys <a,b,…>` | `AMBERDAV_BLANK_KEYS` | `blank_keys` |
| Bounce-toggle key codes | `--bounce-keys <a,b,…>` | `AMBERDAV_BOUNCE_KEYS` | `bounce_keys` |
| Display sink | — | `AMBERDAV_DISPLAY` | — |
| Screen rotation (fb builds) | — | `AMBERDAV_FB_ROTATE` | — |
| Log level | `--verbose` (debug) | `AMBERDAV_LOG` (or `RUST_LOG`): `off`/`error`/`warn`/`info`/`debug`/`trace` | — |

### Config file location

| Build | Location |
|---|---|
| device (`fb`/`sdl`) | next to the binary — `config.json` |
| macOS | `~/Library/Application Support/amber-dav/config.json` |
| Windows | `%APPDATA%\amber-dav\config\config.json` |
| Linux (headless) | `$XDG_CONFIG_HOME/amber-dav/config.json` → `~/.config/amber-dav/config.json` |

`$AMBERDAV_CONFIG` overrides the location on any build.

On **device** (`fb`/`sdl`) builds a default `config.json` is written next to the
binary on first launch (the device is configured through the web UI). The
generated file documents every option and its allowed values in `//` comments,
so it can be edited in place without consulting this README. **Headless** builds
never write a config implicitly — run with `--save` to write the fully-resolved
settings (CLI + env merged onto any existing file) to the config path and exit:

```sh
amber-dav --root /mnt/media --password secret --permission read_only --save
# → writes the config file with those values, then exits
```

> **Migrating from an older desktop build:** the config no longer lives next to
> the binary on desktop/server installs — it moved to the platform location
> above. Move your existing `config.json` there (or point `$AMBERDAV_CONFIG` at
> it, or re-create it with `--save`); otherwise it is ignored and defaults apply.

```jsonc
{
  // Fixed login password. Omit or leave empty for a fresh random code each boot.
  "password": "littleSecr3t",

  // Show the password on the device screen. Forced on for a random password
  // (otherwise it could never be discovered). Set false to hide a fixed one.
  "display_password": true,

  // Absolute path to serve. Omit/empty to use the CLI argument / default (".").
  "root": null,

  // Port to listen on. Omit/null to use the CLI argument / default (8080).
  "port": null,

  // Address to bind. Omit/null for 0.0.0.0 (all interfaces); "127.0.0.1" for
  // tunneled/proxied deployments.
  "bind": null,

  // Folder (relative to root) to open right after login. "" = root.
  "default_folder": "Roms",

  // Named folder shortcuts shown in the web UI sidebar, in order. Each `path`
  // is relative to the served root (same as default_folder; "" = root). Omit
  // or leave empty for no Favorites section. Handy on device builds for
  // jumping between a few frequently-used folders.
  "favorites": [
    { "name": "Game Boy", "path": "Roms/GB" },
    { "name": "Screenshots", "path": "Roms/Imgs" }
  ],

  // Allowed operations: "read_only" | "read_write" | "read_write_delete".
  "permission": "read_write_delete",

  // Burn-in screensaver (see Controls). Toggled on-device with the X button.
  "bounce_screen": {
    "enabled": true,
    // Files or folders to draw images from. Folders are scanned recursively.
    // Relative entries resolve against the served root; absolute paths work too.
    "folders": ["Roms/GBA/Imgs", "Roms/SNES/Imgs"]
  }
}
```

Edit the file directly or over WebDAV, then relaunch the app to apply changes.
The file is parsed as **JSONC**: `//` and `/* */` comments and trailing commas
are all accepted, so the example above works as written. If the file still
fails to parse, the app boots with defaults and shows the parse error on the
device screen and the web **Status** tab (and stderr), so a broken config is
never silently ignored.

**Permission levels** are enforced on both the JSON API and the WebDAV mount:

| Level | Read / browse | Create / upload / rename / move | Delete |
|-------|:---:|:---:|:---:|
| `read_only` | ✅ | ❌ | ❌ |
| `read_write` | ✅ | ✅ | ❌ |
| `read_write_delete` | ✅ | ✅ | ✅ |

`permission`, `default_folder`, and `favorites` take effect per request. `password`,
`display_password`, `root`, `port`, `bind`, `bounce_screen`, and the `*_keys`
lists are bound at boot — relaunch to apply.

## Device controls

| Button | evdev code | Action |
|--------|:---:|--------|
| **Menu** (`KEY_GOTO`, Anbernic) / **☰** (`BTN_START`, Steam Deck) | 354, 315 | Quit the app and return to the OS menu (`exit_keys`) |
| **A** (`BTN_SOUTH`) | 304 | Blank the screen (all black); press again to restore (`blank_keys`) |
| **X** (`BTN_NORTH`) | 307 | Toggle the bounce screensaver, if `bounce_screen.enabled` (`bounce_keys`) |

The **bounce screensaver** drifts a random image around a black screen,
DVD-logo style, swapping images as it ricochets off the edges — preventing
burn-in on OLED/AMOLED panels during long idle periods. It draws from the
images in `bounce_screen.folders` (PNG, JPEG, GIF, BMP, WebP). With no images
configured it simply blanks to black, which still protects the panel.

The screensaver runs on the framebuffer and SDL sinks only. The **Wayland**
sink — what the `fb` build uses under Gamescope, i.e. Steam Deck **Game
Mode** — does not support it: pressing X shows the info screen instead. On an
OLED Deck, use the `-sdl` build if you want the burn-in saver in Game Mode,
or blank the screen with A.

Each control is a **list** of evdev codes, so a button can differ per device
(the defaults above already cover the Anbernic and the Steam Deck). Retarget
them from the config file (`exit_keys`/`blank_keys`/`bounce_keys`), the
`AMBERDAV_EXIT_KEYS`/`AMBERDAV_BLANK_KEYS`/`AMBERDAV_BOUNCE_KEYS` env vars
(comma-separated), or `--exit-keys`/`--blank-keys`/`--bounce-keys`. Find a
button's code in the web UI Status tab's live input view.

## Updating via the web UI

The **Settings tab** has a Software Update card. Click **Check for update** and
AmberDAV queries the GitHub Releases API to compare the running version against
the latest release. If a newer build is available, click **Apply update** to
download and install it in place:

1. The matching platform binary is downloaded from GitHub Releases.
2. The current binary is renamed to `amber-dav.old` (in the same directory),
   replacing any previous `.old` file.
3. The new binary is moved into place.
4. Relaunch the app to run the new version — the process is **not** restarted
   automatically.

If anything goes wrong during the rename step, the original binary is restored.

> The update check and apply target the matching prebuilt release asset (see the
> table above). A device binary only ever pulls its own asset — an `-fb` build
> stays `-fb`, an `-sdl` build stays `-sdl`, and a headless binary only the
> headless one — so a device never self-updates across build shapes. Custom
> builds with no matching asset see a "no asset for this platform" response.

## Updating via WebDAV (local builds)

You can replace the binary without pulling the SD card. Because you can't write
over a running executable (`ETXTBSY`), upload to a temp name and `MOVE` it into
place (a rename over a running binary is allowed):

```sh
BIN=target/aarch64-unknown-linux-musl/release/amber-dav
HOST=http://192.168.1.42:8080
PASS=littleSecr3t

# Upload alongside the running binary, then atomically swap it in.
curl -u x:$PASS -T "$BIN" "$HOST/dav/Roms/APPS/AmberDAV/amber-dav.new"
curl -u x:$PASS -X MOVE \
  -H "Destination: $HOST/dav/Roms/APPS/AmberDAV/amber-dav" \
  "$HOST/dav/Roms/APPS/AmberDAV/amber-dav.new"
```

Quit (Menu button) and relaunch from the Apps menu to run the new build. This
requires `permission` to be `read_write` or `read_write_delete`.

## Troubleshooting

- **Screen looks rotated / sideways.** The on-screen info is authored
  landscape, and portrait-mounted panels (e.g. the RG34XXSP) are auto-rotated
  90° — the Status tab shows `rot=90(auto)` when that kicked in. If a panel
  still comes out wrong, set `AMBERDAV_FB_ROTATE` to `0`, `90`, `180`, or
  `270` to override (there's a commented line in `AmberDAV.sh`).
- **Blank screen / frozen on the loading splash.** Some Allwinner framebuffer
  drivers only present a frame on an `FBIOPAN_DISPLAY` ioctl and use multiple
  buffer pages; amber-dav handles both. Check `log.txt` and the Status tab —
  the screen line reports geometry, e.g. `ok (640x480 32bpp rot=0 pages=2
  virt=960 pan=true) mode=Info`.
- **Buttons don't register.** The input viewer reads `/dev/input/event*`. The
  stock OS runs apps as root, so this normally just works; otherwise the process
  needs read access to those device nodes.
- **Can't connect.** Confirm the phone/PC is on the same LAN/Wi-Fi as the
  handheld, and that the IP in `log.txt` matches what you're hitting.

## Security

This is a **LAN tool**. It serves plain HTTP (no TLS) and authenticates with a
short shared password — fine for a trusted home network, not for the open
internet. Don't port-forward it. Login uses a session cookie; the WebDAV mount
uses HTTP Basic auth. Path-traversal is blocked, and the configured permission
level is enforced on every mutating request.

## Project layout

| File | Purpose |
|------|---------|
| `src/main.rs` | startup, settings resolution, routing, shared state, banner + QR |
| `src/cli.rs` | CLI flags (clap) + CLI/env/file/default precedence resolution |
| `src/config.rs` | `config.json` schema + location, load/save, permission levels |
| `src/auth.rs` | session-cookie login for the web UI |
| `src/webdav.rs` | `dav-server` handler bridged into axum + Basic auth + permission gate |
| `src/files.rs` | JSON file API (list/upload/download/zip/rename/move/copy/delete) + HTTP Range |
| `src/input.rs` | evdev reader → broadcast channel; drives screen controls (device builds only) |
| `src/screen.rs` | draws IP/password/QR to `/dev/fb0`; blank + bounce screensaver (device builds only) |
| `src/ui.rs` | landing/login pages, status/info endpoint, settings (read-only), SSE stream |
| `src/update.rs` | in-app update: GitHub Releases check + binary download/rename dance |
| `src/password.rs` | per-boot password generator |
| `src/web/` | `login.html`, `app.html` (the single-page file manager) |
| `device/anbernic/` | ready-to-copy `AmberDAV.sh` launcher + SD-card layout (Anbernic stock OS) |
| `device/muos/` | `mux_launch.sh` + glyph + `build-muxapp.sh`, packaged into a `.muxapp` |

## License

MIT — see `LICENSE`.
