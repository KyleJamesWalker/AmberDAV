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

/// GET /api/update/check
pub async fn check(_: Session, _: State<AppState>) -> Response {
    todo!()
}

/// POST /api/update/apply
pub async fn apply(
    _: Session,
    _: State<AppState>,
    Json(_body): Json<ApplyRequest>,
) -> Response {
    todo!()
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
