//! JSON file-manager API backing the web UI. All paths are relative to the
//! served root; traversal is rejected ([`resolve`]/[`safe_name`] validate the
//! segments, [`confine`] keeps symlinks from escaping). Every handler
//! requires a session.

use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::{auth::Session, AppState};

/// True when `seg` is a plain file name: no backslash or NUL, and it parses
/// as exactly one `Normal` path component on this OS. The component check is
/// what keeps `PathBuf::push` honest on Windows, where pushing a segment with
/// a prefix (`C:`, `\\?\C:\x`) or a root *replaces* the base path instead of
/// appending — handing out the whole drive. Rooted segments (`/etc`) and
/// `.`/`..` parse as non-`Normal` components and are rejected the same way.
fn plain_segment(seg: &str) -> bool {
    if seg.contains('\\') || seg.contains('\0') {
        return false;
    }
    let mut comps = Path::new(seg).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(std::path::Component::Normal(_)), None)
    )
}

/// Resolve a request path (relative to root, `/`-separated) to an absolute
/// path lexically inside `root`: every segment must be a plain name, so no
/// `..`, no rooted or drive-letter segments, no separators. This is a purely
/// textual check — symlink containment is enforced separately by [`confine`].
fn resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for seg in rel.split('/') {
        match seg {
            "" | "." => continue,
            s if plain_segment(s) => out.push(s),
            _ => return None,
        }
    }
    Some(out)
}

/// Validate a single new file/dir name (no path separators or traversal).
fn safe_name(name: &str) -> Option<&str> {
    plain_segment(name).then_some(name)
}

/// Canonicalize `path` and require the result to stay within `root`, which
/// `main` canonicalized once at startup so the comparison is apples-to-apples
/// (e.g. `/var` vs `/private/var` on macOS). [`resolve`] only validates the
/// textual segments; this is what stops a symlink inside the tree
/// (`link -> /etc`) from escaping it. For operations that create or act on a
/// leaf that must not be followed, confine the parent and re-join the name.
async fn confine(root: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let real = tokio::fs::canonicalize(path).await?;
    if real.starts_with(root) {
        Ok(real)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path escapes the served root",
        ))
    }
}

fn ok() -> Response {
    Json(serde_json::json!({ "ok": true })).into_response()
}

fn bad(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "operation not permitted".to_string()).into_response()
}

fn io_err(e: std::io::Error) -> Response {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, e.to_string()).into_response()
}

// --- list -------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PathQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    dir: bool,
    size: u64,
    /// Unix epoch milliseconds (client formats it).
    modified: i64,
}

pub async fn list(_: Session, State(s): State<AppState>, Query(q): Query<PathQuery>) -> Response {
    let rel = q.path.unwrap_or_default();
    let Some(dir) = resolve(&s.root, &rel) else {
        return bad("invalid path");
    };
    let dir = match confine(&s.root, &dir).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };

    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) => return io_err(e),
    };

    let mut entries = Vec::new();
    while let Ok(Some(ent)) = rd.next_entry().await {
        let Ok(meta) = ent.metadata().await else {
            continue;
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        entries.push(Entry {
            name: ent.file_name().to_string_lossy().into_owned(),
            dir: meta.is_dir(),
            size: meta.len(),
            modified,
        });
    }

    // Folders first, then case-insensitive by name.
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Json(entries).into_response()
}

// --- mkdir / delete / rename ------------------------------------------------

#[derive(Deserialize)]
pub struct MkdirBody {
    path: String,
    name: String,
}

pub async fn mkdir(_: Session, State(s): State<AppState>, Json(b): Json<MkdirBody>) -> Response {
    if !s.permission().can_write() {
        return forbidden();
    }
    let (Some(name), Some(parent)) = (safe_name(&b.name), resolve(&s.root, &b.path)) else {
        return bad("invalid name or path");
    };
    let parent = match confine(&s.root, &parent).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };
    match tokio::fs::create_dir(parent.join(name)).await {
        Ok(()) => ok(),
        Err(e) => io_err(e),
    }
}

#[derive(Deserialize)]
pub struct PathsBody {
    paths: Vec<String>,
}

