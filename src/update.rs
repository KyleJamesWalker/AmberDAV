//! In-app update: check the GitHub Releases API and apply a downloaded binary.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use tokio::io::AsyncWriteExt;

use crate::{auth::Session, state::AppState};

/// GitHub repo to check for releases.
const REPO: &str = "KyleJamesWalker/AmberDAV";

static UPDATE_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Resolve the release asset name from the build's target + features. Pure (no
/// cfg!) so the precedence can be unit-tested on every platform at once.
///
/// Assets follow `amber-dav-<arch>-<os>[-fb|-sdl]`: plain `<arch>-<os>` is the
/// headless build, `-fb` is the static framebuffer/Wayland device UI, and
/// `-sdl` is the dynamic on-screen build (links libSDL2). On Linux the three
/// variants are distinguished by feature so a device never self-updates across
/// build shapes — a headless server stays headless, an `-fb` device stays
/// `-fb`, and an SDL device stays SDL.
///
/// Precedence is **sdl > fb > headless**. The `fb` and `sdl` features are
/// independent; if a build somehow enables both, `sdl` is matched first so it
/// wins — mirroring the sink selection, where `sdl` overrides the framebuffer.
fn asset_for(arch: &str, os: &str, sdl: bool, fb: bool) -> Option<&'static str> {
    match (arch, os) {
        ("aarch64", "linux") if sdl => Some("amber-dav-aarch64-linux-sdl"),
        ("aarch64", "linux") if fb => Some("amber-dav-aarch64-linux-fb"),
        ("aarch64", "linux") => Some("amber-dav-aarch64-linux"),
        ("x86_64", "linux") if sdl => Some("amber-dav-x86_64-linux-sdl"),
        ("x86_64", "linux") if fb => Some("amber-dav-x86_64-linux-fb"),
        ("x86_64", "linux") => Some("amber-dav-x86_64-linux"),
        // Other Linux arches aren't published release targets.
        (_, "linux") => None,
        ("aarch64", "macos") => Some("amber-dav-aarch64-macos"),
        ("x86_64", "macos") => Some("amber-dav-x86_64-macos"),
        ("x86_64", "windows") => Some("amber-dav-x86_64-windows.exe"),
        ("aarch64", "windows") => Some("amber-dav-aarch64-windows.exe"),
        _ => None,
    }
}

/// Build-target OS string, shared by asset resolution and download
/// verification (which checks the executable magic expected for this OS).
fn current_os() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        ""
    }
}

/// Returns the release asset name for the current platform, or `None` if the
/// platform/build is not a published release target. Thin cfg! wrapper around
/// [`asset_for`].
pub fn asset_name() -> Option<&'static str> {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else {
        ""
    };
    asset_for(
        arch,
        current_os(),
        cfg!(feature = "sdl"),
        cfg!(feature = "fb"),
    )
}

/// True if `head` (the first bytes of a download) starts with the executable
/// magic expected on `os`: ELF on Linux, Mach-O (thin or fat, either
/// endianness) on macOS, MZ on Windows. Catches the truncated-download and
/// wrong-content cases — e.g. an HTML error page — before the binary is
/// installed.
fn looks_like_executable(head: &[u8], os: &str) -> bool {
    match os {
        "linux" => head.starts_with(&[0x7f, b'E', b'L', b'F']),
        "macos" => {
            const MAGICS: [[u8; 4]; 6] = [
                [0xfe, 0xed, 0xfa, 0xce], // MH_MAGIC (32-bit)
                [0xce, 0xfa, 0xed, 0xfe], // MH_CIGAM
                [0xfe, 0xed, 0xfa, 0xcf], // MH_MAGIC_64
                [0xcf, 0xfa, 0xed, 0xfe], // MH_CIGAM_64
                [0xca, 0xfe, 0xba, 0xbe], // FAT_MAGIC
                [0xbe, 0xba, 0xfe, 0xca], // FAT_CIGAM
            ];
            head.len() >= 4 && MAGICS.iter().any(|m| &head[..4] == m)
        }
        "windows" => head.starts_with(b"MZ"),
        _ => false,
    }
}

