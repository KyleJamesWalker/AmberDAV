#!/bin/bash
#
# WebDAV.sh — launches amber-dav: a file-manager web UI + WebDAV server +
# live gamepad button viewer. Connect from a browser/WebDAV client on the same
# network. By default the password is regenerated on every launch (see the log
# file below); set a fixed one in config.json to avoid re-reading it.
#
# On-device controls: Menu = quit, A = blank screen, X = bounce screensaver.
#
# Layout on the SD card (the .sh name becomes the Apps-menu label):
#   Roms/APPS/WebDAV.sh
#   Roms/APPS/webdav/amber-dav    <- the aarch64 binary
#   Roms/APPS/webdav/config.json    <- settings (written on first launch)
#   Roms/APPS/webdav/log.txt        <- created on launch (IP, password, QR)

. /mnt/mod/ctrl/configs/functions &>/dev/null
export HOME=/root

progdir="$(cd "$(dirname "$0")" || exit; pwd)"
appdir="$progdir/webdav"
bin="$appdir/amber-dav"
log_file="$appdir/log.txt"

# Serve the whole games SD card. This script lives at <card>/Roms/APPS,
# so two levels up is the card root — works whether it mounts at
# /mnt/sdcard, /mnt/mmc, etc.
root="$(cd "$progdir/../.." || exit; pwd)"

# Listen port (change if 8080 is taken).
port=8080

# Screen rotation is auto-detected: portrait-mounted panels (e.g. the
# RG34XXSP) get the landscape info screen turned 90 automatically. If a panel
# still comes out rotated, uncomment and set to 0/90/180/270 to override.
# export AMBERDAV_FB_ROTATE=270

chmod +x "$bin" 2>/dev/null

# Blocks while the server runs; exiting the app from the OS stops it.
# IP, password, and a QR code are written to the log on startup.
"$bin" "$root" "$port" > "$log_file" 2>&1