pub async fn delete(_: Session, State(s): State<AppState>, Json(b): Json<PathsBody>) -> Response {
    if !s.permission().can_delete() {
        return forbidden();
    }
    for p in &b.paths {
        let Some(target) = resolve(&s.root, p) else {
            return bad("invalid path");
        };
        if target == *s.root {
            return bad("refusing to delete the root");
        }
        // Confine the parent, not the target: the leaf is removed as an entry
        // (a symlink is deleted, never followed), but the directories leading
        // to it must not escape the root through a symlink.
        let (Some(parent), Some(fname)) = (target.parent(), target.file_name()) else {
            return bad("invalid path");
        };
        let target = match confine(&s.root, parent).await {
            Ok(p) => p.join(fname),
            Err(e) => return io_err(e),
        };
        let meta = match tokio::fs::symlink_metadata(&target).await {
            Ok(m) => m,
            Err(e) => return io_err(e),
        };
        let res = if meta.is_dir() {
            tokio::fs::remove_dir_all(&target).await
        } else {
            tokio::fs::remove_file(&target).await
        };
        if let Err(e) = res {
            return io_err(e);
        }
    }
    ok()
}

#[derive(Deserialize)]
pub struct RenameBody {
    path: String,
    name: String,
}

pub async fn rename(_: Session, State(s): State<AppState>, Json(b): Json<RenameBody>) -> Response {
    if !s.permission().can_write() {
        return forbidden();
    }
    let (Some(name), Some(src)) = (safe_name(&b.name), resolve(&s.root, &b.path)) else {
        return bad("invalid name or path");
    };
    let (Some(parent), Some(fname)) = (src.parent(), src.file_name()) else {
        return bad("path has no parent");
    };
    // Confine the parent: rename acts on the leaf entry itself (a symlink is
    // renamed, never followed), and the new name stays in the same directory.
    let parent = match confine(&s.root, parent).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };
    match tokio::fs::rename(parent.join(fname), parent.join(name)).await {
        Ok(()) => ok(),
        Err(e) => io_err(e),
    }
}

// --- move (cut/paste) / copy ------------------------------------------------

#[derive(Deserialize)]
pub struct TransferBody {
    srcs: Vec<String>,
    dest: String,
}

pub async fn move_(_: Session, State(s): State<AppState>, Json(b): Json<TransferBody>) -> Response {
    if !s.permission().can_write() {
        return forbidden();
    }
    let Some(destdir) = resolve(&s.root, &b.dest) else {
        return bad("invalid destination");
    };
    let destdir = match confine(&s.root, &destdir).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };
    for src in &b.srcs {
        let Some(sp) = resolve(&s.root, src) else {
            return bad("invalid source");
        };
        // Confine the source's parent: rename moves the leaf entry itself
        // (a symlink is moved as a link, never followed).
        let (Some(parent), Some(fname)) = (sp.parent(), sp.file_name()) else {
            return bad("invalid source");
        };
        let sp = match confine(&s.root, parent).await {
            Ok(p) => p.join(fname),
            Err(e) => return io_err(e),
        };
        if let Err(e) = tokio::fs::rename(&sp, destdir.join(fname)).await {
            return io_err(e);
        }
    }
    ok()
}

