# Steam Deck Verification Checklist

Build/transfer the `amber-dav-x86_64-linux-handheld` asset to the Deck.

## Desktop Mode (sanity)
- [ ] `WAYLAND_DISPLAY` is set under KDE; `amber-dav .` opens the connection
      screen as a Wayland window (sink auto-selected = Wayland).
- [ ] `AMBERDAV_DISPLAY=fb amber-dav .` falls back to the framebuffer/TTY path
      (or logs "cannot open /dev/fb0" if KDE holds it — expected).
- [ ] The web UI + WebDAV are reachable from another machine at the shown IP.

## Game Mode (the target)
- [ ] Added as a Non-Steam Game and launched: the IP/password/QR fill the screen.
- [ ] The QR scans and opens the file manager from a phone.
- [ ] Late Wi-Fi: with Wi-Fi off at launch it shows "Waiting for Wi-Fi…", then
      recovers to the full screen within ~1s of connecting (frame-callback repaint).
- [ ] Status tab in the web UI shows `ok (wayland WxH) frame=N`.

## Gamepad viewer (Game Mode + Desktop Mode)
- [ ] Status tab streams button/axis events when the Deck's controls are pressed
      (confirms evdev still sees the devices under Steam Input/Gamescope).
- [ ] Record the evdev codes for a convenient quit button (e.g. STEAM, QAM "…",
      L4/R4) and set `AMBERDAV_EXIT_KEY=<code>`; confirm it quits the app. Note
      the default 354 (KEY_GOTO) is Anbernic-only and will not fire on the Deck.
- [ ] Note whether A (304) / X (307) reach the app or whether Steam Input remaps
      them. Under Wayland, A (Black) DOES blank the screen; X (Bounce) has no
      visible effect — the DVD-bounce screensaver is framebuffer-only and falls
      back to the info screen under Wayland (file a follow-up if a Wayland
      screensaver is wanted).

## Record findings
- [ ] File issues/follow-ups for anything that didn't work (exit-key default,
      Wayland blank/bounce, sidecar refresh on IP change).
