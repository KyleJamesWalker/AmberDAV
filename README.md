# amber-dav

A tiny, single-binary file server and utility for Anbernic handhelds. Point a
browser at the device and you get a full file manager; mount it as a
network drive over WebDAV; and read the device's IP, password, and a scannable
QR code straight off the handheld's own screen — no need to know its IP first.

Built and tested on the **RG35XX Pro** and **RG34XXSP** (Allwinner H700 /
aarch64, stock Anbernic OS). The whole thing is one statically-linked binary
with no runtime dependencies — drop it on the SD card and run it.

## Features

- **File-manager web UI** — a single-page app served at `http://<device-ip>:8080/`:
  list/grid views, image thumbnails, a hidden-files (dotfiles) toggle, in-browser
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
- **File-owned settings** — behavior is configured by a `config.json` next to
  the binary (fixed password, served root, permission level, default folder,
  screensaver). The UI displays settings read-only; it never edits the file.

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

# Build
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

Output: `target/aarch64-unknown-linux-musl/release/amber-dav` (~2 MB, static):

```
$ file target/aarch64-unknown-linux-musl/release/amber-dav
ELF 64-bit LSB executable, ARM aarch64, statically linked, stripped
```

The same aarch64 binary runs on both the RG35XX Pro and the RG34XXSP.

### Prebuilt binaries

Each [GitHub Release](../../releases) ships a prebuilt binary per platform:

| Asset | Platform |
| --- | --- |
| `amber-dav-aarch64-linux` | the Anbernic device (static musl) |
| `amber-dav-aarch64-macos` | macOS, Apple Silicon |
| `amber-dav-x86_64-macos` | macOS, Intel |
| `amber-dav-x86_64-windows` | Windows |

Note: macOS/Windows binaries are headless — a quick way to host a folder as a file
server + WebDAV mount from a desktop:

```sh
amber-dav /path/to/folder 8080   # serve <folder> on http://localhost:8080/
```

## Install on the device

The stock OS launches apps from `Roms/APPS/` on the SD card: a `*.sh` script
there appears in the Apps menu, and the filename becomes the menu label. This
repo ships a ready-made launcher under [`example_APPS/`](example_APPS).

Copy this layout to the SD card (the games/second card on a two-slot device):

```
Roms/APPS/WebDAV.sh            <- launcher (this is the Apps-menu entry)
Roms/APPS/webdav/amber-dav     <- the aarch64 binary you built
Roms/APPS/webdav/config.json   <- written automatically on first launch
Roms/APPS/webdav/log.txt       <- created on launch (IP, password, QR text)
```

Then launch **WebDAV** from the device's Apps menu. `WebDAV.sh` serves the whole
SD card root (two levels up from `Roms/APPS`) on port `8080` and writes startup
output to `log.txt`. Edit the script to change the port, served root, or screen
rotation — it's commented.

> The binary itself takes optional `[ROOT_DIR] [PORT]` arguments (defaults:
> current dir, `8080` / `$PORT`), but `config.json` overrides these once set.

## First launch

On startup the device screen and `log.txt` show the connection details:

```
  amber-dav
  serving:  /mnt/sdcard
  status:   http://192.168.1.42:8080/
  webdav:   http://192.168.1.42:8080/dav
  password: 9vqcm   (user: anything)
  <QR code to the status page>
```

- **Web UI:** open `http://<device-ip>:8080/` (or scan the QR), log in with the
  password shown on the device. The password is shown **on the device only** —
  never in the browser.
- **WebDAV:** point a client at `http://<device-ip>:8080/dav`. The username is
  ignored; the password is the one on screen.

By default the password is a fresh random 5-character code each launch. Set a
fixed `password` in `config.json` (below) so you don't have to re-read it every
time.

## Configuration

Settings live in `config.json` in the same folder as the binary (override the
location with `$AMBERDAV_CONFIG`). A default file is written on first launch.
Edit it on the SD card or over WebDAV, then relaunch the app to apply changes.

