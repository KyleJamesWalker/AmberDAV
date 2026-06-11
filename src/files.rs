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

fn conflict(msg: String) -> Response {
    (StatusCode::CONFLICT, msg).into_response()
}

fn io_err(e: std::io::Error) -> Response {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        std::io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, e.to_string()).into_response()
}

/// True when `a` and `b` are the same directory entry (same device + inode).
/// Used by [`rename`] to let case-only renames through on case-insensitive
/// filesystems, where looking up the new name "finds" the source itself.
#[cfg(unix)]
async fn same_entry(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (
        tokio::fs::symlink_metadata(a).await,
        tokio::fs::symlink_metadata(b).await,
    ) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
async fn same_entry(_a: &Path, _b: &Path) -> bool {
    false
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
    // Renaming to the current name is a no-op (the client filters this out,
    // but keep the server honest rather than reporting a self-collision).
    if std::ffi::OsStr::new(name) == fname {
        return ok();
    }
    let (src, dst) = (parent.join(fname), parent.join(name));
    // `tokio::fs::rename` silently clobbers an existing destination, so a
    // rename to a taken name must not destroy that entry (issue #23). There
    // is no overwrite escape hatch here — the client surfaces the 409 and the
    // user picks a different name. Same-entry pairs are allowed so case-only
    // renames still work on case-insensitive filesystems (e.g. macOS), where
    // the destination lookup finds the source itself.
    if tokio::fs::symlink_metadata(&dst).await.is_ok() && !same_entry(&src, &dst).await {
        return conflict(format!("already exists: {name}"));
    }
    match tokio::fs::rename(src, dst).await {
        Ok(()) => ok(),
        Err(e) => io_err(e),
    }
}

// --- move (cut/paste) / copy ------------------------------------------------

#[derive(Deserialize)]
pub struct TransferBody {
    srcs: Vec<String>,
    dest: String,
    /// Replace existing destination entries instead of failing with 409.
    #[serde(default)]
    overwrite: bool,
}

/// Why [`plan_transfer`] refused a move/copy request; the handlers map each
/// variant onto an HTTP response.
#[derive(Debug)]
enum PlanError {
    /// The request can never succeed (bad path, same file, folder into
    /// itself) — `overwrite` does not help. 400.
    Bad(String),
    /// Destination names already exist; a retry with `overwrite=true` will
    /// replace them. 409.
    Conflict(String),
    Io(std::io::Error),
}

impl PlanError {
    fn into_response(self) -> Response {
        match self {
            PlanError::Bad(m) => (StatusCode::BAD_REQUEST, m).into_response(),
            PlanError::Conflict(m) => conflict(m),
            PlanError::Io(e) => io_err(e),
        }
    }
}

/// Resolve and validate a move/copy request into concrete `(src, dst)` jobs
/// **before** any filesystem mutation, so a rejected batch leaves the tree
/// untouched and a client retry with `overwrite=true` simply redoes the whole
/// batch. Guards against silent data loss (issue #23):
///
/// - same file: `std::fs::copy(x, x)` truncates `x` to zero bytes, so a copy
///   onto itself is rejected outright — `overwrite` does not bypass this, and
///   the comparison runs on canonicalized paths so a symlink alias of the
///   source is caught too. A *move* onto itself is a harmless no-op and is
///   skipped instead.
/// - folder into itself: copying `d` into `d` would recurse forever
///   (`d/d/d/…`); moving it there can never succeed. Rejected.
/// - collisions: an existing destination entry is only replaced when the
///   client explicitly passed `overwrite=true`; otherwise the batch fails
///   with 409 naming every conflicting entry.
///
/// `copy_mode` mirrors how the operation treats the source leaf: copy reads
/// *through* a symlinked source (confine the full path), move renames the
/// leaf entry as-is (confine only the parent).
async fn plan_transfer(
    root: &Path,
    srcs: &[String],
    dest: &str,
    overwrite: bool,
    copy_mode: bool,
) -> Result<Vec<(PathBuf, PathBuf)>, PlanError> {
    let invalid = |m: &str| PlanError::Bad(m.to_string());
    let destdir = resolve(root, dest).ok_or_else(|| invalid("invalid destination"))?;
    let destdir = confine(root, &destdir).await.map_err(PlanError::Io)?;
    let mut jobs = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    for src in srcs {
        let sp = resolve(root, src).ok_or_else(|| invalid("invalid source"))?;
        // The destination entry keeps the name the user selected, not a link
        // target's.
        let Some(fname) = sp.file_name().map(std::ffi::OsStr::to_os_string) else {
            return Err(invalid("invalid source"));
        };
        let sp = if copy_mode {
            confine(root, &sp).await.map_err(PlanError::Io)?
        } else {
            let Some(parent) = sp.parent() else {
                return Err(invalid("invalid source"));
            };
            confine(root, parent)
                .await
                .map_err(PlanError::Io)?
                .join(&fname)
        };
        let dst = destdir.join(&fname);
        let name = fname.to_string_lossy();
        if dst == sp {
            if copy_mode {
                return Err(PlanError::Bad(format!(
                    "source and destination are the same: {name}"
                )));
            }
            // Moving an entry into the folder it is already in: nothing to do.
            continue;
        }
        if dst.starts_with(&sp) {
            let verb = if copy_mode { "copy" } else { "move" };
            return Err(PlanError::Bad(format!(
                "cannot {verb} a folder into itself: {name}"
            )));
        }
        if tokio::fs::symlink_metadata(&dst).await.is_ok() {
            // An entry with this name already exists at the destination. If
            // it is the source itself behind a symlink, overwriting it would
            // still copy the file onto itself — reject exactly like
            // `dst == sp` above instead of treating it as a collision.
            if copy_mode {
                if let Ok(real) = tokio::fs::canonicalize(&dst).await {
                    if real == sp {
                        return Err(PlanError::Bad(format!(
                            "source and destination are the same: {name}"
                        )));
                    }
                }
            }
            if !overwrite {
                conflicts.push(name.into_owned());
                continue;
            }
        }
        jobs.push((sp, dst));
    }
    if !conflicts.is_empty() {
        return Err(PlanError::Conflict(format!(
            "already exists: {}",
            conflicts.join(", ")
        )));
    }
    Ok(jobs)
}

pub async fn move_(_: Session, State(s): State<AppState>, Json(b): Json<TransferBody>) -> Response {
    if !s.permission().can_write() {
        return forbidden();
    }
    let jobs = match plan_transfer(&s.root, &b.srcs, &b.dest, b.overwrite, false).await {
        Ok(jobs) => jobs,
        Err(e) => return e.into_response(),
    };
    for (sp, dst) in jobs {
        if let Err(e) = tokio::fs::rename(&sp, &dst).await {
            return io_err(e);
        }
    }
    ok()
}

pub async fn copy(_: Session, State(s): State<AppState>, Json(b): Json<TransferBody>) -> Response {
    if !s.permission().can_write() {
        return forbidden();
    }
    let jobs = match plan_transfer(&s.root, &b.srcs, &b.dest, b.overwrite, true).await {
        Ok(jobs) => jobs,
        Err(e) => return e.into_response(),
    };
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
    /// Replace an existing file instead of failing with 409.
    #[serde(default)]
    overwrite: bool,
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
    // `File::create` would silently truncate an existing file (issue #23):
    // dropping `config.json` onto a folder that already has one must not
    // destroy the old file without consent. Without `overwrite`, demand a
    // brand-new entry (`create_new` = O_EXCL, which also refuses to write
    // through a pre-planted symlink) and let the client turn the resulting
    // 409 into an overwrite prompt.
    let target = dir.join(name);
    let file = if q.overwrite {
        tokio::fs::File::create(&target).await
    } else {
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .await
    };
    let mut file = match file {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return conflict(format!("already exists: {name}"));
        }
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

    // Issue #23 §1: pasting a copied file back into its own folder used to
    // run `std::fs::copy(A/x, A/x)`, which truncates the file to zero bytes.
    // The plan must reject it before anything touches the disk — and
    // `overwrite` must not bypass the guard.
    #[tokio::test]
    async fn plan_rejects_copy_onto_self() {
        let tree = TmpTree::new("copy-self");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("x.txt"), b"payload").unwrap();

        for overwrite in [false, true] {
            let err = plan_transfer(&root, &["x.txt".into()], "", overwrite, true)
                .await
                .unwrap_err();
            assert!(
                matches!(err, PlanError::Bad(ref m) if m.contains("same")),
                "want same-file rejection, got {err:?}"
            );
        }
        assert_eq!(std::fs::read(root.join("x.txt")).unwrap(), b"payload");
    }

    // The same truncation reached through an alias: the destination entry is
    // a symlink pointing back at the source file. Canonical comparison must
    // catch it instead of offering an overwrite that would zero the file.
    #[cfg(unix)]
    #[tokio::test]
    async fn plan_rejects_copy_onto_self_via_symlink() {
        let tree = TmpTree::new("copy-self-link");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("x.txt"), b"payload").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::os::unix::fs::symlink(root.join("x.txt"), root.join("sub/x.txt")).unwrap();

        let err = plan_transfer(&root, &["x.txt".into()], "sub", true, true)
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::Bad(ref m) if m.contains("same")));
        assert_eq!(std::fs::read(root.join("x.txt")).unwrap(), b"payload");
    }

    // Cut-pasting an entry into the folder it already lives in is harmless;
    // it plans to nothing instead of erroring or clobbering.
    #[tokio::test]
    async fn plan_skips_move_onto_self() {
        let tree = TmpTree::new("move-self");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("x.txt"), b"payload").unwrap();

        let jobs = plan_transfer(&root, &["x.txt".into()], "", false, false)
            .await
            .unwrap();
        assert!(jobs.is_empty());
        assert_eq!(std::fs::read(root.join("x.txt")).unwrap(), b"payload");
    }

    // Copying or moving a folder into itself can never finish (the copy
    // would recurse into its own output forever).
    #[tokio::test]
    async fn plan_rejects_folder_into_itself() {
        let tree = TmpTree::new("dir-into-self");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::create_dir(root.join("d")).unwrap();

        for copy_mode in [false, true] {
            let err = plan_transfer(&root, &["d".into()], "d", false, copy_mode)
                .await
                .unwrap_err();
            assert!(
                matches!(err, PlanError::Bad(ref m) if m.contains("into itself")),
                "want into-itself rejection, got {err:?}"
            );
        }
    }

    // Issue #23 §2: a name collision at the destination must come back as a
    // conflict naming the entry — only an explicit overwrite replaces it.
    #[tokio::test]
    async fn plan_collision_requires_overwrite() {
        let tree = TmpTree::new("collision");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::fs::write(root.join("a/x.txt"), b"new").unwrap();
        std::fs::write(root.join("b/x.txt"), b"old").unwrap();

        // Both copy and move refuse to clobber without consent…
        for copy_mode in [false, true] {
            let err = plan_transfer(&root, &["a/x.txt".into()], "b", false, copy_mode)
                .await
                .unwrap_err();
            assert!(
                matches!(err, PlanError::Conflict(ref m) if m.contains("x.txt")),
                "want conflict, got {err:?}"
            );
        }
        assert_eq!(std::fs::read(root.join("b/x.txt")).unwrap(), b"old");

        // …and `overwrite=true` plans the very jobs the handler then runs.
        let jobs = plan_transfer(&root, &["a/x.txt".into()], "b", true, true)
            .await
            .unwrap();
        assert_eq!(jobs, vec![(root.join("a/x.txt"), root.join("b/x.txt"))]);
        for (src, dst) in &jobs {
            copy_recursive(src, dst).unwrap();
        }
        assert_eq!(std::fs::read(root.join("b/x.txt")).unwrap(), b"new");
    }

    // A clean transfer (no collision, distinct paths) plans one job per
    // source and reports a missing destination folder as an I/O error.
    #[tokio::test]
    async fn plan_passes_clean_transfers() {
        let tree = TmpTree::new("clean");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::create_dir(root.join("dst")).unwrap();
        std::fs::write(root.join("x.txt"), b"payload").unwrap();

        let jobs = plan_transfer(&root, &["x.txt".into()], "dst", false, false)
            .await
            .unwrap();
        assert_eq!(jobs, vec![(root.join("x.txt"), root.join("dst/x.txt"))]);

        let err = plan_transfer(&root, &["x.txt".into()], "missing", false, false)
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::Io(_)));
    }

    // `rename` 409s on a taken name via `same_entry`: distinct files collide,
    // while the source compared against itself (what a case-only rename sees
    // on a case-insensitive filesystem) does not.
    #[cfg(unix)]
    #[tokio::test]
    async fn same_entry_separates_collisions_from_case_renames() {
        let tree = TmpTree::new("same-entry");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::write(root.join("b.txt"), b"b").unwrap();

        assert!(same_entry(&root.join("a.txt"), &root.join("a.txt")).await);
        assert!(!same_entry(&root.join("a.txt"), &root.join("b.txt")).await);
        // Nonexistent destination: not the same entry (and not a collision —
        // the metadata probe in `rename` fails first).
        assert!(!same_entry(&root.join("a.txt"), &root.join("missing")).await);
    }
}
