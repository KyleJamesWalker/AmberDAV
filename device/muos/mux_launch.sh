#!/bin/bash
# HELP: AmberDAV — WebDAV server + web file manager over your network
# ICON: amberdav
#
# mux_launch.sh — muOS entry point for AmberDAV, the file-manager web UI +
# WebDAV server + live gamepad button viewer. Connect from a browser/WebDAV
# client on the same network. The password is regenerated on every launch
# (see log.txt below) unless you set a fixed one in config.json.
#
# On-device controls: Menu = quit, A = blank screen, X = bounce screensaver.
#
# Install: drop AmberDAV.muxapp into /mnt/mmc/ARCHIVE, then
# Applications -> Archive Manager -> select it. muOS extracts this folder to
# <storage>/MUOS/application/AmberDAV/ and lists AmberDAV in Applications.
#
# The menu glyph is shipped at AmberDAV/glyph/amberdav.png (matching the
# `# ICON:` name above); muOS reads it from the app's own folder, so the
# launcher does nothing for the icon. (Apps on SD2/mnt/sdcard need muOS
# 2508.3+ for the glyph to render — fixed well before 2601 Jacaranda.)

. /opt/muos/script/var/func.sh        # GET_VAR, FB_SWITCH, SETUP_SDL_ENVIRONMENT, etc.
echo app >/tmp/act_go                 # tell muOS this is an "app" activity

# This ships the SDL build (amber-dav-aarch64-linux-sdl). muOS is an SDL-native
# platform: the SDL sink renders the connection screen through muOS's SDL stack,
# which composites instantly. The raw-framebuffer build instead writes /dev/fb0
# directly, which muOS's UI keeps overwriting, so its screen redraws lag. Pull in
# muOS's SDL environment (controller map, scaler, rotation) so input and the
# display behave like any other muOS app.
SETUP_SDL_ENVIRONMENT

# Resolve our own folder from the script's location. Do NOT derive it from
# GET_VAR storage/rom/mount: that points at the *internal* card (/mnt/mmc), but
# muOS also lists apps installed on the SD card (/mnt/sdcard). When the two
# differ the binary path is wrong, nothing runs, and no log is written. The
# script always lives in its own install dir, whichever card that is.
APP_DIR="$(cd "$(dirname "$0")" && pwd)"
bin="${APP_DIR}/amber-dav"
log_file="${APP_DIR}/log.txt"

# muOS hides rootfs (mmcblk0p5) read-only. Remount it rw so we can serve — and
# edit — the whole OS filesystem, then share / (matches the Ports launcher).
mount -o remount,rw / 2>/dev/null
share_root="/"

# Listen port (change if 8080 is taken).
port=8080

chmod +x "$bin" 2>/dev/null

# Blocks while the server runs; quitting the app from muOS stops it.
# IP, password, and a QR code are written to the log on startup.
"$bin" "$share_root" "$port" > "$log_file" 2>&1

# SDL may have changed the framebuffer mode; hand it back to muOS on exit.
SCREEN_TYPE="internal"
[ "$(GET_VAR config boot/device_mode)" = "1" ] && SCREEN_TYPE="external"
FB_SWITCH "$(GET_VAR device screen/${SCREEN_TYPE}/width)" \
          "$(GET_VAR device screen/${SCREEN_TYPE}/height)" 32

sync
