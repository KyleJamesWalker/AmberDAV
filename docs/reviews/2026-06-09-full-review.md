# AmberDAV — Full Codebase Review

> Generated 2026-06-09 by a six-agent parallel review (backend bugs, frontend bugs,
> architecture, performance, device compatibility, general improvements).
> Every finding was verified against the actual code; `file:line` references are
> from commit `cf02898`. Each item is written to be self-contained so it can be
> lifted directly into a GitHub issue.
>
> **Overall assessment:** the codebase is in notably good shape for a single-binary
> project — every module has a `//!` doc header, ~49 unit tests cover the extracted
> pure functions, CI exercises all three feature shapes, downloads/uploads/zip all
> stream, and path traversal via `..` is blocked. The findings below are mostly
> hardening, polish, and consolidation, not rescue. A "done well — don't fix"
> section at the end lists things that look like problems but aren't.

---

## 1. Bugs

### 1.1 [HIGH][Windows] Drive-letter path segment escapes the served root
`src/files.rs:21-32` (`resolve`), `src/files.rs:35-43` (`safe_name`)

`resolve()` rejects `..`, `\`, and NUL — but not `:`. On Windows, `PathBuf::push`
has a documented quirk: a path with a *prefix* but no root **replaces** self. So
`GET /api/list?path=C:` makes `out.push("C:")` discard the served root entirely,
and every later segment resolves outside it. Read/list/download (and with write
permission, upload/delete) anywhere on the drive. The WebDAV side is unaffected
(dav-server has its own resolver); the JSON `/api/*` layer is the gap.

**Fix:** in `resolve` and `safe_name`, accept a segment only if
`Path::new(seg).components()` yields exactly one `Normal` component. That is
OS-agnostic and closes drive letters, `\\?\` prefixes, and rooted segments at once.

### 1.2 [HIGH] Stale-response race: rapid navigation renders the wrong directory
`src/web/app.html:558-563` (`go`)

`go()` has no request-token guard (unlike `showPreviewAt`, which has
`previewToken`). Click folder A then quickly folder B: `cwd` is immediately B,
but whichever `/api/list` resolves *last* wins `entries`. If A's listing lands
after B's, the list shows A's files while breadcrumbs/`cwd` say B — and every
subsequent action (delete, paste, thumbnails) builds paths from `cwd=B` against
A's filenames. With colliding names this deletes the wrong file.

**Fix:** monotonically increasing nav token captured before the `await`; discard
the response if stale (mirror the existing `previewToken` pattern).

### 1.3 [MEDIUM] Copy-paste into the source folder truncates the file to zero bytes
`src/web/app.html:628-634` (`doPaste`) → `src/files.rs:204-223` (`move_`), `225-255` (`copy`)

Neither client nor server checks `src == dst` or destination existence.
`std::fs::copy(A/x, A/x)` (copy into the folder the file was copied from)
truncates the file onto itself → **silent zero-byte data loss**. Separately,
any name collision on move/copy/rename silently overwrites the existing file
(`tokio::fs::rename` at `files.rs:190` has the same behavior).

**Fix:** backend rejects `src == dst` (especially for copy) and returns 409 when
the destination exists (or requires an `overwrite=true` flag); client prompts
on collision.

### 1.4 [MEDIUM] Upload truncates the target before bytes arrive; aborted upload leaves a corrupt partial
`src/files.rs:294-308` (`upload`)

`File::create` truncates an existing file *immediately*, before the first chunk.
A dropped Wi-Fi connection mid-upload (common on handhelds) destroys the original
and leaves a truncated partial in its place. Inconsistent with the rest of the
codebase, which deliberately writes-then-renames (`connection.rs:37-57`,
`update.rs:226-235`). Client-side, a mid-batch failure shows a generic "Upload
failed" without reporting how many of N succeeded (`src/web/app.html:727-742`).

**Fix:** stream to `name.part` in the same directory, flush, `rename` into place
on success; remove the temp on any error path. Client reports "uploaded X of N"
on partial failure. (See also 2.6 — silent overwrite on upload.)

### 1.5 [MEDIUM] Editor data loss: no unsaved-changes guard
`src/web/app.html:704` (`closeEditor`), `936` (Esc), `679-692` (`openEditor`)

Esc (a documented shortcut) closes the editor and discards all edits with no
confirmation; there is no `beforeunload` handler, so tab-close / back-button /
sidebar navigation also silently lose edits. Given the editor is pitched for
editing `config.json` in place, this is a likely real-world data-loss path.

**Fix:** dirty flag (compare against loaded text); `confirm()` on close/Esc when
dirty; `beforeunload` listener while dirty.

### 1.6 [MEDIUM] Symlinks inside the served root escape it
`src/files.rs:21-32` (`resolve`)

`..` is rejected, but the resolved path is never canonicalized or checked to
remain under `root`. A symlink inside the tree (`link -> /etc`) lets
`/api/raw?path=link/passwd`, download, zip, delete, move, and copy operate
outside the root. Post-auth, requires a pre-existing symlink, and doesn't apply
on FAT SD cards — but contradicts the function's "guaranteed to stay within
root" doc claim.

**Fix:** canonicalize the result (or its parent for create ops) and verify
`starts_with(root)`.

### 1.7 [LOW] Timing-unsafe secret comparisons; no brute-force throttling
`src/auth.rs:36`, `src/auth.rs:56`, `src/webdav.rs:88`

Login password, session cookie, and DAV Basic auth all compare with ordinary
`==`. Timing attacks over a LAN are mostly impractical; the real issue is the
combination of **no rate limiting** with the 5-char default password
(~24 bits from a 30-symbol alphabet) on a server bound to `0.0.0.0` by default —
brute-forceable on a hostile LAN.

**Fix:** constant-time compare (`constant_time_eq`/`subtle`); small per-IP
failure delay/backoff; consider a longer default password.

### 1.8 [LOW] Self-update installs the binary with zero integrity verification
`src/update.rs:201-235`

The downloaded asset is streamed to `<exe>.new`, chmod +x'd, and renamed over
the running binary. Only validation is the HTTPS domain allowlist. A connection
drop mid-download installs a truncated binary; the user discovers it when the
app won't launch (recoverable via `.old`, but on-device that means SD-card
surgery). Additionally, `apply` trusts the client-supplied `asset_url`
(domain-checked only, `update.rs:191-195`) rather than re-resolving via
`asset_name()` — a wrong-platform-asset footgun.

**Fix:** (a) verify bytes-written against `Content-Length`; (b) publish
`SHA256SUMS` from the release workflow (see 2.18) and verify; (c) cheap
fallback — check ELF/Mach-O/MZ magic before the rename; (d) re-resolve the
asset server-side instead of trusting the request body. Also: the reqwest
clients in `update.rs:139-150, 201-207` have **no timeout** — a hung check
leaves the Update button disabled forever; set `.timeout(30s)`.

### 1.9 [LOW] Folder drag-and-drop silently fails
`src/web/app.html:961-964` → `uploadFiles`

Dropping a folder yields a useless `File` entry (no `webkitGetAsEntry`
traversal); the upload errors with a generic message or silently does nothing.
No indication that folder upload is unsupported. (Feature-level fix tracked as
2.1; the bug-level fix is to detect directory items and toast "folder upload
isn't supported — zip it first.")

### 1.10 [LOW] Shift-click range selection desyncs after sorting
`src/web/app.html:532` (`setSort`), `565-570` (`onRowClick`)

The hidden-files toggle resets `lastIndex` (line 914) but `setSort` doesn't.
After re-sorting, the anchor index points at a different entry, so shift-click
selects the wrong contiguous range.

**Fix:** reset `lastIndex = null` in `setSort`, or recompute it from the
anchored entry name after re-sort.

### 1.11 [LOW] Status tab interpolates unescaped values into `innerHTML`/`href`
`src/web/app.html:805-809` (`loadConn`), `820-829` (`loadSettings`), `846-892` (update status)

`i.ip` and `i.dav` go into HTML (including an `href` attribute) raw; settings
values (`s.permission`, `s.default_folder`, favorites names, version strings)
are also unescaped via the `row()` helper. The data source is the server's own
config rather than other users, so this is not remotely exploitable XSS in the
common case — but it's an unsafe pattern and a markup-breakage risk (e.g. a
favorite named `<b>roms`). Filename rendering elsewhere is correctly escaped.

**Fix:** build these nodes with `textContent`/`setAttribute`, or escape
consistently. Note `escapeHtml` (line 429) is not attribute-context-safe.

### 1.12 [LOW] Zip stream failures are invisible; duplicate entry names collide
`src/files.rs:490` (error to stderr only), `507-540` (`build_zip`)

If `build_zip` aborts mid-stream, the user gets a truncated zip that fails to
open with no explanation. The API also accepts arbitrary path lists, so
duplicate top-level names produce duplicate archive entries.

---

## 2. Improvements

*(Product/UX, then operational. Each is issue-ready.)*

### Product / UX

#### 2.1 Folder upload support — **M, highest-value capability gap**
`/api/upload` takes a single flat `name` (`src/files.rs:35` rejects `/`); the
picker is `<input type="file" multiple>` (`src/web/app.html:338`). The core
use case is loading ROM folders onto an SD card. Approach: traverse dropped
directories via `DataTransferItem.webkitGetAsEntry()`, add a `webkitdirectory`
picker, and either accept a validated relative path in upload `name` or drive
`/api/mkdir` + per-file uploads from the client.

#### 2.2 Touch support: multi-select and context menu — **M**
Multi-select requires Ctrl/Shift-click (`app.html:564-571`); the context menu
requires right-click, which iOS Safari never fires. The QR-code-to-phone flow
is a first-class path that currently dead-ends at single-select with no menu.
Approach: a "select mode" toggle (checkboxes / tap-to-toggle) plus a long-press
handler (pointer events + timer) opening the existing menu.

#### 2.3 Search/filter box — **S, best effort-to-delight ratio**
ROM folders hold thousands of entries; the only way to find one is scrolling.
The client already holds the full listing in `entries` (`app.html:365`) — a
toolbar filter input narrowing `view` in `rebuild()` (`app.html:444`) plus a
`/` shortcut is purely client-side.

#### 2.4 Drag-and-drop move within the file list — **M**
Moving requires Cut → navigate → Paste. The backend endpoint exists
(`/api/move`); this is frontend-only (drag rows onto folder rows / breadcrumb
segments; mind the existing file-drop upload overlay at `app.html:955-964`).

#### 2.5 Free-disk-space indicator — **S**
Users filling an SD card hit "disk full" mid-upload with no warning. Add
`free`/`total` for the served root to `/api/info` (`src/ui.rs:34`) via a small
statvfs shim; render in sidebar footer + Status tab.

#### 2.6 Uploads silently overwrite existing files — **S**
`File::create` is unconditional (`src/files.rs:294`). Painful with
`config.json`/saves. Client checks `entries` for collisions and confirms;
optionally server `overwrite=false` param using `OpenOptions::create_new` →
409\. (Pairs with bug 1.4.)

#### 2.7 "New File" action — **S**
The editor is pitched for maintaining `config.json` but can only open existing
files. Add "New File" next to "New Folder" (empty PUT to `/api/upload`, then
open the editor).

#### 2.8 Zip download naming — **S**
Always `amber-dav.zip` (`src/files.rs:499`). A single-folder download of
`Roms/GB` should be `GB.zip`. Derive from the selection.

#### 2.9 "Hidden" toolbar toggle has no visual on-state — **S**
`$('t-hidden').classList.toggle('on', …)` (`app.html:913`) but no `.tbtn.on`
CSS rule exists (only `.seg-toggle button.on`, `app.html:92`). Zero feedback.

#### 2.10 Banner/QR/sidecar show the LAN IP even when bound to loopback — **S**
`amber-dav --bind 127.0.0.1` prints a QR pointing at an address that refuses
connections (`src/main.rs:265, 284-303`). Use the bind address when specific.

#### 2.11 Login page copy is wrong for fixed passwords and desktop builds — **S**
`src/web/login.html:73-74` claims the code "changes every time the app
restarts. It is never displayed here" — false with a configured password, and
desktop builds have no "handheld's screen." Soften the copy; add a show/hide
eye toggle (5-char codes are easy to mistype blind on a phone).

#### 2.12 Update-check errors surface raw reqwest text — **S**
GitHub's 60/hr unauthenticated rate limit surfaces as a raw `403 Forbidden`
blob (`src/update.rs:113-117`). Map 403/429 → "GitHub rate limit reached";
timeouts → "could not reach github.com."

### Operational

#### 2.13 SIGTERM is not handled — no graceful shutdown under systemd/Docker — **S**
`shutdown_signal` only awaits `ctrl_c()` (`src/main.rs:305-310`), but the
README markets headless builds for servers/NAS/Docker where stop = SIGTERM.
Add a `signal(SignalKind::terminate())` branch in a `select!` (cfg(unix)).

#### 2.14 Port-in-use produces a raw OS error — **S**
`TcpListener::bind(...).await?` (`src/main.rs:277`) surfaces as
`Os { code: 48, kind: AddrInUse }`. Wrap it: which address/port, and what to
do (`--port` / config `"port"`). Same for an unparseable `--bind`.

#### 2.15 Fatal startup errors never reach the device screen — **M**
The info screen paints before the TCP bind (`src/main.rs:268-277`); on bind
failure the process exits and the handheld flashes back to the OS menu with no
clue (evidence only in `log.txt`). The `config_error` red-line plumbing
(`src/canvas.rs:78-93`, issue #19) is a ready-made pattern: paint the error
for ~10s, then exit. Same for first-run config-write failure
(`src/main.rs:103` is stderr-only).

#### 2.16 No log levels, timestamps, or request logs — **M**
All diagnostics are bare `eprintln!` with inconsistent prefixes and no
timestamps; there are no per-request access logs. Suggest `tracing` +
`tracing-subscriber` with `RUST_LOG`/`--verbose`, plus a `tower_http::trace`
layer — or, if musl binary size matters, a tiny timestamp+level macro.

#### 2.17 `connection.json` sidecar written once at boot — stale IP forever — **S**
The screen re-queries the IP every paint precisely because Wi-Fi associates
late (`src/screen.rs:168-171`), but the sidecar is written once pre-bind
(`src/main.rs:256-263`); launchers/Decky may read `"ip": "0.0.0.0"` forever.
Spawn a task that rewrites on IP change (the write is already atomic).

#### 2.18 Releases ship no checksums; release workflow inefficiencies — **S**
Assets are uploaded bare (`.github/workflows/release.yml:151-155`). Add a job
that writes and uploads `SHA256SUMS` — this also unlocks self-update
verification (bug 1.8). Separately, every matrix job compiles `cargo-edit`
from source just to stamp the version (`release.yml:119-123`); a `sed` on the
`0.0.0` placeholder or `taiki-e/install-action` (also for `cargo-zigbuild`)
saves minutes per job.

#### 2.19 Local builds report `0.0.0` and always see an "update available" — **S**
`version = "0.0.0"` is stamped only in CI. Anyone following the README's build
instructions gets a device screen saying `v0.0.0` and an update check
(`latest == current` string equality, `src/update.rs:128`) offering to replace
their custom `fb` build with a release asset — silently discarding local
patches. Approach: `build.rs` embedding `git describe --tags --dirty` when the
version is `0.0.0`; treat dev versions specially in the update UI; semver
comparison instead of `!=` so a newer-than-latest build isn't offered a
downgrade.

#### 2.20 CI only runs on pull_request — **S**
`.github/workflows/ci.yml:3-4`. Direct-to-main commits (history shows them,
e.g. `c9eb33c`) land untested and feed the release workflow. Add
`push: branches: [main]`.

#### 2.21 README/docs accuracy — **S**
- README:182-183 + login page claim "the password is never in the browser" —
  contradicted by `/api/settings`, which serializes the full `Settings`
  including `password` to any logged-in session (`src/ui.rs:61-63`). Redact
  the field server-side (preferred) or fix the claim.
- In-app Settings help hardcodes "config.json next to the binary"
  (`app.html:326-331`) — wrong for desktop builds since the platform-dirs
  change (`src/config.rs:166-190`). Add `config_path` to `/api/info` and
  render the real path.
- `AMBERDAV_FB_ROTATE` appears only in Troubleshooting, missing from the
  config table (README:201-217).
- README "relaunch to apply" list omits `port`/`bind` (also boot-bound).
- Add a "WebDAV client notes" subsection: Windows `BasicAuthLevel` registry
  requirement and the ~50 MB `FileSizeLimitInBytes` cap (see 6.3), Finder
  PROPFIND slowness, locks are advisory/fake.
- Claims spot-checked as **true**: headless never writes config implicitly;
  config-location table matches; controls table matches; live-vs-boot settings
  split is accurate.

#### 2.22 Code-quality nits — **S**
- Magic numbers deserving named consts: cookie `Max-Age 86400`
  (`auth.rs:58` — 24h cookies outliving expectations is worth a comment of
  its own), password length 5 / token length 32 (`main.rs:148,176`),
  broadcast depth 256 (`main.rs:188`), zip pipe 64 KiB (`files.rs:487`),
  bounce scan cap 5000 (`bounce.rs:178`), editor cap 2 MiB (`app.html:669`).
- Duplicate `/api/info` fetch when opening Settings (`app.html:837-844` vs
  `796`).
- `WebDAV.sh:16`: `&>/dev/null 2>&1` — the trailing `2>&1` is redundant.
- Clippy is clean (headless, this host); all Cargo.toml deps verified used.

---

## 3. Structural Changes (make future changes easier)

#### 3.1 Extract the router into `fn router(state: AppState) -> Router` — **S, highest leverage**
`main.rs:225-250` builds the router inline inside a 190-line `main()`. Nothing
HTTP-level is tested: auth gating of `/api/*`, permission enforcement,
end-to-end traversal rejection, and the `webdav::route` method gate are only
reachable through a live socket. With the extraction, integration tests drive
it via `tower::ServiceExt::oneshot` against a tempdir — no port, no process.
This single seam unlocks the most untested (and security-critical) behavior.

#### 3.2 Move `AppState`/`ServerInfo`/`SharedSettings`/`current_ip` to `src/state.rs` — **S**
Every handler module imports them from the binary root (`main.rs:40-89`), so
the composition root doubles as the shared-types home. A `state.rs` makes the
dependency direction honest and shrinks `main.rs` to wiring. Pairs with 3.1.

#### 3.3 Extract boot-time settings resolution into a pure, tested function — **S/M**
`main.rs:125-173` derives effective values (root `"."`, port `8080`, bind
`"0.0.0.0"`, random-password rule, bounce-path resolution) inline and
untestably — a second resolution layer beyond the tested `cli::resolve`.
Extract `fn effective(settings: &Settings) -> Effective` and promote the
compiled defaults to named consts.

#### 3.4 Route gamepad/SDL exit through the existing shutdown token — **S/M**
A careful graceful-shutdown path exists (CancellationToken, `main.rs:204`;
SSE `take_until`, `ui.rs:68-75`) — but the device exit key calls
`std::process::exit(0)` directly (`input.rs:96`) and so does SDL Quit
(`sdl.rs:125`). In-flight uploads and WebDAV writes are killed mid-stream on
the platform where it matters most. Pass the token into `input::spawn` and the
SDL sink; have `axum::serve` await Ctrl+C *or* the token (with a drain
timeout).

#### 3.5 Deduplicate the per-sink render loop — **S/M**
The `Mode -> canvas` selection is triplicated (`screen.rs:166-186`,
`sdl.rs:140-157`, `wayland.rs:145-160` — where Wayland silently maps `Bounce`
to `Info`, a behavioral divergence hiding in the duplication), as is the
`set_status` helper (three copies). Extract a shared
`render_frame(mode, …) -> Vec<[u8;3]>` next to `canvas.rs`; the Wayland bounce
gap becomes one explicit `// not supported` match arm.

#### 3.6 Consolidate `fb`/`sdl` cfg gates behind one `device` cfg — **M**
`#[cfg(any(feature = "fb", feature = "sdl"))]` and friends appear ~23 times,
with compensating `#[allow(dead_code)]` sprinkles. Step 1 (mechanical): a tiny
`build.rs` emitting `cargo:rustc-cfg=device` → gates become `#[cfg(device)]`.
Step 2 (optional): group device-only modules into `src/device/` with one gated
`mod` and a no-op stub for headless, eliminating most dead-code waivers
(keep `any(device, test)` where headless tests rely on `canvas`/`bounce`).

#### 3.7 Split `app.html` into `app.html` + `app.css` + `app.js` — no build step — **S/M**
At 995 lines mixing ~350 CSS + ~630 JS + markup, the file is at the upper edge
of sustainable, and nothing ever syntax-checks the JS. Split into three files,
each `include_str!`-ed, served from two extra routes in `ui.rs`. Diffs become
reviewable per concern and CI can run `node --check`/a linter over `app.js`.
A bundler is **not** warranted at this size.

#### 3.8 Highest-value missing tests (ordered)
1. Router integration (per 3.1): unauthenticated `/api/list` → 401; cookie
   happy path; `read_only` blocks `POST /api/mkdir` and DAV `PUT`/`MKCOL`/
   `MOVE`; URL-encoded `..` traversal.
2. `webdav::route` method gate (`webdav.rs:54-62`): extract a pure
   `fn method_allowed(method, perm) -> bool` and table-test it — this list IS
   the read-only guarantee and has zero tests.
3. `cli::resolve` precedence: currently untestable (reads `std::env`
   directly, `cli.rs:93-181`). Inject the env lookup
   (`resolve_with(…, env: impl Fn(&str) -> Option<String>)`).
4. `files.rs` handlers against a tempdir via oneshot — covers the
   `safe_name` + `resolve` composition.
5. `auth::login` sets the cookie only on the right password.

#### 3.9 Error handling: keep the current strategy; two small notes — **S**
The undeclared strategy is coherent (handlers → `Response` via helpers; sinks →
`Result<(), String>`; config → loud-but-nonfatal tuple). Don't add
anyhow/thiserror. (1) The `ok()/bad()/forbidden()/io_err()` helpers are
private to `files.rs`; `update.rs`/`webdav.rs` hand-roll equivalents — share
them if any new endpoint family is added. (2) `panic = "abort"`
(`Cargo.toml:71`) means a stray panic kills the device app with nothing on
screen — document it, and keep handlers panic-free (they currently are).

#### 3.10 The build matrix lives in four places — **S to mitigate**
`update.rs:34-50` (`asset_for`), the README table, `ci.yml:69-124`, and
`release.yml:37-99` must stay in sync; the two workflow matrices are
near-duplicates. Cheapest: cross-referencing comments in each + a CI guard
diffing the two YAML matrices. Full unification (reusable workflow) probably
isn't worth the indirection yet.

---

## 4. Code Structures for Agent (and Human) Comprehension

**Already good (preserve these patterns):** every module opens with a
role-stating `//!` header; routes are declared in one visible block; Cargo.toml
has explanatory feature comments; comments cite issue numbers (#15, #19); pure
functions are extracted specifically to be unit-testable; the generated config
is self-documenting JSONC.

#### 4.1 Add a `CLAUDE.md` — **S, do this first** (none exists; nor ARCHITECTURE.md)
Concrete contents:
- **Build/test commands** mirroring `ci.yml` exactly: `cargo fmt --all --
  --check`; `cargo clippy --all-targets -- -D warnings` + `cargo test
  --all-targets` for headless, then with `--features fb` and `--features sdl`
  (sdl needs system libSDL2; `brew install sdl2` on macOS). Device
  cross-build: `cargo zigbuild --release --target aarch64-unknown-linux-musl
  --features fb`.
- **The feature matrix**, three rows (headless / fb / sdl), the "`sdl` wins if
  both" precedence, and that fb/sdl only apply on Linux.
- **Module map** (the README table plus the files it omits: `display.rs` sink
  selection, `bounce.rs` screensaver engine, `connection.rs` sidecar).
- **Gotchas agents will otherwise "fix":** `version = "0.0.0"` is intentional
  (stamped from the release tag); `panic = "abort"`; the generated config is
  JSONC on purpose; the canvas palette is hand-synced with `app.html`;
  device-only code paths are unreachable on a dev machine — verify via the
  cfg'd test suites.
- **Entry points:** routing = `main.rs` router block; web UI =
  `src/web/app.html` via `include_str!` in `ui::index`; config precedence =
  CLI > env > file > defaults, resolved in `cli.rs::resolve`.

#### 4.2 Routes table doc comment at the router — **S**
Auth/permission semantics per route aren't visible from the router block. Add
above `Router::new()` (or the future `router()` fn):
```
/// Route map (auth: S = session cookie, B = HTTP Basic, - = public):
///   GET  /             S -> app.html | redirect /login
///   POST /login        -    sets sid cookie
///   GET  /api/list     S    read
///   PUT  /api/upload   S    write
///   ANY  /dav[/*]      B    read; write methods gated by permission
```
Also note here that permission enforcement lives in TWO places (`files.rs`
per-handler + `webdav.rs:54` method gate) and must stay in sync.

#### 4.3 Cross-reference the five display-adjacent modules — **S**
`screen.rs` (mode state + fb sink), `display.rs` (sink *selection*),
`canvas.rs` (pixel content), `sdl.rs`, `wayland.rs` orbit one concern; an
agent must read all five headers to learn the layering. Add a one-line "see
also" chain to each header: *content: canvas.rs → choice: display.rs → sinks:
screen.rs/sdl.rs/wayland.rs → state: screen::Mode*. Structural follow-up: the
`src/device/` folder from 3.6, plus renaming `ui.rs` → `web.rs` — `ui.rs` is a
trap (it's the *web* handlers, while "the UI" in this project means the device
screen; it already lives next to `src/web/`).

#### 4.4 Document the build/display matrix in-code — **S**
The best in-repo explanation currently lives in Cargo.toml comments and
`asset_for`'s doc — not where an agent looks first. Add to the `main.rs`
module doc: the three build shapes, and the runtime sink chain for `fb` builds
(`display::detect()` → Wayland (Gamescope) | `/dev/fb0` | headless,
overridable via `AMBERDAV_DISPLAY`) and the sdl driver preference list
(`sdl.rs:24`).

#### 4.5 Header comment in `app.html` — **S**
Ten lines at the top: served by `ui::index` via `include_str!` (auth-gated;
`login.html` is the public page); a TOC of the `// ----` script sections; the
`:root` palette is mirrored in `src/canvas.rs`; the API surface is the
`/api/*` route set in `main.rs`. Saves every future agent a 1000-line scan.

---

## 5. Speed Improvements

*(Target hardware: Allwinner H700 — 4× Cortex-A53, ~1 GB RAM, slow SD storage.)*

#### 5.1 [HIGH] Thumbnails serve the full-size original — no downscale, no caching
`app.html:407` (`thumbUrl = '/api/raw?…'`) → `files.rs:376` (streams whole file);
no `Cache-Control`/`ETag`/`Last-Modified` on the response (`files.rs:436-448`).

Grid view on a folder of 300 × 2 MB PNGs reads ~600 MB off the SD card and
ships it over Wi-Fi to paint 128 px cells — and every revisit re-downloads
everything, because nothing is cacheable. `loading="lazy"` limits it to
visible cells, but each fetch is still the whole file.
- **S:** add `Cache-Control: private` + `ETag`/`Last-Modified` (mtime+size)
  with conditional-request handling on `/api/raw`. Kills the repeat storms.
- **M:** `/api/thumb?path=…&w=128` that downscales server-side and caches to
  disk. Note the `image` crate is currently only compiled under fb/sdl
  (`Cargo.toml:53`); headless would need it added.

#### 5.2 [MEDIUM] `/api/download` ignores Range — no resumable downloads — **S**
`files.rs:542-574` streams (good) but ignores `Range` and sends no
`Accept-Ranges`. A 4 GB image that dies at 90% over flaky Wi-Fi restarts from
zero. The parse/seek/`take` machinery already exists, tested, for `raw`
(`files.rs:350-372, 404-430`) — reuse it.

#### 5.3 [MEDIUM] Wayland sink re-renders the static screen at compositor refresh — **S/M**
`wayland.rs:261-269` re-arms the frame callback unconditionally and
`build_canvas()` (`wayland.rs:145-160`) rebuilds everything — including
`QrCode::new` and a netlink IP lookup — then converts ~1 M pixels, every frame.
~60 fps software rendering of a static screen = constant CPU/battery burn on
the Steam Deck. Keep the callback loop but skip content rebuild unless N
callbacks elapsed or mode/IP changed (mirror the fb sink's `frame % 40`
throttle, `screen.rs:160-163`); cache the rendered canvas.

#### 5.4 [MEDIUM] Bounce screensaver: full decode per bounce + per-frame allocation — **M**
`bounce.rs:117-126` → `198-207`: each wall bounce calls `image::open` on the
*full* image before downscaling — a 12 MP JPEG is ~36 MB RGB and hundreds of
ms on an A53 (visible hitch). `bounce.rs:142-160` allocates a fresh ~900 KB
canvas every frame at 12.5 fps; `screen.rs:355-391` then does per-pixel
rotate/pack to every fb page. Pre-decode a small sprite pool (or LRU cache);
reuse one canvas buffer; optionally blit only the dirty rect.

#### 5.5 [MEDIUM] Frontend rebuilds the entire list DOM on every selection click — **S**
`onRowClick` ends in `render()` (`app.html:564-572`), which wipes and rebuilds
every row/card (`app.html:487-530`). In a 3,000-entry folder every
click/ctrl-click/shift-click rebuilds 3,000 `<tr>`s — hundreds of ms of jank
on a phone. Toggle the `sel` class on affected rows only; full `render()` only
on listing/sort/view changes. (Optional L: virtualization for huge folders.)

#### 5.6 [LOW] SDL sink presents at ~60 fps while idle — **S**
`sdl.rs:171-177` clears/copies/presents every 16 ms even when the texture
wasn't updated. Present only when refreshed (plus occasional re-present);
lengthen the sleep in Info/Black modes.

#### 5.7 [LOW] Server-side sort allocates two lowercase Strings per comparison — and the client re-sorts anyway — **S**
`files.rs:113-117` vs `app.html:444-459` (`sortView` always re-sorts). A
5,000-entry folder ≈ 120k transient allocations per `/api/list`, all wasted.
Drop the server sort or precompute a sort key.

#### 5.8 [LOW] fb info screen rebuilds the QR every ~3.2 s re-latch — **S**
`screen.rs:160-199`: the periodic repaint (deliberate — console cursor stomps
the fb; keep the blit) also rebuilds `info_canvas` including `QrCode::new`.
Cache the canvas; rebuild only when `current_ip()` changes.

#### 5.9 [LOW] No compression / ETag on the embedded SPA — **S, marginal**
`app.html` (~36 KB) and large `/api/list` JSON go uncompressed; `/` has no
ETag despite being immutable per build (`ui.rs:21-27`). gzip costs A53 CPU and
file payloads are incompressible, so scope a `CompressionLayer` to HTML/JSON
only — or skip; genuinely marginal.

#### 5.10 [LOW] `opt-level = "z"` slows hot pixel/decode loops — **S**
`Cargo.toml:66-71`. Right trade for binary size; if screensaver hitches become
a complaint, add `[profile.release.package.image] opt-level = 3`.

---

## 6. Device Compatibility

#### 6.1 [HIGH→ see 1.1] Windows drive-letter traversal — the top compat *and* security item.

#### 6.2 [MEDIUM] All-zero fb RGB bitfields render a black screen reported as "ok"
`screen.rs:355-408`. The generic bitfield-driven pack correctly handles
RGB565/XRGB8888/BGRA (done well) — but some DRM-backed fbdev shims report
zeroed `red/green/blue` bitfield lengths, making `pack` return 0 for every
pixel: an entirely black screen with `render ok` status. Detect all-zero
lengths in `Geom::probe` and fall back by `bits_per_pixel` (16→RGB565,
32→XRGB8888), surfacing the assumption in the status string.

#### 6.3 [MEDIUM] Windows Explorer cannot mount `/dav` out of the box — document it
`webdav.rs:30-49`. The Windows WebClient mini-redirector **refuses Basic auth
over plain HTTP by default** (`BasicAuthLevel`), and caps downloads at ~50 MB
(`FileSizeLimitInBytes`). dav-server *does* advertise class 2 and provide
LOCK via `FakeLs` (verified in crate source), so the class-2 requirement is
met — the auth-over-HTTP policy is the blocker, and it's undocumented. Add a
README "WebDAV client notes" section (registry keys + WebClient restart), or
recommend rclone on Windows. Also note locks are advisory/fake (fine for a
single-user LAN tool; one doc line).

#### 6.4 [MEDIUM] RG34XXSP rotated panel: ships sideways until the user finds `AMBERDAV_FB_ROTATE`
`screen.rs:299-305`, `example_APPS/WebDAV.sh:32-33`. Rotation defaults to 0;
the README claims both devices work with "the same aarch64 binary," but a
rotated panel renders sideways out of the box and the only fix is editing the
launcher. The rotation math itself is correct and bounds-tested (done well).
Fix: per-device default via `/proc/device-tree/model` detection, or at minimum
make `WebDAV.sh` set it and document which device needs which value.

#### 6.5 [MEDIUM] No evdev hotplug — controllers connected after launch are inert
`input.rs:50-133`. `evdev::enumerate()` runs once at startup. A Bluetooth pad
paired after launch (Steam Deck / desktop) can't quit/blank and never appears
in the live button viewer. (Discovery itself is capability-based, not
hard-coded paths — done well.) Fix: inotify watch on `/dev/input` or periodic
re-enumeration.

#### 6.6 [MEDIUM] `std::env::set_var` from the SDL thread while other threads run
`sdl.rs:55`. Called in a loop while the tokio runtime and input threads are
live; `set_var` is unsound in multithreaded context (and `unsafe` in Rust
2024). In practice SDL reads it on the same thread, but a concurrent `getenv`
is UB and this breaks on a future toolchain bump. Use SDL's hint API
(`SDL_HINT_VIDEODRIVER`) or set it once before spawning threads.

#### 6.7 [LOW] dav-server's platform resolvers are disabled on Windows/macOS
`webdav.rs:33`: `LocalFs::new(root, false, false, false)` hard-codes
`case_insensitive=false` and `macos=false` on every platform, leaving
dav-server's existing `localfs_windows`/`localfs_macos` handling off. On
case-insensitive hosts the mini-redirector's case-normalized lookups can miss.
Pass `cfg!(windows)` / `cfg!(target_os = "macos")`.

#### 6.8 [LOW] Windows reserved device names accepted
`files.rs:35-43`. `con`, `nul`, `aux`, `com1`… (and `name.txt` forms) pass
`safe_name`; creating them on Windows fails with an opaque OS error. Reject
reserved basenames on Windows targets. Not a security issue (stays in-root).

#### 6.9 [LOW] Hidden-files toggle is dotfile-only
`app.html:445`. Windows hidden-attribute and macOS `UF_HIDDEN` files show as
normal. Acceptable for a LAN tool — document that "hidden" means dotfiles.

#### 6.10 [LOW] Wayland: bounce screensaver silently unavailable
`wayland.rs:149` maps `Mode::Bounce` → `Info`. Fine for Gamescope (which
provides the three required protocols, `wayland.rs:68-71`), but the burn-in
saver doesn't run in Game Mode — relevant for OLED Decks. Note it in README;
making it explicit falls out of the render-loop dedup (3.5).

#### 6.11 Verified fine (no action)
- **CI/release matrix** builds every claimed target/feature combo; the missing
  musl+sdl combo is contradictory by design and not claimed. ✔
- **Browser compat** of `app.html`: EventSource/fetch/optional chaining/lazy
  images — safe back to ~2018 browsers; no bleeding-edge APIs. ✔
- **SDL resolution**: hard-coded 1280×800 is corrected by the `output_size()`
  read-back; worst case is stretch, not crash. Consider logging
  `display_bounds(0)` for diagnosis. ✔
- **All display sinks fail soft** — server keeps running with connection info
  in the log if fb/Wayland/SDL init fails. ✔

---

## Done well — do not "fix"

- Downloads, uploads, zip streaming, and self-update download all **stream**;
  no whole-file buffering anywhere → no OOM on 4 GB files on a 1 GB device.
- Zip-on-the-fly is memory-bounded (64 KiB duplex pipe, client backpressure)
  and deliberately uses `Stored` compression — right call for the CPU and
  already-compressed payloads.
- Range requests with seek+`take` on `/api/raw` — media seeking works.
- Tokio multi-threaded runtime + hyper keep-alive; recursive copy correctly
  pushed to `spawn_blocking`; `tokio::fs` throughout.
- WebDAV PROPFIND/multistatus comes from the `dav-server` crate (streamed,
  conditional headers handled) — no hand-rolled XML.
- Config loaded once at boot, shared via `Arc`; precedence
  CLI > env > file > defaults implemented correctly; `--save` round-trip and
  JSONC parsing correct; config errors surface on the device screen and
  Status tab (issue #19 plumbing).
- SSE bounded (`broadcast::channel(256)` drops lagged) and terminates on
  shutdown via CancellationToken (issue #15).
- fb render loop paced (80 ms, static modes every 40th frame); Wayland shm
  buffer reused across frames; SDL texture updates throttled.
- Frontend: filenames correctly escaped everywhere they hit `innerHTML`;
  URL-encoding via `encodeURIComponent` consistent (incl. `#?%&`, unicode);
  zip paths via UTF-8-safe base64 matching the backend; preview has a
  stale-response token guard; text preview capped at 2 MiB.
- Cookies `HttpOnly; SameSite=Strict` (CSRF mitigation); all `/api/*` and
  `/dav` routes auth-gated; WebDAV write methods gated by permission level,
  matching the JSON API; SSRF domain allowlist in update.rs sound;
  `parse_range` overflow-safe.
- evdev discovery capability-based; display-sink selection unit-tested
  including Gamescope socket discovery; framebuffer rotation bounds-proven by
  test; module `//!` headers everywhere; ~49 unit tests on extracted pure
  functions; CI runs fmt/clippy/test across all three feature shapes.

---

## Suggested issue-filing priority

| # | Item | Section | Effort |
|---|------|---------|--------|
| 1 | Windows drive-letter path traversal | 1.1 | S |
| 2 | Navigation stale-response race | 1.2 | S |
| 3 | Copy-onto-self truncation + collision overwrites | 1.3 | S |
| 4 | Upload truncate-before-write (temp + rename) | 1.4 | S |
| 5 | Editor unsaved-changes guard | 1.5 | S |
| 6 | Thumbnail caching (then real thumb endpoint) | 5.1 | S→M |
| 7 | Router extraction + integration tests | 3.1/3.8 | S |
| 8 | CLAUDE.md | 4.1 | S |
| 9 | Self-update integrity (+ SHA256SUMS in releases) | 1.8/2.18 | M |
| 10 | Folder upload | 2.1 | M |
| 11 | Touch multi-select / long-press menu | 2.2 | M |
| 12 | SIGTERM + friendly bind errors + on-screen fatal errors | 2.13–2.15 | S/M |
| 13 | Range support on /api/download | 5.2 | S |
| 14 | Wayland/SDL idle render throttle | 5.3/5.6 | S/M |
| 15 | RG34XXSP rotation default + fb black-screen fallback | 6.4/6.2 | M |
