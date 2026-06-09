#!/bin/bash
#
# SDL.sh — launches the SDL build of amber-dav (on-screen QR via SDL's `mali`
# driver). Copy to the SD card as:
#   Roms/APPS/SDL.sh
#   Roms/APPS/sdl/amber-dav-aarch64-linux-sdl
#   Roms/APPS/sdl/log.txt   (created on launch)

. /mnt/mod/ctrl/configs/functions &>/dev/null 2>&1
export HOME=/root

progdir="$(cd "$(dirname "$0")" || exit; pwd)"
appdir="$progdir/sdl"
bin="$appdir/amber-dav-aarch64-linux-sdl"
log_file="$appdir/log.txt"
root="$(cd "$progdir/../.." || exit; pwd)"
port=8080

# The stock SDL2 build only offers the `mali` vendor driver for display.
export SDL_VIDEODRIVER=mali

chmod +x "$bin" 2>/dev/null
"$bin" "$root" "$port" > "$log_file" 2>&1
