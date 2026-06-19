# AmberDAV on muOS

Two ways to run AmberDAV on muOS:

- **`.muxapp` (recommended)** — a one-file install via Archive Manager. Files in
  this folder (`mux_launch.sh` + `glyph/amberdav.png`) are packaged into
  `AmberDAV-<version>.muxapp` by `build-muxapp.sh` and by the release CI.
- **Ports** — drop a launcher and binary under `ROMS/Ports/` (the original
  method, below).

Both wrap the **`amber-dav-aarch64-linux-sdl`** binary, remount the rootfs
read-write, and serve the whole OS filesystem (`/`) on port `8080`. On-device
controls: **Menu** = quit, **A** = blank screen, **X** = bounce screensaver.

> **Why the SDL build, not `-fb`?** muOS is an SDL-native platform — it keeps
> its own UI drawing to `/dev/fb0`, so the raw-framebuffer build's connection
> screen redraws slowly (muOS overwrites each frame; it only re-asserts every
> few seconds). The SDL build renders through muOS's SDL stack, which composites
> instantly. It dynamically links the system `libSDL2`, which muOS ships.

## Install via `.muxapp` (Archive Manager)

A `.muxapp` is just a ZIP whose single top-level `AmberDAV/` folder holds
`mux_launch.sh`, the binary, and the menu glyph:

```
AmberDAV.muxapp                  (zip)
└── AmberDAV/
    ├── mux_launch.sh            # entry point muOS runs
    ├── amber-dav                # aarch64 -sdl binary (dynamic; muOS ships libSDL2)
    └── glyph/amberdav.png       # 24×24 menu glyph (muOS reads it here; name matches `# ICON:`)
```

> The glyph must live in `glyph/amberdav.png` — that is the path muOS reads an
> app's list icon from, and the name matches the `# ICON: amberdav` header in
> `mux_launch.sh`. Apps installed on the SD card (SD2, `/mnt/sdcard`) need muOS
> **2508.3+** for the icon to render (a frontend fix for SD2 glyph paths).

1. Copy `AmberDAV-<version>.muxapp` to `/mnt/mmc/ARCHIVE` on the device.
2. **Applications → Archive Manager**, select it — muOS extracts AmberDAV to
   `<storage>/MUOS/application/AmberDAV/`.
3. Launch **AmberDAV** from the **Applications** menu.

`mux_launch.sh` sets up muOS's SDL environment (controller map, scaler,
rotation), remounts rootfs rw, serves `/`, logs the IP/password/QR to `log.txt`
next to the binary, and restores the framebuffer mode to muOS on exit. The glyph
needs no launcher action — muOS reads it straight from `glyph/amberdav.png`.

### Building the `.muxapp` yourself

Easiest is to download `AmberDAV-<version>.muxapp` from the GitHub release — the
SDL build is `aarch64-gnu` and doesn't cross-compile cleanly off an aarch64
Linux host. Otherwise package a prebuilt binary with the local packager (it
mirrors the release CI `muxapp` job):

```sh
device/muos/build-muxapp.sh dist/amber-dav-aarch64-linux-sdl  # package an already-built binary
device/muos/build-muxapp.sh                                   # build from source (needs aarch64 Linux + libsdl2-dev)
# -> dist/AmberDAV.muxapp   (copy that one file to the device's ARCHIVE)
```

## Install via Ports

The original method, before `.muxapp` packaging: muOS lists `*.sh` launchers
under `ROMS/Ports/`. Lay out:

```
/mnt/sdcard/ROMS/Ports/AmberDAV.sh         <- launcher (menu entry)
/mnt/sdcard/ROMS/Ports/AmberDAV/amber-dav  <- the aarch64 binary (-sdl recommended; see above)
/mnt/sdcard/ROMS/Ports/AmberDAV/log.txt    <- created on launch (IP, password, QR)
```

`AmberDAV.sh` — needed to serve the **entire** OS filesystem (it remounts rootfs
read-write and shares `/`):

```sh
#!/bin/bash
#

. /mnt/mod/ctrl/configs/functions &>/dev/null 2>&1
export HOME=/root

progdir="$(cd "$(dirname "$0")" || exit; pwd)"
appdir="$progdir/AmberDAV"
bin="$appdir/amber-dav"
log_file="$appdir/log.txt"

# muOS hides rootfs (mmcblk0p5) unlock it to serve / with editing OS files.
mount -o remount,rw /

share_root="/"
port=8080

chmod +x "$bin" 2>/dev/null

"$bin" "$share_root" "$port" > "$log_file" 2>&1

sync
```

Launch **AmberDAV** from the Ports menu. The `.muxapp` route is just this same
idea repackaged for Archive Manager, with the muOS `func.sh`/`FB_SWITCH`
framebuffer-restore conventions added — see `mux_launch.sh`.