```jsonc
{
  // Fixed login password. Omit or leave empty for a fresh random code each boot.
  "password": "littleSecr3t",

  // Show the password on the device screen. Forced on for a random password
  // (otherwise it could never be discovered). Set false to hide a fixed one.
  "display_password": true,

  // Absolute path to serve. Omit/empty to use the launcher's argument/default.
  "root": null,

  // Folder (relative to root) to open right after login. "" = root.
  "default_folder": "Roms",

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

**Permission levels** are enforced on both the JSON API and the WebDAV mount:

| Level | Read / browse | Create / upload / rename / move | Delete |
|-------|:---:|:---:|:---:|
| `read_only` | ✅ | ❌ | ❌ |
| `read_write` | ✅ | ✅ | ❌ |
| `read_write_delete` | ✅ | ✅ | ✅ |

`permission` and `default_folder` take effect per request. `password`,
`display_password`, `root`, and `bounce_screen` are bound at boot — relaunch to
apply.

## On-device controls

The gamepad drives the app directly, no browser needed:

| Button | evdev code | Action |
|--------|:---:|--------|
| **Menu** (`KEY_GOTO`) | 354 | Quit the app and return to the OS menu |
| **A** (`BTN_SOUTH`) | 304 | Blank the screen (all black); press again to restore |
| **X** (`BTN_NORTH`) | 307 | Toggle the bounce screensaver (if `bounce_screen.enabled`) |

The **bounce screensaver** drifts a random image around a black screen,
DVD-logo style, swapping images as it ricochets off the edges — preventing
burn-in on OLED/AMOLED panels during long idle periods. It draws from the
images in `bounce_screen.folders` (PNG, JPEG, GIF, BMP, WebP). With no images
configured it simply blanks to black, which still protects the panel.

Override the quit key with `AMBERDAV_EXIT_KEY=<code>` if your device differs.

## Updating a running device over WebDAV

You can replace the binary without pulling the SD card. Because you can't write
over a running executable (`ETXTBSY`), upload to a temp name and `MOVE` it into
place (a rename over a running binary is allowed):

```sh
BIN=target/aarch64-unknown-linux-musl/release/amber-dav
HOST=http://192.168.1.42:8080
PASS=littleSecr3t

# Upload alongside the running binary, then atomically swap it in.
curl -u x:$PASS -T "$BIN" "$HOST/dav/Roms/APPS/webdav/amber-dav.new"
curl -u x:$PASS -X MOVE \
  -H "Destination: $HOST/dav/Roms/APPS/webdav/amber-dav" \
  "$HOST/dav/Roms/APPS/webdav/amber-dav.new"
```

Quit (Menu button) and relaunch from the Apps menu to run the new build. This
requires `permission` to be `read_write` or `read_write_delete`.

## Troubleshooting

- **Screen looks rotated / sideways.** The on-screen info is authored landscape.
  Set `AMBERDAV_FB_ROTATE` to `90`, `180`, or `270` (there's a commented line in
  `WebDAV.sh`).
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
| `src/main.rs` | startup, config load, routing, shared state, banner + QR |
| `src/config.rs` | `config.json` schema, load/save, permission levels |
| `src/auth.rs` | session-cookie login for the web UI |
| `src/webdav.rs` | `dav-server` handler bridged into axum + Basic auth + permission gate |
| `src/files.rs` | JSON file API (list/upload/download/zip/rename/move/copy/delete) + HTTP Range |
| `src/input.rs` | evdev reader → broadcast channel; drives screen controls (Linux only) |
| `src/screen.rs` | draws IP/password/QR to `/dev/fb0`; blank + bounce screensaver (Linux only) |
| `src/ui.rs` | landing/login pages, status/info endpoint, settings (read-only), SSE stream |
| `src/password.rs` | per-boot password generator |
| `src/web/` | `login.html`, `app.html` (the single-page file manager) |
| `example_APPS/` | ready-to-copy `WebDAV.sh` launcher + SD-card layout |

## License

MIT — see `LICENSE`.
```