pub async fn copy(_: Session, State(s): State<AppState>, Json(b): Json<TransferBody>) -> Response {
    if !s.permission().can_write() {
        return forbidden();
    }
    let Some(destdir) = resolve(&s.root, &b.dest) else {
        return bad("invalid destination");
    };
    let destdir = match confine(&s.root, &destdir).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };
    let mut jobs = Vec::new();
    for src in &b.srcs {
        let Some(sp) = resolve(&s.root, src) else {
            return bad("invalid source");
        };
        // The copy reads through the leaf (a symlinked file copies its
        // target's bytes), so confine the full source path. The destination
        // entry keeps the name the user selected, not the link target's.
        let Some(fname) = sp.file_name().map(std::ffi::OsStr::to_os_string) else {
            return bad("invalid source");
        };
        let sp = match confine(&s.root, &sp).await {
            Ok(p) => p,
            Err(e) => return io_err(e),
        };
        jobs.push((sp, destdir.join(fname)));
    }
    // Recursive copy can be heavy; run it off the async runtime.
    let res = tokio::task::spawn_blocking(move || {
        for (src, dst) in jobs {
            copy_recursive(&src, &dst)?;
        }
        Ok::<(), std::io::Error>(())
    })
    .await;
    match res {
        Ok(Ok(())) => ok(),
        Ok(Err(e)) => io_err(e),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for ent in std::fs::read_dir(src)? {
            let ent = ent?;
            copy_recursive(&ent.path(), &dst.join(ent.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

// --- upload / download ------------------------------------------------------

#[derive(Deserialize)]
pub struct UploadQuery {
    path: String,
    name: String,
}

pub async fn upload(
    _: Session,
    State(s): State<AppState>,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> Response {
    if !s.permission().can_write() {
        return forbidden();
    }
    let (Some(name), Some(dir)) = (safe_name(&q.name), resolve(&s.root, &q.path)) else {
        return bad("invalid name or path");
    };
    let dir = match confine(&s.root, &dir).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };
    let mut file = match tokio::fs::File::create(dir.join(name)).await {
        Ok(f) => f,
        Err(e) => return io_err(e),
    };
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                if let Err(e) = file.write_all(&bytes).await {
                    return io_err(e);
                }
            }
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    }
    match file.flush().await {
        Ok(()) => ok(),
        Err(e) => io_err(e),
    }
}

/// Best-effort content type from a file extension (for inline serving).
fn content_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/aac",
        "txt" | "md" | "cfg" | "ini" | "log" | "conf" | "sh" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "xml" => "text/xml",
        "csv" => "text/csv",
        "html" | "htm" => "text/html; charset=utf-8",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Parse a single-range `Range: bytes=...` header against a known total size.
/// Returns `None` if absent/unparseable (caller serves the whole file), or
/// `Some(Err(()))` if present but unsatisfiable (caller returns 416).
fn parse_range(h: &str, total: u64) -> Option<Result<(u64, u64), ()>> {
    let (a, b) = h.strip_prefix("bytes=")?.split_once('-')?;
    let (start, end) = if a.is_empty() {
        // suffix form: last N bytes
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return Some(Err(()));
        }
        (total.saturating_sub(n), total.saturating_sub(1))
    } else {
        let start: u64 = a.parse().ok()?;
        let end = if b.is_empty() {
            total.saturating_sub(1)
        } else {
            b.parse::<u64>().ok()?.min(total.saturating_sub(1))
        };
        (start, end)
    };
    if total == 0 || start > end || start >= total {
        return Some(Err(()));
    }
    Some(Ok((start, end)))
}

/// Serve a file inline (for thumbnails and previews), honoring Range requests
/// so the browser can seek video/audio.
pub async fn raw(
    _: Session,
    State(s): State<AppState>,
    Query(q): Query<PathQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    let rel = q.path.unwrap_or_default();
    let Some(path) = resolve(&s.root, &rel) else {
        return bad("invalid path");
    };
    let path = match confine(&s.root, &path).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) => return io_err(e),
    };
    if meta.is_dir() {
        return bad("not a file");
    }
    let total = meta.len();
    let ct = path
        .file_name()
        .map(|n| content_type(&n.to_string_lossy()))
        .unwrap_or("application/octet-stream");

    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_range(h, total));

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return io_err(e),
    };

    match range {
        Some(Ok((start, end))) => {
            if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
                return (StatusCode::INTERNAL_SERVER_ERROR, "seek failed").into_response();
            }
            let len = end - start + 1;
            let body = Body::from_stream(tokio_util::io::ReaderStream::new(file.take(len)));
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, ct.to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {start}-{end}/{total}"),
                    ),
                    (header::CONTENT_LENGTH, len.to_string()),
                ],
                body,
            )
                .into_response()
        }
        Some(Err(())) => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{total}"))],
        )
            .into_response(),
        None => {
            let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, ct.to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_LENGTH, total.to_string()),
                ],
                body,
            )
                .into_response()
        }
    }
}

// --- zip (download multiple items / folders) -------------------------------

#[derive(Deserialize)]
pub struct ZipQuery {
    /// base64(JSON array of relative paths). Encoded so it survives a GET and
    /// handles arbitrary filenames; a GET lets the browser stream to disk.
    p: String,
}

