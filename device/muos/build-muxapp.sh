#!/usr/bin/env bash
#
# build-muxapp.sh — package AmberDAV as a muOS .muxapp for one-file installs.
#
# A .muxapp is just a ZIP whose single top-level folder (named after the app)
# holds mux_launch.sh, the binary, and the menu glyph. muOS's Archive Manager
# extracts that folder to <storage>/MUOS/application/AmberDAV/.
#
# Usage:
#   device/muos/build-muxapp.sh path/to/binary  # package an already-built binary
#   AMBER_DAV_BIN=dist/amber-dav-aarch64-linux-sdl device/muos/build-muxapp.sh
#   device/muos/build-muxapp.sh                  # build the sdl binary, then package
#
# Output: dist/AmberDAV.muxapp — copy that one file to the device's ARCHIVE.
#
# The muxapp ships the SDL build (amber-dav-aarch64-linux-sdl): muOS is an
# SDL-native platform, so the SDL sink composites instantly where the raw
# framebuffer build lags. The SDL build is aarch64-gnu and dynamically links the
# system libSDL2 (present on muOS) — so it does NOT cross-compile cleanly off an
# aarch64 Linux host. The easy path is to pass the prebuilt
# `amber-dav-aarch64-linux-sdl` release asset; the from-source fallback below
# only works on an aarch64 Linux box with libsdl2-dev (and is what CI does).
#
# This mirrors the CI `muxapp` job in .github/workflows/release.yml; keep the
# two layouts in sync.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
target="aarch64-unknown-linux-gnu"
# CI overrides MUXAPP_OUT with a versioned name (AmberDAV-<version>.muxapp);
# locally it defaults to a plain name that's easy to copy to the device.
out="${MUXAPP_OUT:-$repo/dist/AmberDAV.muxapp}"
out_dir="$(dirname "$out")"

# Binary source: positional arg > env var > built from source.
bin="${1:-${AMBER_DAV_BIN:-}}"
if [ -z "$bin" ]; then
  bin="$repo/target/$target/release/amber-dav"
  echo ">> Building amber-dav ($target, --features sdl)… (needs aarch64 Linux + libsdl2-dev)"
  ( cd "$repo" && cargo build --release --target "$target" --features sdl )
fi

if [ ! -f "$bin" ]; then
  echo "error: binary not found: $bin" >&2
  echo "       build it first or pass a path (see usage at top of this script)." >&2
  exit 1
fi

# Stage the inner folder muOS extracts: AmberDAV/{mux_launch.sh, amber-dav,
# glyph/amberdav.png}. The glyph lives in glyph/ (not resources/) because that
# is where muOS reads an app's menu icon — the name matches the `# ICON:`
# header in mux_launch.sh.
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
app="$stage/AmberDAV"
mkdir -p "$app/glyph"
cp "$here/mux_launch.sh" "$app/mux_launch.sh"
cp "$here/glyph/amberdav.png" "$app/glyph/amberdav.png"
cp "$bin" "$app/amber-dav"
chmod +x "$app/mux_launch.sh" "$app/amber-dav"

mkdir -p "$out_dir"
rm -f "$out"
( cd "$stage" && zip -r -q "$out" AmberDAV )

echo ">> Wrote $out"
unzip -l "$out"
