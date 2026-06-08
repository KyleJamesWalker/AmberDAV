//! In-app update: check the GitHub Releases API and apply a downloaded binary.

use axum::{extract::State, http::StatusCode, response::{IntoResponse, Response}, Json};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{auth::Session, AppState};

/// GitHub repo to check for releases.
const REPO: &str = "KyleJamesWalker/AmberDAV";

/// Returns the release asset name for the current platform, or `None` if the
/// platform is not a known release target.
pub fn asset_name() -> Option<&'static str> {
    if cfg!(all(target_arch = "aarch64", target_os = "linux")) {
        Some("amber-dav-aarch64-linux")
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Some("amber-dav-aarch64-macos")
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        Some("amber-dav-x86_64-macos")
    } else if cfg!(all(target_arch = "x86_64", target_os = "windows")) {
        Some("amber-dav-x86_64-windows.exe")
    } else {
        None
    }
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
pub async fn apply(
    _: Session,
    _: State<AppState>,
    Json(body): Json<ApplyRequest>,
) -> Response {
    match do_apply(&body.asset_url).await {
        Ok(msg) => Json(ApplyResult { ok: true, message: msg }).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApplyResult { ok: false, message: e.to_string() }),
        )
            .into_response(),
    }
}

async fn do_apply(asset_url: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let exe = std::env::current_exe()?;
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
    std::fs::rename(&exe, &old_path)?;
    std::fs::rename(&new_path, &exe)?;

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
        assert_eq!(new_path, std::path::PathBuf::from("/some/dir/amber-dav.new"));
        assert_eq!(old_path, std::path::PathBuf::from("/some/dir/amber-dav.old"));
    }
}