/// Find the lowercase hex SHA-256 for `name` in sha256sum-format output
/// (`<hex>  <filename>` per line; binary-mode lines prefix the name with `*`).
fn sha256_entry(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?;
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

/// Validate a finished download before it replaces the running binary. All
/// inputs are plain values so every rejection path is unit-testable.
fn verify_download(
    written: u64,
    expected_len: Option<u64>,
    head: &[u8],
    os: &str,
    actual_sha: &str,
    expected_sha: Option<&str>,
) -> Result<(), String> {
    if let Some(expected) = expected_len {
        if written != expected {
            return Err(format!(
                "download incomplete: got {written} of {expected} bytes"
            ));
        }
    }
    if !looks_like_executable(head, os) {
        return Err("downloaded file is not an executable for this platform".into());
    }
    if let Some(expected) = expected_sha {
        if actual_sha != expected {
            return Err(format!(
                "checksum mismatch: expected {expected}, got {actual_sha}"
            ));
        }
    }
    Ok(())
}

#[derive(Serialize)]
pub struct CheckResult {
    pub current: String,
    pub latest: String,
    pub up_to_date: bool,
    /// True for a from-source build (unstamped `0.0.0[+describe]`). The UI
    /// labels these instead of advertising an "update" that would replace a
    /// custom build with the latest release (issue #46).
    pub dev_build: bool,
    /// Download URL for the matching asset, if one exists for this platform.
    /// Informational only — `apply` re-resolves the asset itself rather than
    /// trusting a URL from the client.
    pub asset_url: Option<String>,
}

/// The up-to-date verdict (issue #46): release builds compare numerically —
/// equal or *newer* than the latest release is up to date, so a local build
/// of an unreleased version is never offered a downgrade (the old `latest ==
/// current` string equality flagged it). Dev builds are never "up to date"
/// (there is nothing meaningful to compare), but the paired `dev_build` flag
/// tells the UI to present that as "development build", not "update now".
fn verdict(current: &str, latest: &str) -> (bool, bool) {
    let dev = crate::version::is_dev(current);
    let up_to_date =
        !dev && crate::version::cmp_versions(current, latest) != std::cmp::Ordering::Less;
    (up_to_date, dev)
}

#[derive(Serialize)]
pub struct ApplyResult {
    pub ok: bool,
    pub message: String,
}

/// Minimal GitHub Releases API response — only the fields we need.
#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Translate an HTTP failure into a message a person can act on: GitHub's
/// unauthenticated rate limit (60/hr) otherwise surfaces as a raw
/// "403 Forbidden" blob, and timeouts as a reqwest debug string. Pure so the
/// mapping is unit-testable without manufacturing reqwest errors.
fn friendly_http_failure(status: Option<u16>, unreachable: bool, raw: &str) -> String {
    match status {
        Some(403) | Some(429) => "GitHub rate limit reached — try again later".into(),
        Some(s) => format!("github.com returned HTTP {s}"),
        None if unreachable => {
            "could not reach github.com — check the connection and try again".into()
        }
        None => raw.to_string(),
    }
}

fn friendly_reqwest_error(e: &reqwest::Error) -> String {
    friendly_http_failure(
        e.status().map(|s| s.as_u16()),
        e.is_timeout() || e.is_connect(),
        &e.to_string(),
    )
}

/// GET /api/update/check — compares current version against the latest GitHub release.
pub async fn check(_: Session, _: State<AppState>) -> Response {
    let current = crate::version::VERSION.to_string();

    match fetch_latest_release().await {
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": friendly_reqwest_error(&e) })),
        )
            .into_response(),
        Ok(release) => {
            // Tags are "vX.Y.Z"; strip the leading "v" for comparison.
            let latest = release.tag_name.trim_start_matches('v').to_string();
            let asset_url = asset_name().and_then(|name| {
                release
                    .assets
                    .iter()
                    .find(|a| a.name == name)
                    .map(|a| a.browser_download_url.clone())
            });
            let (up_to_date, dev_build) = verdict(&current, &latest);
            Json(CheckResult {
                up_to_date,
                dev_build,
                current,
                latest,
                asset_url,
            })
            .into_response()
        }
    }
}

async fn fetch_latest_release() -> Result<GhRelease, reqwest::Error> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    reqwest::Client::builder()
        .user_agent("amber-dav")
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json::<GhRelease>()
        .await
}