pub async fn zip(_: Session, State(s): State<AppState>, Query(q): Query<ZipQuery>) -> Response {
    use base64::Engine;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(q.p.as_bytes()) else {
        return bad("invalid selection");
    };
    let Ok(rels) = serde_json::from_slice::<Vec<String>>(&bytes) else {
        return bad("invalid selection");
    };

    // Resolve each selection to (abs path, top-level entry name).
    let mut roots: Vec<(PathBuf, String)> = Vec::new();
    for rel in &rels {
        let Some(abs) = resolve(&s.root, rel) else {
            return bad("invalid path");
        };
        // The archive entry keeps the selected name; the path it reads from
        // is confined (the zip reads through symlinks, so the full path is
        // canonicalized).
        let Some(name) = abs.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return bad("invalid path");
        };
        let abs = match confine(&s.root, &abs).await {
            Ok(p) => p,
            Err(e) => return io_err(e),
        };
        roots.push((abs, name));
    }
    if roots.is_empty() {
        return bad("nothing selected");
    }

    // Stream the archive: a writer task fills one end of a pipe, the response
    // reads the other. Memory stays bounded to the pipe buffer.
    let (writer, reader) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Err(e) = build_zip(writer, roots).await {
            eprintln!("zip: aborted: {e}");
        }
    });
    let stream = tokio_util::io::ReaderStream::new(reader);
    (
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"amber-dav.zip\"".to_string(),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn build_zip(
    writer: tokio::io::DuplexStream,
    roots: Vec<(PathBuf, String)>,
) -> std::io::Result<()> {
    use async_zip::base::write::ZipFileWriter;
    use async_zip::{Compression, ZipEntryBuilder};
    use tokio_util::compat::TokioAsyncReadCompatExt;

    let to_io = |e: async_zip::error::ZipError| std::io::Error::other(e.to_string());

    let mut zw = ZipFileWriter::with_tokio(writer);
    // Walk the selection iteratively (a stack avoids boxed async recursion).
    let mut stack = roots;
    while let Some((abs, name)) = stack.pop() {
        let meta = tokio::fs::metadata(&abs).await?;
        if meta.is_dir() {
            let mut rd = tokio::fs::read_dir(&abs).await?;
            while let Some(ent) = rd.next_entry().await? {
                let child = ent.file_name().to_string_lossy().into_owned();
                stack.push((ent.path(), format!("{name}/{child}")));
            }
        } else {
            // Stored (no compression): fast on the handheld's CPU, and most
            // content here (images, archives, ROMs) is already compressed.
            let entry = ZipEntryBuilder::new(name.into(), Compression::Stored);
            let mut ew = zw.write_entry_stream(entry).await.map_err(to_io)?;
            let mut f = tokio::fs::File::open(&abs).await?.compat();
            futures_util::io::copy(&mut f, &mut ew).await?;
            ew.close().await.map_err(to_io)?;
        }
    }
    zw.close().await.map_err(to_io)?;
    Ok(())
}

pub async fn download(
    _: Session,
    State(s): State<AppState>,
    Query(q): Query<PathQuery>,
) -> Response {
    let rel = q.path.unwrap_or_default();
    let Some(path) = resolve(&s.root, &rel) else {
        return bad("invalid path");
    };
    // Suggested filename comes from the requested path (what the UI shows),
    // while the bytes are read from the confined, canonical path.
    let fname = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let path = match confine(&s.root, &path).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return io_err(e),
    };
    let stream = tokio_util::io::ReaderStream::new(file);
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{}\"",
                    fname.replace(['"', '\\'], "")
                ),
            ),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_blocks_traversal() {
        let root = Path::new("/srv/root");
        assert_eq!(resolve(root, "a/b"), Some(PathBuf::from("/srv/root/a/b")));
        assert_eq!(
            resolve(root, "/a/./b/"),
            Some(PathBuf::from("/srv/root/a/b"))
        );
        assert_eq!(resolve(root, "a/../../etc"), None);
        assert_eq!(resolve(root, "../etc/passwd"), None);
        assert_eq!(resolve(root, ""), Some(root.to_path_buf()));
    }

    #[test]
    fn resolve_blocks_windows_style_escapes() {
        let root = Path::new("/srv/root");
        // Backslash forms are rejected on every OS: drive-letter paths,
        // `\\?\` verbatim prefixes, and backslash-rooted segments.
        assert_eq!(resolve(root, "C:\\foo"), None);
        assert_eq!(resolve(root, "\\\\?\\C:\\x"), None);
        assert_eq!(resolve(root, "\\etc"), None);
        assert_eq!(resolve(root, "a\\b"), None);
        // NUL never makes a valid name.
        assert_eq!(resolve(root, "a\0b"), None);
        // A bare drive-letter segment parses as a Prefix component on
        // Windows, where `PathBuf::push` would REPLACE the root with it
        // (`GET /api/list?path=C:` serving the whole drive). On Unix `C:`
        // is an ordinary file name and stays inside the root.
        #[cfg(windows)]
        assert_eq!(resolve(root, "C:"), None);
        #[cfg(unix)]
        assert_eq!(resolve(root, "C:"), Some(PathBuf::from("/srv/root/C:")));
        // `/`-rooted input cannot escape: paths are split on `/`, so the
        // empty leading segment is skipped and `etc` lands under the root.
        assert_eq!(resolve(root, "/etc"), Some(PathBuf::from("/srv/root/etc")));
    }

    #[test]
    fn range_parsing() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some(Ok((0, 99))));
        assert_eq!(parse_range("bytes=100-", 1000), Some(Ok((100, 999))));
        assert_eq!(parse_range("bytes=-200", 1000), Some(Ok((800, 999))));
        // end clamped to last byte
        assert_eq!(parse_range("bytes=0-99999", 1000), Some(Ok((0, 999))));
        // unsatisfiable: start past end of file
        assert_eq!(parse_range("bytes=2000-3000", 1000), Some(Err(())));
        // not a byte range we handle -> serve whole file
        assert_eq!(parse_range("items=0-1", 1000), None);
        assert_eq!(parse_range("bytes=abc", 1000), None);
    }

    #[test]
    fn safe_name_rejects_separators() {
        assert!(safe_name("photo.png").is_some());
        assert!(safe_name("../x").is_none());
        assert!(safe_name("a/b").is_none());
        assert!(safe_name("").is_none());
        assert!(safe_name(".").is_none());
        assert!(safe_name("..").is_none());
        assert!(safe_name("a\\b").is_none());
        assert!(safe_name("a\0b").is_none());
        assert!(safe_name("/etc").is_none());
        // Drive-letter names: a Prefix component on Windows (where `join`
        // would replace the base path), a plain file name elsewhere.
        assert!(safe_name("C:\\foo").is_none());
        #[cfg(windows)]
        assert!(safe_name("C:").is_none());
        #[cfg(unix)]
        assert!(safe_name("C:").is_some());
    }

    /// A scratch directory tree that cleans itself up.
    struct TmpTree(PathBuf);

    impl TmpTree {
        fn new(name: &str) -> TmpTree {
            let path = std::env::temp_dir()
                .join(format!("amberdav-files-test-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TmpTree(path)
        }
    }

    impl Drop for TmpTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // The lexical checks in `resolve` cannot see through symlinks; this
    // exercises the canonicalize-and-compare step that actually jails them
    // (issue #21 §2). Symlinks need a real filesystem, hence the tempdir.
    #[cfg(unix)]
    #[tokio::test]
    async fn confine_blocks_symlink_escape() {
        let outside = TmpTree::new("confine-outside");
        let tree = TmpTree::new("confine-root");
        std::fs::write(outside.0.join("secret.txt"), b"top secret").unwrap();
        // Canonicalize the root exactly like main() does, so the comparison
        // is apples-to-apples (/var vs /private/var on macOS).
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.join("link")).unwrap();
        std::fs::write(root.join("ok.txt"), b"fine").unwrap();

        // The escape resolves lexically (no `..`, plain segments) and the
        // target really exists — confine() is the layer that rejects it.
        let escaped = resolve(&root, "link/secret.txt").unwrap();
        assert!(tokio::fs::metadata(&escaped).await.is_ok());
        let err = confine(&root, &escaped).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        // The out-pointing symlink itself is rejected too.
        let link = resolve(&root, "link").unwrap();
        assert!(confine(&root, &link).await.is_err());

        // Plain paths inside the root pass and come back canonicalized…
        let ok = resolve(&root, "ok.txt").unwrap();
        assert_eq!(confine(&root, &ok).await.unwrap(), root.join("ok.txt"));
        // …and a symlink that stays inside the root is still allowed.
        std::os::unix::fs::symlink(root.join("ok.txt"), root.join("inlink")).unwrap();
        let inlink = resolve(&root, "inlink").unwrap();
        assert_eq!(confine(&root, &inlink).await.unwrap(), root.join("ok.txt"));
    }
}
