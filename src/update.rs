//! In-app update: check the GitHub Releases API and apply a downloaded binary.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{auth::Session, AppState};

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
    let os = if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        ""
    };
    asset_for(arch, os, cfg!(feature = "sdl"), cfg!(feature = "fb"))
}

#[derive(Serialize)]
pub struct CheckResult {
    pub current: String,
    pub latest: String,
    pub up_to_date: bool,
    /// Download URL for the matching asset, if one exists for this platform.
    pub asset_url: Option<String>,
}

#[derive(Serialize)]
pub struct ApplyResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Deserialize)]
pub struct ApplyRequest {
    pub asset_url: String,
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

/// GET /api/update/check — compares current version against the latest GitHub release.
pub async fn check(_: Session, _: State<AppState>) -> Response {
    let current = env!("CARGO_PKG_VERSION").to_string();

    match fetch_latest_release().await {
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": e.to_string() })),
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
            Json(CheckResult {
                up_to_date: latest == current,
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
        .build()?
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json::<GhRelease>()
        .await
}

/// POST /api/update/apply — downloads asset_url and replaces the running binary.
/// The running process is NOT restarted; caller must relaunch after this returns Ok.
pub async fn apply(_: Session, _: State<AppState>, Json(body): Json<ApplyRequest>) -> Response {
    match do_apply(&body.asset_url).await {
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

async fn do_apply(asset_url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
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
    // Guard against SSRF: only allow GitHub release asset domains.
    let allowed = asset_url.starts_with("https://github.com/")
        || asset_url.starts_with("https://objects.githubusercontent.com/")
        || asset_url.starts_with("https://github-releases.githubusercontent.com/");
    if !allowed {
        return Err("asset_url must be a github.com release asset".into());
    }
    let new_path = exe.with_extension("new");
    let old_path = exe.with_extension("old");

    // Stream download to <exe>.new
    let resp = reqwest::Client::builder()
        .user_agent("amber-dav")
        .build()?
        .get(asset_url)
        .send()
        .await?
        .error_for_status()?;

    let mut file = tokio::fs::File::create(&new_path).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk?).await?;
    }
    file.flush().await?;
    drop(file);

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
            asset_url: Some("https://example.com/asset".into()),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("up_to_date"));
        assert!(json.contains("asset_url"));
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