/// POST /api/update/apply — downloads the latest release's binary for this
/// platform and replaces the running binary. The asset is re-resolved here
/// from the GitHub release via [`asset_name`] rather than read from the
/// request body, so a client can't point the updater at the wrong platform's
/// binary (or any other URL).
/// The running process is NOT restarted; caller must relaunch after this returns Ok.
pub async fn apply(_: Session, _: State<AppState>) -> Response {
    match do_apply().await {
        Ok(msg) => Json(ApplyResult {
            ok: true,
            message: msg,
        })
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApplyResult {
                ok: false,
                message: e.to_string(),
            }),
        )
            .into_response(),
    }
}

async fn do_apply() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use std::sync::atomic::Ordering;
    if UPDATE_IN_PROGRESS
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("an update is already in progress".into());
    }
    // Ensure we always clear the flag when this function returns.
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            UPDATE_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _guard = Guard;

    let exe = std::env::current_exe()?;
    let asset = asset_name().ok_or("no published release binary for this platform")?;
    let release = fetch_latest_release()
        .await
        .map_err(|e| friendly_reqwest_error(&e))?;
    let asset_url = &release
        .assets
        .iter()
        .find(|a| a.name == asset)
        .ok_or_else(|| format!("the latest release has no {asset} asset"))?
        .browser_download_url;
    // Defense in depth: even the API-resolved URL must be a GitHub release
    // asset domain before we'll download and execute it.
    let allowed = asset_url.starts_with("https://github.com/")
        || asset_url.starts_with("https://objects.githubusercontent.com/")
        || asset_url.starts_with("https://github-releases.githubusercontent.com/");
    if !allowed {
        return Err("asset_url must be a github.com release asset".into());
    }
    let new_path = exe.with_extension("new");
    let old_path = exe.with_extension("old");

    // No total timeout on this client — a full binary on a slow device link
    // can legitimately take minutes — but cap connect time and stall time so a
    // dead connection can't wedge the update flag forever.
    let client = reqwest::Client::builder()
        .user_agent("amber-dav")
        .connect_timeout(std::time::Duration::from_secs(10))
        .read_timeout(std::time::Duration::from_secs(30))
        .build()?;

    // If the release ships a SHA256SUMS asset, the binary's hash MUST match
    // its entry. Releases that predate the checksum job simply don't have the
    // asset, and fall back to the length + magic checks below.
    let expected_sha = match release.assets.iter().find(|a| a.name == "SHA256SUMS") {
        None => None,
        Some(sums_asset) => {
            let sums = client
                .get(&sums_asset.browser_download_url)
                .send()
                .await
                .map_err(|e| friendly_reqwest_error(&e))?
                .error_for_status()
                .map_err(|e| friendly_reqwest_error(&e))?
                .text()
                .await
                .map_err(|e| friendly_reqwest_error(&e))?;
            Some(
                sha256_entry(&sums, asset)
                    .ok_or_else(|| format!("SHA256SUMS has no entry for {asset}"))?,
            )
        }
    };

    // Stream download to <exe>.new, hashing and counting as we go.
    let resp = client
        .get(asset_url)
        .send()
        .await
        .map_err(|e| friendly_reqwest_error(&e))?
        .error_for_status()
        .map_err(|e| friendly_reqwest_error(&e))?;
    let expected_len = resp.content_length();

    let mut file = tokio::fs::File::create(&new_path).await?;
    let mut stream = resp.bytes_stream();
    let mut hasher = sha2::Sha256::new();
    let mut written: u64 = 0;
    let mut head: Vec<u8> = Vec::with_capacity(4);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| friendly_reqwest_error(&e))?;
        if head.len() < 4 {
            head.extend_from_slice(&chunk[..chunk.len().min(4 - head.len())]);
        }
        hasher.update(&chunk);
        written += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);

    // Verify before touching the running binary; a bad download must never
    // survive to the rename dance. Drop the partial file on rejection.
    let actual_sha: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if let Err(e) = verify_download(
        written,
        expected_len,
        &head,
        current_os(),
        &actual_sha,
        expected_sha.as_deref(),
    ) {
        let _ = std::fs::remove_file(&new_path);
        return Err(e.into());
    }

    // Make the new binary executable (no-op on Windows).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&new_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&new_path, perms)?;
    }

    // Atomic rename dance: current → .old, .new → current.
    // Remove stale .old first — Windows rename fails if the destination exists.
    let _ = std::fs::remove_file(&old_path);
    std::fs::rename(&exe, &old_path)?;
    if let Err(e) = std::fs::rename(&new_path, &exe) {
        // Best-effort rollback: restore the old binary.
        let _ = std::fs::rename(&old_path, &exe);
        let _ = std::fs::remove_file(&new_path);
        return Err(e.into());
    }

    Ok(format!(
        "Update applied. Old binary saved as {}. Restart the app to use the new version.",
        old_path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_is_known_or_none() {
        let _ = asset_name();
    }

    // An SDL build is (sdl=true, fb=false) and must resolve to the dynamic
    // `-sdl` asset. `sdl` is also matched before `fb`, so even a build that
    // enabled both still maps to `-sdl` rather than the static `-fb` one.
    #[test]
    fn sdl_builds_resolve_to_the_sdl_asset() {
        assert_eq!(
            asset_for("aarch64", "linux", true, false),
            Some("amber-dav-aarch64-linux-sdl")
        );
        assert_eq!(
            asset_for("x86_64", "linux", true, false),
            Some("amber-dav-x86_64-linux-sdl")
        );
        // sdl wins when both are enabled.
        assert_eq!(
            asset_for("aarch64", "linux", true, true),
            Some("amber-dav-aarch64-linux-sdl")
        );
    }

    #[test]
    fn fb_without_sdl_resolves_to_the_fb_asset() {
        assert_eq!(
            asset_for("aarch64", "linux", false, true),
            Some("amber-dav-aarch64-linux-fb")
        );
        assert_eq!(
            asset_for("x86_64", "linux", false, true),
            Some("amber-dav-x86_64-linux-fb")
        );
    }

    #[test]
    fn headless_linux_resolves_to_the_plain_asset() {
        assert_eq!(
            asset_for("aarch64", "linux", false, false),
            Some("amber-dav-aarch64-linux")
        );
        assert_eq!(
            asset_for("x86_64", "linux", false, false),
            Some("amber-dav-x86_64-linux")
        );
    }

    #[test]
    fn desktop_targets_ignore_feature_flags_and_unknown_is_none() {
        assert_eq!(
            asset_for("aarch64", "macos", false, false),
            Some("amber-dav-aarch64-macos")
        );
        assert_eq!(
            asset_for("x86_64", "windows", false, false),
            Some("amber-dav-x86_64-windows.exe")
        );
        assert_eq!(asset_for("riscv64", "linux", false, false), None);
        assert_eq!(asset_for("", "", false, false), None);
    }

    #[test]
    fn check_result_serializes() {
        let r = CheckResult {
            current: "1.0.0".into(),
            latest: "1.1.0".into(),
            up_to_date: false,
            dev_build: false,
            asset_url: Some("https://example.com/asset".into()),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("up_to_date"));
        assert!(json.contains("dev_build"));
        assert!(json.contains("asset_url"));
    }

    // The update verdict (issue #46): behind → update; equal and *ahead* →
    // up to date (no downgrade offer, the old string-equality bug); any
    // unstamped 0.0.0 build → flagged dev and never silently updatable.
    #[test]
    fn verdict_handles_release_dev_and_newer_builds() {
        assert_eq!(verdict("1.2.3", "1.3.0"), (false, false)); // behind
        assert_eq!(verdict("1.3.0", "1.3.0"), (true, false)); // current
        assert_eq!(verdict("1.4.0", "1.3.0"), (true, false)); // ahead: no downgrade
        assert_eq!(verdict("1.10.0", "1.9.0"), (true, false)); // numeric compare
        assert_eq!(verdict("0.0.0", "1.3.0"), (false, true)); // tarball dev build
        assert_eq!(
            verdict("0.0.0+v1.3.0-12-gabc-dirty", "1.3.0"),
            (false, true)
        );
    }

    #[test]
    fn rate_limit_statuses_get_a_friendly_message() {
        for status in [403, 429] {
            assert_eq!(
                friendly_http_failure(Some(status), false, "raw"),
                "GitHub rate limit reached — try again later"
            );
        }
    }

    #[test]
    fn other_statuses_name_the_code_and_unreachable_names_github() {
        assert_eq!(
            friendly_http_failure(Some(502), false, "raw"),
            "github.com returned HTTP 502"
        );
        assert_eq!(
            friendly_http_failure(None, true, "raw"),
            "could not reach github.com — check the connection and try again"
        );
        // No status and not a connectivity failure: pass the raw error through.
        assert_eq!(friendly_http_failure(None, false, "boom"), "boom");
    }

    #[test]
    fn executable_magic_is_checked_per_os() {
        assert!(looks_like_executable(b"\x7fELF\x02\x01", "linux"));
        assert!(looks_like_executable(b"MZ\x90\x00", "windows"));
        // Thin 64-bit and fat Mach-O, both endiannesses.
        assert!(looks_like_executable(&[0xcf, 0xfa, 0xed, 0xfe], "macos"));
        assert!(looks_like_executable(&[0xfe, 0xed, 0xfa, 0xcf], "macos"));
        assert!(looks_like_executable(&[0xca, 0xfe, 0xba, 0xbe], "macos"));

        // An HTML error page is not a binary on any platform.
        for os in ["linux", "macos", "windows"] {
            assert!(!looks_like_executable(b"<!DOCTYPE html>", os));
        }
        // Wrong format for the platform is rejected too.
        assert!(!looks_like_executable(b"MZ\x90\x00", "linux"));
        assert!(!looks_like_executable(b"\x7fELF", "macos"));
        // Truncated/empty downloads never match, nor does an unknown OS.
        assert!(!looks_like_executable(b"\x7fEL", "linux"));
        assert!(!looks_like_executable(b"", "windows"));
        assert!(!looks_like_executable(b"\x7fELF", ""));
    }

    #[test]
    fn sha256_entry_parses_sha256sum_output() {
        let h = "a".repeat(64);
        let sums = format!(
            "{h}  amber-dav-aarch64-linux\n{}  *amber-dav-x86_64-windows.exe\n",
            "B".repeat(64)
        );
        assert_eq!(sha256_entry(&sums, "amber-dav-aarch64-linux"), Some(h));
        // Binary-mode '*' prefix is stripped, and hashes are lowercased.
        assert_eq!(
            sha256_entry(&sums, "amber-dav-x86_64-windows.exe"),
            Some("b".repeat(64))
        );
        assert_eq!(sha256_entry(&sums, "amber-dav-x86_64-linux"), None);
        // Malformed lines (wrong hash length, missing name) are ignored.
        assert_eq!(
            sha256_entry("deadbeef  amber-dav-x86_64-linux", "amber-dav-x86_64-linux"),
            None
        );
        assert_eq!(sha256_entry("just-one-token", "just-one-token"), None);
    }

    #[test]
    fn verify_download_rejects_each_failure_mode() {
        let elf = b"\x7fELF";
        let sha = "a".repeat(64);
        // Happy path: length, magic, and checksum all line up.
        assert!(verify_download(100, Some(100), elf, "linux", &sha, Some(&sha)).is_ok());
        // No Content-Length and no SHA256SUMS still passes on magic alone.
        assert!(verify_download(100, None, elf, "linux", &sha, None).is_ok());

        // Truncated download.
        let err = verify_download(50, Some(100), elf, "linux", &sha, Some(&sha)).unwrap_err();
        assert!(err.contains("50 of 100"), "{err}");
        // Wrong content (e.g. an error page).
        assert!(verify_download(100, Some(100), b"<!DO", "linux", &sha, None).is_err());
        // Checksum mismatch.
        let other = "b".repeat(64);
        let err = verify_download(100, Some(100), elf, "linux", &sha, Some(&other)).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{err}");
    }

    #[test]
    fn build_update_paths_correct() {
        let exe = std::path::PathBuf::from("/some/dir/amber-dav");
        let new_path = exe.with_extension("new");
        let old_path = exe.with_extension("old");
        assert_eq!(
            new_path,
            std::path::PathBuf::from("/some/dir/amber-dav.new")
        );
        assert_eq!(
            old_path,
            std::path::PathBuf::from("/some/dir/amber-dav.old")
        );
    }
}
