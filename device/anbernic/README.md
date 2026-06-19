# AmberDAV on Anbernic (stock OS)

Ready-to-copy launcher for the **Anbernic stock OS**, which lists apps from
`Roms/APPS/` on the SD card — a `*.sh` script there appears in the Apps menu and
the filename becomes the menu label.

## Layout

Copy this onto the SD card (the games / second card on a two-slot device):

```
Roms/APPS/AmberDAV.sh            <- launcher (this is the Apps-menu entry)
Roms/APPS/AmberDAV/amber-dav     <- the aarch64 binary
Roms/APPS/AmberDAV/config.json   <- written automatically on first launch
Roms/APPS/AmberDAV/log.txt       <- created on launch (IP, password, QR text)
```

- `AmberDAV.sh` — the launcher in this folder; rename it to change the menu label.
- `AmberDAV/` — drop the binary here. Use the **`amber-dav-aarch64-linux-fb`**
  release asset (static musl, framebuffer + gamepad screen), or build it
  yourself:

  ```sh
  cargo zigbuild --release --target aarch64-unknown-linux-musl --features fb
  cp target/aarch64-unknown-linux-musl/release/amber-dav device/anbernic/AmberDAV/amber-dav
  ```

## Use

Launch **AmberDAV** from the device's Apps menu. The script serves the whole SD
card root (two levels up from `Roms/APPS`, so it works whether the card mounts
at `/mnt/sdcard`, `/mnt/mmc`, …) on port `8080` and writes startup output — the
IP, password, and a QR code — to `log.txt`.

On-device controls: **Menu** = quit, **A** = blank screen, **X** = bounce
screensaver.

`AmberDAV.sh` is commented; edit it to change the port, served root, or screen
rotation (`AMBERDAV_FB_ROTATE`). The password regenerates on every launch unless
you set a fixed one in `config.json`.

> The SDL on-screen build (`amber-dav-aarch64-linux-sdl`) is an alternative to
> the framebuffer binary — just point `AmberDAV/amber-dav` at it instead; no script
> changes needed. See the repo README's "Anbernic — SDL on-screen QR" section.
