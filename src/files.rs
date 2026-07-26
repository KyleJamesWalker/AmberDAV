//! JSON file-manager API backing the web UI. All paths are relative to the
//! served root; traversal is rejected (`state::MountTable::resolve` and
//! [`safe_name`] validate the segments via the shared `state::plain_segment`,
//! [`confine`] keeps symlinks from escaping). Every handler requires a
//! session.

use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::{auth::Session, state::plain_segment, state::AppState};

/// Validate a single new file/dir name (no path separators or traversal).
fn safe_name(name: &str) -> Option<&str> {
    plain_segment(name).then_some(name)
}

/// Canonicalize `path` and require the result to stay within `root`, which
/// `main` canonicalized once at startup so the comparison is apples-to-apples
/// (e.g. `/var` vs `/private/var` on macOS). The mount resolver only validates
/// the textual segments; this is what stops a symlink inside the tree
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

/// Zip-download pipe depth: how far the archive writer may run ahead of the
/// client before it backpressures. 64 KiB keeps memory bounded per download
/// while comfortably covering a typical TCP send window on the device.
const ZIP_PIPE_BUFFER: usize = 64 * 1024;

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

/// Modification time as Unix epoch milliseconds, `0` when the filesystem
/// cannot report one — the shape `/api/list` and `/api/find` hand the client,
/// which does the formatting.
fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub async fn list(_: Session, State(s): State<AppState>, Query(q): Query<PathQuery>) -> Response {
    let rel = q.path.unwrap_or_default();

    // Multi-root virtual root: synthesize a listing of the mount names.
    if s.mounts.is_virtual_root(&rel) {
        let entries: Vec<Entry> = s
            .mounts
            .mounts()
            .iter()
            .map(|(name, _)| Entry {
                name: name.clone(),
                dir: true,
                size: 0,
                modified: 0,
            })
            .collect();
        return Json(entries).into_response();
    }

    let Some((mount_root, dir)) = s.mounts.resolve(&rel) else {
        return bad("invalid path");
    };
    let dir = match confine(mount_root, &dir).await {
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
        entries.push(Entry {
            name: ent.file_name().to_string_lossy().into_owned(),
            dir: meta.is_dir(),
            size: meta.len(),
            modified: mtime_ms(&meta),
        });
    }

    // Deliberately unsorted (issue #58): ordering is the client's job — the
    // web UI's sortView() re-sorts every listing by the user's chosen column
    // anyway (and is this endpoint's only consumer), so a server-side sort
    // was pure waste; the old comparator allocated two lowercase Strings per
    // comparison (~120k transient allocations for a 5,000-entry folder on
    // the A53). Entries arrive in readdir order — assume nothing about it.
    Json(entries).into_response()
}

// --- find -------------------------------------------------------------------

/// Caps on one recursive search. The walk reads an SD card behind a handheld
/// CPU, so it is bounded four ways instead of trusted to finish: the client
/// gets a usable prefix plus `truncated`, never an open-ended stall. Every cap
/// is a stop, not an error — partial results beat a spinner that never ends.
const FIND_MAX_HITS: usize = 500;
const FIND_MAX_SCANNED: usize = 50_000;
const FIND_MAX_DEPTH: usize = 24;
const FIND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Deserialize)]
pub struct FindQuery {
    /// Folder to search under; empty = the served root (in multi-root mode,
    /// every mount).
    path: Option<String>,
    /// The name pattern — see [`name_matches`].
    q: String,
}

/// One match. `parent` is the rel path of the containing folder (`""` = the
/// search root's own level), so `parent/name` is a path every other endpoint
/// already accepts — the UI reuses download/preview/zip on hits unchanged.
#[derive(Serialize)]
struct Hit {
    parent: String,
    name: String,
    dir: bool,
    size: u64,
    /// Unix epoch milliseconds, like [`Entry::modified`].
    modified: i64,
}

#[derive(Serialize)]
struct FindResult {
    hits: Vec<Hit>,
    /// True when a cap stopped the walk: `hits` is a prefix of the matches,
    /// not all of them.
    truncated: bool,
    /// Which cap fired — `"results"`, `"entries"`, `"time"`, or `"depth"`;
    /// `None` when the whole subtree was searched.
    limit: Option<&'static str>,
    /// Directory entries examined, reported so the UI can say how much of the
    /// tree the answer covers.
    scanned: usize,
}

/// True when `name` matches the search pattern `pat`.
///
/// A pattern with no wildcard is a case-insensitive **substring** test —
/// identical to the web UI's in-folder filter, so typing a few letters means
/// the same thing at any depth. A pattern containing `*` or `?` is matched as
/// a **glob against the whole name**, `find -name` style (`*.srm`,
/// `save?.dat`). There is no escape syntax: `*` and `?` are always wildcards,
/// which keeps the rule explainable on a device with no shell. Both modes
/// ignore case — the content here is mixed-case ROM dumps and save files, so
/// `-iname` is what anyone actually wants.
fn name_matches(pat: &str, name: &str) -> bool {
    let pat = pat.to_lowercase();
    let name = name.to_lowercase();
    if pat.contains('*') || pat.contains('?') {
        let pat: Vec<char> = pat.chars().collect();
        let name: Vec<char> = name.chars().collect();
        glob_match(&pat, &name)
    } else {
        name.contains(&pat)
    }
}

/// Whole-string glob: `*` matches any run of characters (including none), `?`
/// exactly one. Backtracking is limited to the most recent `*`, which is all a
/// single-star-class glob needs and keeps a pathological pattern from blowing
/// up the walk. Compares `char`s, not bytes, so a multi-byte name can't be
/// split mid-character.
fn glob_match(pat: &[char], name: &[char]) -> bool {
    let (mut p, mut n) = (0, 0);
    // The last `*` seen and how much of `name` it had consumed — where we
    // resume when a literal run further along turns out not to match.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pat.len() && (pat[p] == '?' || pat[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pat.len() && pat[p] == '*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((sp, sn)) = star {
            // Let the `*` swallow one more character and retry after it.
            p = sp + 1;
            n = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    // Trailing `*`s may match nothing; anything else left over is a miss.
    pat[p..].iter().all(|c| *c == '*')
}

/// Append `name` to a rel folder path (`""` = the root level).
fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// Recursive name search under `path` — the tree-wide counterpart to the web
/// UI's in-folder filter (`find -name`, over HTTP). Matches directories as
/// well as files, and reports each hit's containing folder so the client can
/// act on it or navigate there.
///
/// Entries are described by `DirEntry::metadata`, which does not traverse
/// symlinks — so a hit renders exactly as the same entry does in its folder's
/// `/api/list`, and a symlinked directory is never recursed into. That is
/// `find`'s own default, and it is what keeps a cycle (`a/link -> a`) from
/// looping forever — the trap issue #113 documents for the ZIP walker.
/// Containment therefore needs no per-entry `confine`: the walk only ever
/// descends into real directories below an already-confined root, so it cannot
/// leave the served tree.
pub async fn find(_: Session, State(s): State<AppState>, Query(q): Query<FindQuery>) -> Response {
    let pat = q.q.trim();
    if pat.is_empty() {
        return bad("empty search");
    }
    let rel = q.path.unwrap_or_default();

    // Search roots as (absolute dir, its rel path, depth). The multi-root
    // virtual root has no filesystem path of its own, so a search there fans
    // out across every mount, each rooted at its own name.
    let mut stack: Vec<(PathBuf, String, usize)> = Vec::new();
    if s.mounts.is_virtual_root(&rel) {
        for (name, root) in s.mounts.mounts() {
            stack.push((root.clone(), name.clone(), 0));
        }
    } else {
        let Some((mount_root, dir)) = s.mounts.resolve(&rel) else {
            return bad("invalid path");
        };
        let dir = match confine(mount_root, &dir).await {
            Ok(p) => p,
            Err(e) => return io_err(e),
        };
        // Normalize the request path the way the resolver does (drop empty and
        // `.` segments) so every `parent` we hand back is in the canonical
        // form the client's own paths use.
        let base_rel = rel
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect::<Vec<_>>()
            .join("/");
        stack.push((dir, base_rel, 0));
    }

    let started = std::time::Instant::now();
    let mut hits: Vec<Hit> = Vec::new();
    let mut scanned = 0usize;
    let mut limit: Option<&'static str> = None;

    'walk: while let Some((dir, dir_rel, depth)) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            // An unreadable subdirectory is skipped, not fatal: one bad
            // permission bit mid-tree must not discard the hits found
            // everywhere else.
            Err(_) => continue,
        };
        while let Ok(Some(ent)) = rd.next_entry().await {
            scanned += 1;
            if scanned >= FIND_MAX_SCANNED {
                limit = Some("entries");
                break 'walk;
            }
            if started.elapsed() >= FIND_DEADLINE {
                limit = Some("time");
                break 'walk;
            }
            let name = ent.file_name().to_string_lossy().into_owned();
            // Unreadable entry: skip it, exactly as `list` does.
            let Ok(meta) = ent.metadata().await else {
                continue;
            };
            if name_matches(pat, &name) {
                hits.push(Hit {
                    parent: dir_rel.clone(),
                    name: name.clone(),
                    dir: meta.is_dir(),
                    size: meta.len(),
                    modified: mtime_ms(&meta),
                });
                if hits.len() >= FIND_MAX_HITS {
                    limit = Some("results");
                    break 'walk;
                }
            }
            // `is_dir()` is false for a symlink to a directory (the metadata
            // above does not traverse), which is what stops the recursion at
            // links.
            if meta.is_dir() {
                if depth + 1 > FIND_MAX_DEPTH {
                    // Too deep to follow: flag the answer as partial but keep
                    // searching the rest of the tree.
                    limit = Some("depth");
                    continue;
                }
                stack.push((ent.path(), join_rel(&dir_rel, &name), depth + 1));
            }
        }
    }

    // Deliberately unsorted, like `/api/list` (issue #58): the client re-sorts
    // every listing by the user's chosen column anyway.
    Json(FindResult {
        hits,
        truncated: limit.is_some(),
        limit,
        scanned,
    })
    .into_response()
}

// --- mkdir / delete / rename ------------------------------------------------

#[derive(Deserialize)]
pub struct MkdirBody {
    path: String,
    name: String,
}

pub async fn mkdir(
    session: Session,
    State(s): State<AppState>,
    Json(b): Json<MkdirBody>,
) -> Response {
    if !session.permission.can_write() {
        return forbidden();
    }
    if s.mounts.is_virtual_root(&b.path) {
        return forbidden();
    }
    let Some(name) = safe_name(&b.name) else {
        return bad("invalid name or path");
    };
    let Some((mount_root, parent)) = s.mounts.resolve(&b.path) else {
        return bad("invalid name or path");
    };
    let parent = match confine(mount_root, &parent).await {
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

pub async fn delete(
    session: Session,
    State(s): State<AppState>,
    Json(b): Json<PathsBody>,
) -> Response {
    if !session.permission.can_delete() {
        return forbidden();
    }
    for p in &b.paths {
        if s.mounts.is_virtual_root(p) {
            return bad("refusing to delete the root");
        }
        let Some((mount_root, target)) = s.mounts.resolve(p) else {
            return bad("invalid path");
        };
        if target == mount_root {
            return bad("refusing to delete the root");
        }
        // Confine the parent, not the target: the leaf is removed as an entry
        // (a symlink is deleted, never followed), but the directories leading
        // to it must not escape the root through a symlink.
        let (Some(parent), Some(fname)) = (target.parent(), target.file_name()) else {
            return bad("invalid path");
        };
        let target = match confine(mount_root, parent).await {
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

pub async fn rename(
    session: Session,
    State(s): State<AppState>,
    Json(b): Json<RenameBody>,
) -> Response {
    if !session.permission.can_write() {
        return forbidden();
    }
    if s.mounts.is_virtual_root(&b.path) {
        return forbidden();
    }
    let Some(name) = safe_name(&b.name) else {
        return bad("invalid name or path");
    };
    let Some((mount_root, src)) = s.mounts.resolve(&b.path) else {
        return bad("invalid name or path");
    };
    let (Some(parent), Some(fname)) = (src.parent(), src.file_name()) else {
        return bad("path has no parent");
    };
    // Confine the parent: rename acts on the leaf entry itself (a symlink is
    // renamed, never followed), and the new name stays in the same directory.
    let parent = match confine(mount_root, parent).await {
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
///
/// For multi-root: src and dst may be in different mounts. Each is confined
/// to its own mount root. Cross-mount moves fall back to copy-then-delete in
/// `move_` when `rename(2)` fails with `EXDEV`.
async fn plan_transfer(
    mounts: &crate::state::MountTable,
    srcs: &[String],
    dest: &str,
    overwrite: bool,
    copy_mode: bool,
) -> Result<Vec<(PathBuf, PathBuf)>, PlanError> {
    let invalid = |m: &str| PlanError::Bad(m.to_string());
    let (dest_root, destdir) = mounts
        .resolve(dest)
        .ok_or_else(|| invalid("invalid destination"))?;
    let destdir = confine(dest_root, &destdir).await.map_err(PlanError::Io)?;
    let mut jobs = Vec::new();
    let mut conflicts: Vec<String> = Vec::new();
    for src in srcs {
        let (src_root, sp) = mounts
            .resolve(src)
            .ok_or_else(|| invalid("invalid source"))?;
        // The destination entry keeps the name the user selected, not a link
        // target's.
        let Some(fname) = sp.file_name().map(std::ffi::OsStr::to_os_string) else {
            return Err(invalid("invalid source"));
        };
        let sp = if copy_mode {
            confine(src_root, &sp).await.map_err(PlanError::Io)?
        } else {
            let Some(parent) = sp.parent() else {
                return Err(invalid("invalid source"));
            };
            confine(src_root, parent)
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

pub async fn move_(
    session: Session,
    State(s): State<AppState>,
    Json(b): Json<TransferBody>,
) -> Response {
    if !session.permission.can_write() {
        return forbidden();
    }
    if s.mounts.is_virtual_root(&b.dest) {
        return forbidden();
    }
    let jobs = match plan_transfer(&s.mounts, &b.srcs, &b.dest, b.overwrite, false).await {
        Ok(jobs) => jobs,
        Err(e) => return e.into_response(),
    };
    for (sp, dst) in jobs {
        match tokio::fs::rename(&sp, &dst).await {
            Ok(()) => {}
            Err(e) if cross_device(&e) => {
                if let Err(e) = move_across_devices(&sp, &dst).await {
                    return io_err(e);
                }
            }
            Err(e) => return io_err(e),
        }
    }
    ok()
}

/// Cross-mount / cross-filesystem move fallback: replicate `sp` at `dst` with
/// symlinks preserved as symlinks — exactly what `rename(2)` does on one
/// filesystem; dereferencing would instead materialize the targets' bytes,
/// including content from *outside* the served mounts — then delete the
/// source (directory or file). On copy-success/delete-failure the copy stays
/// in place and the error surfaces: partial state is visible, the original is
/// safe.
async fn move_across_devices(sp: &Path, dst: &Path) -> std::io::Result<()> {
    let res = tokio::task::spawn_blocking({
        let (sp, dst) = (sp.to_path_buf(), dst.to_path_buf());
        move || copy_recursive_links(&sp, &dst)
    })
    .await;
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(e) => return Err(std::io::Error::other(e.to_string())),
    }
    let removed = match tokio::fs::symlink_metadata(sp).await {
        Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(sp).await,
        _ => tokio::fs::remove_file(sp).await,
    };
    if let Err(e) = removed {
        // Copy succeeded; delete failed. Leave the copy.
        tracing::warn!(
            "cross-mount move: copy succeeded but delete of {} failed: {e}",
            sp.display()
        );
        return Err(e);
    }
    Ok(())
}

/// True when `e` is a cross-device rename error (`EXDEV` on Unix).
fn cross_device(e: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(libc::EXDEV)
    }
    #[cfg(not(unix))]
    {
        // On Windows a cross-volume rename surfaces as ErrorCode 17 (ERROR_NOT_SAME_DEVICE).
        e.raw_os_error() == Some(17)
    }
}

pub async fn copy(
    session: Session,
    State(s): State<AppState>,
    Json(b): Json<TransferBody>,
) -> Response {
    if !session.permission.can_write() {
        return forbidden();
    }
    if s.mounts.is_virtual_root(&b.dest) {
        return forbidden();
    }
    let jobs = match plan_transfer(&s.mounts, &b.srcs, &b.dest, b.overwrite, true).await {
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

/// Recursive copy for the `copy` endpoint. File symlinks are dereferenced
/// (`std::fs::copy` follows them) — pasting a link produces an independent
/// copy of the data, and `plan_transfer` already canonicalized + confined the
/// source leaf so the bytes always come from inside the source mount.
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

/// Recursive copy for [`move_across_devices`]: symlinks are recreated as
/// symlinks, never followed. The move path deliberately skips canonicalizing
/// the leaf (so links can be moved as entries), which means following one
/// here could read through a link pointing outside every mount.
fn copy_recursive_links(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = std::fs::read_link(src)?;
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            return std::os::unix::fs::symlink(target, dst);
        }
        // Windows symlinks need per-type calls and a privilege; refusing the
        // move is safer than silently materializing the target's bytes.
        #[cfg(not(unix))]
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("cannot move symlink {} across drives", src.display()),
        ));
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for ent in std::fs::read_dir(src)? {
            let ent = ent?;
            copy_recursive_links(&ent.path(), &dst.join(ent.file_name()))?;
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
    /// Optional folder path *under* `path` (`/`-separated, relative), created
    /// on demand — folder uploads (issue #30) send one of these per file so a
    /// dropped directory tree lands with its structure intact. Unlike `path`,
    /// the directories may not exist yet; [`ensure_subdir`] validates every
    /// segment with the same single-Normal-component rule before creating
    /// anything.
    #[serde(default)]
    dir: Option<String>,
    /// Replace an existing file instead of failing with 409.
    #[serde(default)]
    overwrite: bool,
}

/// Why [`write_upload`]/[`ensure_subdir`] failed; the handler maps each
/// variant onto an HTTP response.
#[derive(Debug)]
enum UploadError {
    /// Invalid `dir` path (traversal, absolute, drive-letter segments). 400.
    Bad(String),
    /// Destination name is taken and the client did not pass `overwrite`. 409.
    Conflict(String),
    /// The request body stream broke mid-upload (dropped connection). 400.
    Stream(String),
    Io(std::io::Error),
}

impl UploadError {
    fn into_response(self) -> Response {
        match self {
            UploadError::Bad(m) => (StatusCode::BAD_REQUEST, m).into_response(),
            UploadError::Conflict(m) => conflict(m),
            UploadError::Stream(m) => (StatusCode::BAD_REQUEST, m).into_response(),
            UploadError::Io(e) => io_err(e),
        }
    }
}

/// Resolve `sub` — a `/`-separated folder path relative to the already
/// confined `base` — creating missing directories one segment at a time, and
/// return the (canonical) leaf directory. This is what lets a folder upload
/// say `dir=GameBoy/Saves` without a mkdir round-trip per level.
///
/// Validation is the same single-`Normal`-component rule as [`resolve`], but
/// *stricter*: every split segment must be a plain name, so `..`, rooted
/// (`/etc`), backslash, drive-letter (`C:` on Windows) and empty segments are
/// all rejected — and they are rejected up front, before any directory is
/// created, so a half-valid path (`good/../evil`) leaves nothing behind.
///
/// Symlink containment matches [`confine`]'s policy elsewhere: a segment that
/// already exists is canonicalized and must stay inside `root` (an in-tree
/// symlinked folder is fine, an out-pointing one is refused before anything
/// is created beyond it); a missing segment is created under the confined
/// parent — never through a link.
async fn ensure_subdir(root: &Path, base: PathBuf, sub: &str) -> Result<PathBuf, UploadError> {
    let segs: Vec<&str> = sub.split('/').collect();
    if !segs.iter().all(|s| plain_segment(s)) {
        return Err(UploadError::Bad("invalid folder path".to_string()));
    }
    let mut cur = base;
    for seg in segs {
        let next = cur.join(seg);
        match tokio::fs::symlink_metadata(&next).await {
            Ok(_) => {
                let real = confine(root, &next).await.map_err(UploadError::Io)?;
                if !tokio::fs::metadata(&real)
                    .await
                    .map_err(UploadError::Io)?
                    .is_dir()
                {
                    return Err(UploadError::Conflict(format!(
                        "already exists and is not a folder: {seg}"
                    )));
                }
                cur = real;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&next)
                    .await
                    .map_err(UploadError::Io)?;
                cur = next;
            }
            Err(e) => return Err(UploadError::Io(e)),
        }
    }
    Ok(cur)
}

/// Unique sibling temp path for an upload of `name` into `dir`: a hidden
/// `.{name}.{pid}.{seq}.part` dotfile, same write-then-rename shape as
/// `connection.rs`/`update.rs`. The per-process sequence number keeps two
/// concurrent uploads of the same file from ever sharing a temp file, and the
/// same-directory placement keeps the final rename atomic (no cross-device
/// copy).
fn upload_temp_path(dir: &Path, name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".{name}.{}.{seq}.part", std::process::id()))
}

/// Stream a request body into `dir/name` without ever truncating an existing
/// file before the bytes have safely arrived (issue #24). Writes to a unique
/// temp file in the same directory, flushes, then renames into place; the
/// temp is removed on every error path, so a dropped Wi-Fi connection
/// mid-upload leaves the original untouched and no `.part` litter behind.
///
/// Overwrite semantics (issue #23) are preserved: without `overwrite` an
/// existing entry — including a pre-planted symlink, hence `symlink_metadata`
/// — is a 409 *before* any bytes stream, and the check is repeated just
/// before the rename. The recheck-then-rename window is the same plan-then-act
/// TOCTOU the rest of the codebase accepts (`rename`, `plan_transfer`) for a
/// single-user LAN server. With `overwrite` the destination is only replaced
/// at rename time — atomically, never truncated up front.
async fn write_upload<S, B, E>(
    dir: &Path,
    name: &str,
    overwrite: bool,
    mut stream: S,
) -> Result<(), UploadError>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let target = dir.join(name);
    if !overwrite && tokio::fs::symlink_metadata(&target).await.is_ok() {
        return Err(UploadError::Conflict(format!("already exists: {name}")));
    }
    let tmp = upload_temp_path(dir, name);
    // `create_new` (O_EXCL) refuses to write through anything pre-planted at
    // the temp path — the same symlink guard the old direct `create_new` on
    // the target gave us, now moved to where the bytes actually land.
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await
        .map_err(UploadError::Io)?;
    let written: Result<(), UploadError> = async {
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| UploadError::Stream(e.to_string()))?;
            file.write_all(bytes.as_ref())
                .await
                .map_err(UploadError::Io)?;
        }
        file.flush().await.map_err(UploadError::Io)
    }
    .await;
    // Close before renaming (Windows cannot rename an open file).
    drop(file);
    let committed = match written {
        Ok(()) => {
            // Recheck the collision: a file that appeared while the bytes
            // streamed must not be clobbered without consent. `rename`
            // replaces the destination *entry* atomically (a symlink is
            // replaced, never followed), so an existing file is swapped
            // whole — never seen partially written.
            if !overwrite && tokio::fs::symlink_metadata(&target).await.is_ok() {
                Err(UploadError::Conflict(format!("already exists: {name}")))
            } else {
                tokio::fs::rename(&tmp, &target)
                    .await
                    .map_err(UploadError::Io)
            }
        }
        Err(e) => Err(e),
    };
    if committed.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    committed
}

pub async fn upload(
    session: Session,
    State(s): State<AppState>,
    Query(q): Query<UploadQuery>,
    body: Body,
) -> Response {
    if s.mounts.is_virtual_root(&q.path) {
        return forbidden();
    }
    if !session.permission.can_write() {
        return forbidden();
    }
    let Some(name) = safe_name(&q.name) else {
        return bad("invalid name or path");
    };
    let Some((mount_root, dir)) = s.mounts.resolve(&q.path) else {
        return bad("invalid name or path");
    };
    let mut dir = match confine(mount_root, &dir).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };
    // Folder uploads carry the file's folder path relative to the upload
    // destination; create it (validated per segment) before streaming.
    if let Some(sub) = q.dir.as_deref().filter(|d| !d.is_empty()) {
        dir = match ensure_subdir(mount_root, dir, sub).await {
            Ok(p) => p,
            Err(e) => return e.into_response(),
        };
    }
    match write_upload(&dir, name, q.overwrite, body.into_data_stream()).await {
        Ok(()) => ok(),
        Err(e) => e.into_response(),
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

// --- cache validators (issue #28) --------------------------------------------

/// (whole seconds, subsec nanos) of the file's mtime since the Unix epoch.
/// `(0, 0)` when the filesystem cannot report one — the validators then never
/// match, which degrades to plain uncached serving rather than wrong 304s.
fn mtime_parts(meta: &std::fs::Metadata) -> (u64, u32) {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs(), d.subsec_nanos()))
        .unwrap_or((0, 0))
}

/// Whole seconds since the Unix epoch — HTTP dates carry no finer resolution,
/// so every date comparison happens at this granularity.
fn unix_secs(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Strong ETag derived from mtime (seconds + nanos) and size — the same
/// identity the thumbnail disk cache is keyed on. Editing a file in place
/// bumps the mtime, so a revalidation after a change always misses. `variant`
/// distinguishes derived representations of the same file (the thumbnail
/// width) so a `/api/thumb` validator can never satisfy an `/api/raw` request.
fn etag_for(meta: &std::fs::Metadata, variant: Option<u32>) -> String {
    let (s, n) = mtime_parts(meta);
    match variant {
        None => format!("\"{s:x}.{n:x}.{:x}\"", meta.len()),
        Some(w) => format!("\"{s:x}.{n:x}.{:x}.t{w:x}\"", meta.len()),
    }
}

/// Evaluate the conditional-request headers (RFC 9110 §13): `true` means the
/// client's cached copy is current and the handler answers `304 Not Modified`
/// without touching the file body. `If-None-Match` wins over
/// `If-Modified-Since` when both are present, per the RFC's precedence rules.
fn not_modified(headers: &HeaderMap, etag: &str, modified: Option<std::time::SystemTime>) -> bool {
    if let Some(inm) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    {
        // Weak comparison: a `W/` prefix on a listed tag is ignored, so a
        // tag that round-tripped through a weakening proxy still matches.
        return inm.trim() == "*"
            || inm.split(',').any(|t| {
                let t = t.trim();
                t.strip_prefix("W/").unwrap_or(t) == etag
            });
    }
    let (Some(ims), Some(modified)) = (
        headers
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok()),
        modified,
    ) else {
        return false;
    };
    let Ok(since) = httpdate::parse_http_date(ims) else {
        return false;
    };
    unix_secs(modified) <= unix_secs(since)
}

/// `If-Range` (RFC 9110 §13.1.5): `true` means a `Range` header may be
/// honored. With no `If-Range` the range always applies; with one, the
/// validator must match the file's *current* state — otherwise the file
/// changed under the client and the full body is served instead of splicing
/// bytes of two different versions together. Weak validators never match.
fn if_range_matches(
    headers: &HeaderMap,
    etag: &str,
    modified: Option<std::time::SystemTime>,
) -> bool {
    let Some(v) = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let v = v.trim();
    if v.starts_with('"') {
        return v == etag;
    }
    if v.starts_with("W/") {
        return false;
    }
    match (httpdate::parse_http_date(v), modified) {
        (Ok(at), Some(m)) => unix_secs(m) == unix_secs(at),
        _ => false,
    }
}

/// Attach the cache validators to a response. `Cache-Control: private`
/// because every `/api/*` response is session-gated — a shared cache must
/// never store one — while the browser is free to keep it and revalidate
/// with a conditional request (the 304 path above).
fn with_cache_headers(
    mut resp: Response,
    etag: &str,
    modified: Option<std::time::SystemTime>,
) -> Response {
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private"));
    if let Ok(v) = HeaderValue::from_str(etag) {
        h.insert(header::ETAG, v);
    }
    if let Some(v) = modified
        .map(httpdate::fmt_http_date)
        .and_then(|d| HeaderValue::from_str(&d).ok())
    {
        h.insert(header::LAST_MODIFIED, v);
    }
    resp
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

/// Body-serving tail shared by `/api/raw` and `/api/download` (issue #29):
/// open `path` and serve it honoring a single `Range` header — `206` +
/// `Content-Range` when a satisfiable range applies (gated by `If-Range`, so
/// bytes of two file versions are never spliced), `416` + `Content-Range:
/// bytes */{total}` when one is present but unsatisfiable, else `200` with
/// the whole body. Success responses advertise `Accept-Ranges: bytes`, carry
/// `Content-Length`, and get the cache validators; the caller layers any
/// per-endpoint headers (e.g. `Content-Disposition`) on top.
async fn serve_ranged(
    path: &Path,
    total: u64,
    ct: &str,
    etag: &str,
    modified: Option<std::time::SystemTime>,
    headers: &HeaderMap,
) -> Response {
    // A Range only applies when its If-Range validator (if any) still matches
    // the file — otherwise fall back to the full body, never mixed versions.
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| parse_range(h, total))
        .filter(|_| if_range_matches(headers, etag, modified));

    let mut file = match tokio::fs::File::open(path).await {
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
            let resp = (
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
                .into_response();
            with_cache_headers(resp, etag, modified)
        }
        Some(Err(())) => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{total}"))],
        )
            .into_response(),
        None => {
            let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
            let resp = (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, ct.to_string()),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    (header::CONTENT_LENGTH, total.to_string()),
                ],
                body,
            )
                .into_response();
            with_cache_headers(resp, etag, modified)
        }
    }
}

/// Serve a file inline (for thumbnails and previews), honoring Range requests
/// so the browser can seek video/audio. Responses carry cache validators
/// (`ETag` from mtime+size, `Last-Modified`) and a matched conditional
/// request short-circuits to `304 Not Modified` — so revisiting a folder of
/// already-seen images costs a handful of header exchanges instead of
/// re-reading every file off the SD card (issue #28).
pub async fn raw(
    _: Session,
    State(s): State<AppState>,
    Query(q): Query<PathQuery>,
    headers: HeaderMap,
) -> Response {
    let rel = q.path.unwrap_or_default();
    let Some((mount_root, path)) = s.mounts.resolve(&rel) else {
        return bad("invalid path");
    };
    let path = match confine(mount_root, &path).await {
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

    // Conditional GET first (RFC 9110 §13.2.2 evaluates these before Range):
    // a matched validator answers 304 before the file is even opened.
    let etag = etag_for(&meta, None);
    let modified = meta.modified().ok();
    if not_modified(&headers, &etag, modified) {
        return with_cache_headers(StatusCode::NOT_MODIFIED.into_response(), &etag, modified);
    }

    serve_ranged(&path, total, ct, &etag, modified, &headers).await
}

// --- thumbnails (issue #28) ---------------------------------------------------

/// Default and hard bounds for the `w` query parameter. The frontend asks
/// for 256 (largest grid cell at 2x DPR); the clamp keeps a hand-crafted
/// request from turning the endpoint into a free image-resizing service.
const THUMB_DEFAULT_W: u32 = 128;
const THUMB_MIN_W: u32 = 16;
const THUMB_MAX_W: u32 = 512;

#[derive(Deserialize)]
pub struct ThumbQuery {
    path: Option<String>,
    w: Option<u32>,
}

/// Directory for the on-disk thumbnail cache. Lives under the OS temp dir:
/// on the handhelds `/tmp` is tmpfs, so cached thumbnails cost zero SD-card
/// wear and the cache clears itself on reboot. Entries are keyed by content
/// identity ([`thumb_cache_key`]), so a stale entry is never *served* — a
/// changed file simply misses and leaves the old entry behind to die with
/// the temp dir.
fn thumb_cache_dir() -> PathBuf {
    std::env::temp_dir().join("amber-dav-thumbs")
}

/// Cache file stem for one rendered thumbnail: a hash of the canonical source
/// path (hex, so never a path separator) plus the same mtime+size identity
/// the ETag uses, plus the requested width. Any change to the source flips
/// the key, so lookups can trust a hit without re-checking the source.
fn thumb_cache_key(path: &Path, meta: &std::fs::Metadata, w: u32) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let hash = Sha256::digest(path.as_os_str().as_encoded_bytes());
    let mut hex = String::with_capacity(32);
    for b in &hash[..16] {
        let _ = write!(hex, "{b:02x}");
    }
    let (s, n) = mtime_parts(meta);
    format!("{hex}-{s:x}.{n:x}.{:x}.{w}", meta.len())
}

/// The two encodings a thumbnail is stored and served as, with their cache
/// file extension and MIME type. JPEG for opaque images (a 256 px photo cell
/// is ~5–15 KB instead of ~50 KB as PNG), PNG when the source has an alpha
/// channel worth keeping.
const THUMB_FORMATS: [(&str, &str); 2] = [("jpg", "image/jpeg"), ("png", "image/png")];

/// Decode `path` and downscale it to fit in `w`×`w` (aspect ratio preserved,
/// never upscaled). Runs on a blocking thread — decoding a 12 MP JPEG takes
/// hundreds of ms on the A53. `thumbnail()` is the image crate's fast path
/// (integer box-sample, then a triangle filter), the right trade for this
/// CPU. Returns the encoded bytes and the cache extension from
/// [`THUMB_FORMATS`]. Decode memory is capped so one absurd PNG cannot eat
/// the device's ~1 GB of RAM.
fn render_thumb(path: &Path, w: u32) -> Result<(Vec<u8>, &'static str), image::ImageError> {
    let mut reader = image::ImageReader::open(path)
        .map_err(image::ImageError::IoError)?
        .with_guessed_format()
        .map_err(image::ImageError::IoError)?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode()?;
    let thumb = if img.width() <= w && img.height() <= w {
        img // already small enough: re-encode as-is, never upscale
    } else {
        img.thumbnail(w, w)
    };
    let mut out = Vec::new();
    if thumb.color().has_alpha() {
        thumb.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
        Ok((out, "png"))
    } else {
        let mut cursor = std::io::Cursor::new(&mut out);
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 80);
        thumb.to_rgb8().write_with_encoder(enc)?;
        Ok((out, "jpg"))
    }
}

/// Best-effort cache write: temp file in the cache dir, then an atomic rename
/// (the same write-then-rename shape as uploads), so a concurrent request for
/// the same thumbnail never reads a half-written file. Failures are swallowed
/// — a broken cache only costs a re-render, never the response.
async fn store_thumb(dir: &Path, name: &str, bytes: &[u8]) {
    if tokio::fs::create_dir_all(dir).await.is_err() {
        return;
    }
    let tmp = dir.join(format!(".{name}.{}.part", std::process::id()));
    if tokio::fs::write(&tmp, bytes).await.is_ok()
        && tokio::fs::rename(&tmp, dir.join(name)).await.is_ok()
    {
        return;
    }
    let _ = tokio::fs::remove_file(&tmp).await;
}

/// `GET /api/thumb?path=…&w=128`: serve a server-side downscaled thumbnail
/// instead of the full original — the grid no longer pulls 2 MB per 128 px
/// cell off the SD card (issue #28). Same auth gate and path hardening as
/// every other handler, same cache validators as [`raw`] (the ETag carries
/// the width so the two endpoints' validators can never cross-match):
///
/// 1. conditional hit → 304, nothing read;
/// 2. disk-cache hit → serve the cached encoding;
/// 3. miss → decode + downscale off the runtime, cache, serve.
///
/// Non-image files (and formats the pure-Rust decoders don't know, e.g. SVG)
/// answer 415 — the frontend falls back to `/api/raw` for those.
pub async fn thumb(
    _: Session,
    State(s): State<AppState>,
    Query(q): Query<ThumbQuery>,
    headers: HeaderMap,
) -> Response {
    let rel = q.path.unwrap_or_default();
    let Some((mount_root, path)) = s.mounts.resolve(&rel) else {
        return bad("invalid path");
    };
    let path = match confine(mount_root, &path).await {
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
    let w =
        q.w.unwrap_or(THUMB_DEFAULT_W)
            .clamp(THUMB_MIN_W, THUMB_MAX_W);

    let etag = etag_for(&meta, Some(w));
    let modified = meta.modified().ok();
    if not_modified(&headers, &etag, modified) {
        return with_cache_headers(StatusCode::NOT_MODIFIED.into_response(), &etag, modified);
    }

    let dir = thumb_cache_dir();
    let key = thumb_cache_key(&path, &meta, w);
    for (ext, mime) in THUMB_FORMATS {
        if let Ok(bytes) = tokio::fs::read(dir.join(format!("{key}.{ext}"))).await {
            let resp = ([(header::CONTENT_TYPE, mime)], bytes).into_response();
            return with_cache_headers(resp, &etag, modified);
        }
    }

    let src = path.clone();
    let (bytes, ext) = match tokio::task::spawn_blocking(move || render_thumb(&src, w)).await {
        Ok(Ok(v)) => v,
        Ok(Err(image::ImageError::IoError(e))) => return io_err(e),
        Ok(Err(e)) => {
            return (
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("cannot thumbnail: {e}"),
            )
                .into_response()
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    store_thumb(&dir, &format!("{key}.{ext}"), &bytes).await;
    let mime = if ext == "png" {
        "image/png"
    } else {
        "image/jpeg"
    };
    let resp = ([(header::CONTENT_TYPE, mime)], bytes).into_response();
    with_cache_headers(resp, &etag, modified)
}

// --- zip (download multiple items / folders) -------------------------------

#[derive(Deserialize)]
pub struct ZipQuery {
    /// base64(JSON array of relative paths). Encoded so it survives a GET and
    /// handles arbitrary filenames; a GET lets the browser stream to disk.
    p: String,
}

/// Suggested archive filename for a zip download (review §2.8): a single
/// selected item keeps its own name (`Roms/GB` → `GB.zip`); a multi
/// selection is named after the common parent folder of the requested paths.
/// Selections rooted at the served root (no parent name) or with mixed
/// parents fall back to the old `amber-dav.zip` default.
fn zip_filename(rels: &[String]) -> String {
    fn segments(rel: &str) -> Vec<&str> {
        rel.split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect()
    }
    let base = match rels {
        [] => None,
        [one] => segments(one).last().copied(),
        many => {
            let mut parents = many.iter().map(|r| {
                let mut v = segments(r);
                v.pop();
                v
            });
            let first = parents.next().unwrap_or_default();
            parents
                .all(|p| p == first)
                .then(|| first.last().copied())
                .flatten()
        }
    };
    match base {
        Some(b) => format!("{b}.zip"),
        None => "amber-dav.zip".to_string(),
    }
}

/// Make the top-level archive entries unique (review §1.12): an exact
/// duplicate selection (same source path) is dropped outright; distinct
/// sources that happen to share a name get a `" (2)"`, `" (3)"`… suffix
/// before the extension. Without this, a crafted path list produces a zip
/// with duplicate entry names, which extractors handle unpredictably
/// (skip, clobber, or error).
fn uniquify_roots(roots: Vec<(PathBuf, String)>) -> Vec<(PathBuf, String)> {
    let mut seen_paths = std::collections::HashSet::new();
    let mut used_names = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (path, name) in roots {
        if !seen_paths.insert(path.clone()) {
            continue;
        }
        let mut unique = name.clone();
        for n in 2.. {
            if used_names.insert(unique.clone()) {
                break;
            }
            unique = match name.rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
                _ => format!("{name} ({n})"),
            };
        }
        out.push((path, unique));
    }
    out
}

/// `Content-Disposition: attachment` for `fname`, quotes/backslashes
/// stripped (the policy `/api/download` has always used). `None` when the
/// name can't form a header value — callers keep their default disposition.
fn attachment(fname: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        fname.replace(['"', '\\'], "")
    ))
    .ok()
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
        let Some((mount_root, abs)) = s.mounts.resolve(rel) else {
            return bad("invalid path");
        };
        // The archive entry keeps the selected name; the path it reads from
        // is confined (the zip reads through symlinks, so the full path is
        // canonicalized).
        let Some(name) = abs.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return bad("invalid path");
        };
        let abs = match confine(mount_root, &abs).await {
            Ok(p) => p,
            Err(e) => return io_err(e),
        };
        roots.push((abs, name));
    }
    if roots.is_empty() {
        return bad("nothing selected");
    }
    let roots = uniquify_roots(roots);

    // Pre-flight: an unreadable directory fails *now*, as a proper error
    // response — once streaming starts, the 200 is irrevocable (§1.12).
    for (abs, _) in &roots {
        match tokio::fs::metadata(abs).await {
            Ok(m) if m.is_dir() => {
                if let Err(e) = tokio::fs::read_dir(abs).await {
                    return io_err(e);
                }
            }
            Ok(_) => {}
            Err(e) => return io_err(e),
        }
    }

    // Stream the archive: a writer task fills one end of a pipe, the response
    // reads the other. Memory stays bounded to the pipe buffer.
    let (writer, reader) = tokio::io::duplex(ZIP_PIPE_BUFFER);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<std::io::Result<()>>();
    tokio::spawn(async move {
        let res = build_zip(writer, roots).await;
        if let Err(e) = &res {
            tracing::warn!("zip download aborted mid-stream: {e}");
        }
        let _ = done_tx.send(res);
    });
    // The archive bytes, then a tail that surfaces the writer's outcome: on
    // failure the body stream ends with an Err, so hyper drops the connection
    // without the terminating chunk and the client sees a failed/truncated
    // transfer — instead of a "complete" download that is silently a corrupt
    // zip. (The status line is long gone by then; this is the only signal
    // HTTP still allows mid-stream.)
    let data = tokio_util::io::ReaderStream::new(reader);
    let tail = futures_util::stream::once(async move {
        done_rx
            .await
            .unwrap_or_else(|_| Err(std::io::Error::other("zip writer task vanished")))
    })
    .filter_map(|res| async move {
        match res {
            Ok(()) => None, // clean finish: nothing to append
            Err(e) => Some(Err(e)),
        }
    });
    let stream = data.chain(tail);
    let disposition = attachment(&zip_filename(&rels))
        .unwrap_or_else(|| HeaderValue::from_static("attachment; filename=\"amber-dav.zip\""));
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
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

/// Download a file as an attachment, honoring `Range` so an interrupted
/// transfer resumes where it died instead of re-reading the whole file off
/// the SD card (issue #29). Shares the tested parse/seek/`take` machinery
/// with `/api/raw` via [`serve_ranged`]; the validators it emits (`ETag`,
/// `Last-Modified`) give download managers an `If-Range` token, so a resume
/// of a since-changed file falls back to the full body rather than splicing
/// two versions together.
pub async fn download(
    _: Session,
    State(s): State<AppState>,
    Query(q): Query<PathQuery>,
    headers: HeaderMap,
) -> Response {
    let rel = q.path.unwrap_or_default();
    let Some((mount_root, path)) = s.mounts.resolve(&rel) else {
        return bad("invalid path");
    };
    // Suggested filename comes from the requested path (what the UI shows),
    // while the bytes are read from the confined, canonical path.
    let fname = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let path = match confine(mount_root, &path).await {
        Ok(p) => p,
        Err(e) => return io_err(e),
    };
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) => return io_err(e),
    };
    if meta.is_dir() {
        // Folders go through /api/zip; opening one here would only fail at
        // first read, after the 200 and the attachment headers are long gone.
        return bad("not a file");
    }
    let etag = etag_for(&meta, None);
    let modified = meta.modified().ok();
    let mut resp = serve_ranged(
        &path,
        meta.len(),
        "application/octet-stream",
        &etag,
        modified,
        &headers,
    )
    .await;
    // Attachment disposition on both the full (200) and partial (206) body —
    // error responses (416, open/seek failures) must not invite a "save as".
    if matches!(resp.status(), StatusCode::OK | StatusCode::PARTIAL_CONTENT) {
        if let Some(v) = attachment(&fname) {
            resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    // The textual path-safety helpers live in `state` (one shared copy for
    // this module and the mount resolver); the tests here exercise them
    // against this module's handlers, so alias the old local names.
    use crate::state::{resolve_segments as resolve, windows_reserved};

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

    // The reserved-name *table* is pure and checked on every host; whether it
    // is applied is a target question, covered by the cfg'd test below.
    #[test]
    fn windows_reserved_matches_the_device_name_table() {
        // The documented Win32 set, bare and with extensions, any case, and
        // with the trailing spaces Win32 strips during name resolution.
        for name in [
            "con",
            "CON",
            "Con",
            "prn",
            "aux",
            "nul",
            "NUL.txt",
            "con.tar.gz",
            "com1",
            "COM9",
            "lpt1",
            "LPT9",
            "con ",
            "con .txt",
        ] {
            assert!(windows_reserved(name), "should be reserved: {name:?}");
        }
        // Near misses stay ordinary names: prefixes/suffixes, COM0/LPT0 and
        // double-digit ports (real files on Windows), non-ASCII lookalikes,
        // and names that merely contain a reserved stem.
        for name in [
            "console",
            "con1",
            "com",
            "com0",
            "com10",
            "lpt",
            "lpt0",
            "lptx",
            "aux2",
            "nula",
            "my.con",
            "xcon",
            "cön",
            "",
            "photo.png",
        ] {
            assert!(!windows_reserved(name), "should not be reserved: {name:?}");
        }
    }

    #[test]
    fn safe_name_rejects_reserved_device_names_on_windows_only() {
        for name in ["con", "NUL.txt", "com1", "lpt9.log"] {
            #[cfg(windows)]
            assert!(safe_name(name).is_none(), "must reject on Windows: {name}");
            #[cfg(unix)]
            assert!(safe_name(name).is_some(), "ordinary name on Unix: {name}");
        }
        // And `resolve` (which shares `plain_segment`) gates them per-segment.
        let root = Path::new("/srv/root");
        #[cfg(windows)]
        assert_eq!(resolve(root, "a/nul.txt"), None);
        #[cfg(unix)]
        assert_eq!(
            resolve(root, "a/nul.txt"),
            Some(PathBuf::from("/srv/root/a/nul.txt"))
        );
    }

    // Archive name derivation (review §2.8): single item → its own name,
    // common-parent multi selection → the parent's name, root-level or
    // mixed-parent selections → the old default.
    #[test]
    fn zip_filename_follows_the_selection() {
        let v = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(zip_filename(&v(&["Roms/GB"])), "GB.zip");
        assert_eq!(zip_filename(&v(&["GB"])), "GB.zip");
        assert_eq!(zip_filename(&v(&["Roms/GB/"])), "GB.zip");
        assert_eq!(zip_filename(&v(&["./Roms/./GB"])), "GB.zip");
        // Multi with a common parent: named after the folder they live in.
        assert_eq!(
            zip_filename(&v(&["Roms/GB/a.gb", "Roms/GB/b.gb"])),
            "GB.zip"
        );
        // Root-level multi: there is no parent name to use.
        assert_eq!(zip_filename(&v(&["a.txt", "b.txt"])), "amber-dav.zip");
        // Mixed parents: ambiguous, keep the default.
        assert_eq!(
            zip_filename(&v(&["Roms/GB/a.gb", "Roms/GBA/b.gba"])),
            "amber-dav.zip"
        );
        // Degenerate inputs never panic and keep the default.
        assert_eq!(zip_filename(&[]), "amber-dav.zip");
        assert_eq!(zip_filename(&v(&[""])), "amber-dav.zip");
    }

    // Entry-name hygiene (review §1.12): exact duplicate selections collapse
    // to one entry; distinct sources sharing a name get numbered, with the
    // suffix landing before the extension when there is one.
    #[test]
    fn zip_roots_are_deduplicated_and_uniquified() {
        let r = |p: &str, n: &str| (PathBuf::from(p), n.to_string());
        let out = uniquify_roots(vec![
            r("/a/x.txt", "x.txt"),
            r("/a/x.txt", "x.txt"), // exact duplicate: dropped
            r("/b/x.txt", "x.txt"), // same name, different source: numbered
            r("/c/x.txt", "x.txt"),
            r("/d/dir", "dir"),
            r("/e/dir", "dir"),         // no extension: suffix at the end
            r("/f/.hidden", ".hidden"), // dotfile: not treated as extension-only
            r("/g/.hidden", ".hidden"),
        ]);
        assert_eq!(
            out,
            vec![
                r("/a/x.txt", "x.txt"),
                r("/b/x.txt", "x (2).txt"),
                r("/c/x.txt", "x (3).txt"),
                r("/d/dir", "dir"),
                r("/e/dir", "dir (2)"),
                r("/f/.hidden", ".hidden"),
                r("/g/.hidden", ".hidden (2)"),
            ]
        );
    }

    // The disposition helper strips quote/backslash (header injection) and
    // keeps the attachment shape.
    #[test]
    fn attachment_disposition_strips_quotes() {
        let v = attachment("we\"ird\\name.zip").unwrap();
        assert_eq!(
            v.to_str().unwrap(),
            "attachment; filename=\"weirdname.zip\""
        );
        let v = attachment("GB.zip").unwrap();
        assert_eq!(v.to_str().unwrap(), "attachment; filename=\"GB.zip\"");
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

    // Cross-mount moves fall back to copy-then-delete (issue #76). The delete
    // step must handle directories — `remove_file` fails with "is a directory",
    // which would leave the tree duplicated in both mounts on every dir move.
    #[tokio::test]
    async fn move_across_devices_moves_a_directory_tree() {
        let src = TmpTree::new("xdev-src");
        let dst = TmpTree::new("xdev-dst");
        let from = src.0.join("folder");
        std::fs::create_dir(&from).unwrap();
        std::fs::write(from.join("a.txt"), b"a").unwrap();
        std::fs::create_dir(from.join("sub")).unwrap();
        std::fs::write(from.join("sub/b.txt"), b"b").unwrap();

        let to = dst.0.join("folder");
        move_across_devices(&from, &to)
            .await
            .expect("directory move must succeed");
        assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(to.join("sub/b.txt")).unwrap(), b"b");
        assert!(!from.exists(), "source must be deleted after the copy");
    }

    // A symlink crossing mounts must arrive as a symlink — exactly what
    // rename(2) does on one filesystem — never as a dereferenced copy of its
    // target: following it would materialize content from OUTSIDE the mounts
    // into the share (issue #76 review).
    #[cfg(unix)]
    #[tokio::test]
    async fn move_across_devices_preserves_symlinks() {
        let outside = TmpTree::new("xdev-outside");
        std::fs::write(outside.0.join("secret.txt"), b"top secret").unwrap();
        let target = outside.0.join("secret.txt");
        let src = TmpTree::new("xdev-link-src");
        let dst = TmpTree::new("xdev-link-dst");
        let link = src.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let moved = dst.0.join("link");
        move_across_devices(&link, &moved)
            .await
            .expect("symlink move must succeed");
        let meta = std::fs::symlink_metadata(&moved).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "must stay a symlink, not become a data copy"
        );
        assert_eq!(std::fs::read_link(&moved).unwrap(), target);
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "source link must be deleted"
        );

        // The same holds for symlinks nested inside a moved folder.
        let from = src.0.join("folder");
        std::fs::create_dir(&from).unwrap();
        std::os::unix::fs::symlink(&target, from.join("inner")).unwrap();
        let to = dst.0.join("folder");
        move_across_devices(&from, &to).await.unwrap();
        assert!(std::fs::symlink_metadata(to.join("inner"))
            .unwrap()
            .file_type()
            .is_symlink());
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
        let mounts = crate::state::MountTable::single(root.clone());

        for overwrite in [false, true] {
            let err = plan_transfer(&mounts, &["x.txt".into()], "", overwrite, true)
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
        let mounts = crate::state::MountTable::single(root.clone());

        let err = plan_transfer(&mounts, &["x.txt".into()], "sub", true, true)
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
        let mounts = crate::state::MountTable::single(root.clone());

        let jobs = plan_transfer(&mounts, &["x.txt".into()], "", false, false)
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
        let mounts = crate::state::MountTable::single(root.clone());

        for copy_mode in [false, true] {
            let err = plan_transfer(&mounts, &["d".into()], "d", false, copy_mode)
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
        let mounts = crate::state::MountTable::single(root.clone());
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::create_dir(root.join("b")).unwrap();
        std::fs::write(root.join("a/x.txt"), b"new").unwrap();
        std::fs::write(root.join("b/x.txt"), b"old").unwrap();

        // Both copy and move refuse to clobber without consent…
        for copy_mode in [false, true] {
            let err = plan_transfer(&mounts, &["a/x.txt".into()], "b", false, copy_mode)
                .await
                .unwrap_err();
            assert!(
                matches!(err, PlanError::Conflict(ref m) if m.contains("x.txt")),
                "want conflict, got {err:?}"
            );
        }
        assert_eq!(std::fs::read(root.join("b/x.txt")).unwrap(), b"old");

        // …and `overwrite=true` plans the very jobs the handler then runs.
        let jobs = plan_transfer(&mounts, &["a/x.txt".into()], "b", true, true)
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
        let mounts = crate::state::MountTable::single(root.clone());
        std::fs::create_dir(root.join("dst")).unwrap();
        std::fs::write(root.join("x.txt"), b"payload").unwrap();

        let jobs = plan_transfer(&mounts, &["x.txt".into()], "dst", false, false)
            .await
            .unwrap();
        assert_eq!(jobs, vec![(root.join("x.txt"), root.join("dst/x.txt"))]);

        let err = plan_transfer(&mounts, &["x.txt".into()], "missing", false, false)
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::Io(_)));
    }

    /// Names of leftover `.part` temp files in `dir` (should always be none —
    /// every [`write_upload`] error path removes its temp).
    fn part_files(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".part"))
            .collect()
    }

    /// A body stream that delivers `chunks` in order.
    fn body_ok(
        chunks: &[&[u8]],
    ) -> impl futures_util::Stream<Item = Result<Vec<u8>, String>> + Unpin {
        futures_util::stream::iter(chunks.iter().map(|c| Ok(c.to_vec())).collect::<Vec<_>>())
    }

    // Issue #24 happy path: the bytes land in a temp file and only a rename
    // makes them visible under the final name — and the temp is gone after.
    #[tokio::test]
    async fn upload_streams_then_renames_into_place() {
        let tree = TmpTree::new("upload-ok");
        let root = std::fs::canonicalize(&tree.0).unwrap();

        write_upload(&root, "new.txt", false, body_ok(&[b"hello ", b"world"]))
            .await
            .unwrap();
        assert_eq!(std::fs::read(root.join("new.txt")).unwrap(), b"hello world");
        assert!(part_files(&root).is_empty());
    }

    // Issue #24 core scenario: Wi-Fi drops mid-upload (the body stream yields
    // an error). The original file must survive byte-for-byte — the old code
    // had already truncated it — and the temp must not be left behind.
    #[tokio::test]
    async fn upload_aborted_stream_leaves_original_intact() {
        let tree = TmpTree::new("upload-abort");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("save.dat"), b"precious original").unwrap();

        let broken = futures_util::stream::iter(vec![
            Ok(b"half a replac".to_vec()),
            Err("connection reset".to_string()),
        ]);
        let err = write_upload(&root, "save.dat", true, broken)
            .await
            .unwrap_err();
        assert!(
            matches!(err, UploadError::Stream(ref m) if m.contains("connection reset")),
            "want stream error, got {err:?}"
        );
        assert_eq!(
            std::fs::read(root.join("save.dat")).unwrap(),
            b"precious original"
        );
        assert!(part_files(&root).is_empty());
    }

    // Issue #23 semantics survive the temp-file rework: without `overwrite`
    // an existing file is a conflict *before* any bytes stream, and with it
    // the file is replaced only at rename time.
    #[tokio::test]
    async fn upload_collision_requires_overwrite() {
        let tree = TmpTree::new("upload-collision");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("x.txt"), b"old").unwrap();

        let err = write_upload(&root, "x.txt", false, body_ok(&[b"new"]))
            .await
            .unwrap_err();
        assert!(
            matches!(err, UploadError::Conflict(ref m) if m.contains("x.txt")),
            "want conflict, got {err:?}"
        );
        assert_eq!(std::fs::read(root.join("x.txt")).unwrap(), b"old");

        write_upload(&root, "x.txt", true, body_ok(&[b"new"]))
            .await
            .unwrap();
        assert_eq!(std::fs::read(root.join("x.txt")).unwrap(), b"new");
        assert!(part_files(&root).is_empty());
    }

    // The old `create_new` on the target refused to write *through* a
    // pre-planted symlink; the rework must not regress that. Without
    // `overwrite` the link is a conflict like any other entry; with it the
    // rename replaces the link *entry* itself — the file it points to is
    // never written through.
    #[cfg(unix)]
    #[tokio::test]
    async fn upload_never_writes_through_preplanted_symlink() {
        let tree = TmpTree::new("upload-symlink");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("victim.txt"), b"do not touch").unwrap();
        std::os::unix::fs::symlink(root.join("victim.txt"), root.join("up.txt")).unwrap();

        let err = write_upload(&root, "up.txt", false, body_ok(&[b"payload"]))
            .await
            .unwrap_err();
        assert!(matches!(err, UploadError::Conflict(_)));

        write_upload(&root, "up.txt", true, body_ok(&[b"payload"]))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(root.join("victim.txt")).unwrap(),
            b"do not touch"
        );
        assert_eq!(std::fs::read(root.join("up.txt")).unwrap(), b"payload");
        let meta = std::fs::symlink_metadata(root.join("up.txt")).unwrap();
        assert!(!meta.file_type().is_symlink(), "link entry was replaced");
    }

    // Concurrent uploads of the same name must not share a temp file.
    #[test]
    fn upload_temp_paths_are_unique() {
        let dir = Path::new("/d");
        let a = upload_temp_path(dir, "x.txt");
        let b = upload_temp_path(dir, "x.txt");
        assert_ne!(a, b);
        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_string_lossy();
            assert!(name.starts_with(".x.txt."), "hidden dotfile: {name}");
            assert!(name.ends_with(".part"), "part suffix: {name}");
        }
    }

    // Folder uploads (issue #30): the `dir` field is resolved one validated
    // segment at a time, creating what is missing and returning the leaf.
    #[tokio::test]
    async fn ensure_subdir_creates_nested_dirs() {
        let tree = TmpTree::new("subdir-create");
        let root = std::fs::canonicalize(&tree.0).unwrap();

        let leaf = ensure_subdir(&root, root.clone(), "GameBoy/Saves")
            .await
            .unwrap();
        assert_eq!(leaf, root.join("GameBoy/Saves"));
        assert!(root.join("GameBoy/Saves").is_dir());

        // Idempotent: existing directories are reused, not errors.
        let leaf = ensure_subdir(&root, root.clone(), "GameBoy/Saves")
            .await
            .unwrap();
        assert_eq!(leaf, root.join("GameBoy/Saves"));
    }

    // Every segment of the new `dir` field goes through the same
    // single-Normal-component validation as `resolve` — `..`, absolute,
    // backslash and drive-letter segments are rejected *before* any directory
    // is created, so a half-valid path leaves nothing behind.
    #[tokio::test]
    async fn ensure_subdir_rejects_traversal_segments() {
        let tree = TmpTree::new("subdir-reject");
        let root = std::fs::canonicalize(&tree.0).unwrap();

        for sub in [
            "..",
            "../evil",
            "a/../b",
            "good/..",
            "/etc",
            "/",
            "a//b",
            "a/",
            ".",
            "a/./b",
            "a\\b",
            "\\etc",
            "C:\\evil",
            "\\\\?\\C:\\x",
            "a\0b",
            "",
        ] {
            let err = ensure_subdir(&root, root.clone(), sub).await.unwrap_err();
            assert!(
                matches!(err, UploadError::Bad(_)),
                "want Bad for {sub:?}, got {err:?}"
            );
        }
        // Nothing was created — not even the valid prefix of `good/..`.
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    // A file standing where a folder is needed is a conflict, not an
    // overwrite candidate — `dir` creation never replaces existing entries.
    #[tokio::test]
    async fn ensure_subdir_refuses_file_in_the_way() {
        let tree = TmpTree::new("subdir-file");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("taken"), b"a file").unwrap();

        let err = ensure_subdir(&root, root.clone(), "taken/sub")
            .await
            .unwrap_err();
        assert!(matches!(err, UploadError::Conflict(_)), "got {err:?}");
        assert_eq!(std::fs::read(root.join("taken")).unwrap(), b"a file");
    }

    // Symlink containment matches `confine` everywhere else: an existing
    // segment that points outside the root is refused before anything is
    // created beyond it, while an in-tree symlinked folder still works.
    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_subdir_blocks_symlink_escape() {
        let outside = TmpTree::new("subdir-outside");
        let tree = TmpTree::new("subdir-root");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.join("link")).unwrap();

        let err = ensure_subdir(&root, root.clone(), "link/sub")
            .await
            .unwrap_err();
        assert!(
            matches!(err, UploadError::Io(ref e) if e.kind() == std::io::ErrorKind::PermissionDenied),
            "got {err:?}"
        );
        assert!(!outside.0.join("sub").exists(), "escaped the root");

        // An in-tree symlink resolves to its canonical target and continues.
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("alias")).unwrap();
        let leaf = ensure_subdir(&root, root.clone(), "alias/sub")
            .await
            .unwrap();
        assert_eq!(leaf, root.join("real/sub"));
        assert!(root.join("real/sub").is_dir());
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

    /// Build a HeaderMap from (name, value) pairs for the conditional tests.
    fn hdrs(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.clone(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    // Issue #28 §1: the ETag is a quoted strong validator that tracks the
    // file's content identity — same bytes, same tag; touched or rewritten
    // file, different tag.
    #[test]
    fn etag_tracks_mtime_and_size() {
        let tree = TmpTree::new("etag");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        let m1 = std::fs::metadata(root.join("a.txt")).unwrap();

        let tag = etag_for(&m1, None);
        assert!(tag.starts_with('"') && tag.ends_with('"'), "quoted: {tag}");
        assert_eq!(tag, etag_for(&m1, None), "stable for unchanged metadata");

        // Same mtime cannot be guaranteed cheaply, but a size change alone
        // must already flip the tag.
        std::fs::write(root.join("a.txt"), b"hello world").unwrap();
        let m2 = std::fs::metadata(root.join("a.txt")).unwrap();
        assert_ne!(tag, etag_for(&m2, None));

        // A thumbnail variant of the same file is a different representation
        // and must carry a different tag (per width, too).
        assert_ne!(etag_for(&m1, None), etag_for(&m1, Some(128)));
        assert_ne!(etag_for(&m1, Some(128)), etag_for(&m1, Some(256)));
    }

    // The revalidation round-trip the browser performs: send back the ETag in
    // If-None-Match, get a 304. Covers the RFC's weak-compare and precedence
    // corners so a proxy-mangled tag still revalidates.
    #[test]
    fn conditional_if_none_match() {
        let etag = "\"abc.def.10\"";
        let now = std::time::SystemTime::now();

        // Round-trip: the tag we handed out comes back and matches.
        assert!(not_modified(
            &hdrs(&[(header::IF_NONE_MATCH, etag)]),
            etag,
            Some(now)
        ));
        // Weak form and tag lists still match; a different tag does not.
        assert!(not_modified(
            &hdrs(&[(header::IF_NONE_MATCH, "W/\"abc.def.10\"")]),
            etag,
            Some(now)
        ));
        assert!(not_modified(
            &hdrs(&[(header::IF_NONE_MATCH, "\"other\", \"abc.def.10\"")]),
            etag,
            Some(now)
        ));
        assert!(not_modified(
            &hdrs(&[(header::IF_NONE_MATCH, "*")]),
            etag,
            Some(now)
        ));
        assert!(!not_modified(
            &hdrs(&[(header::IF_NONE_MATCH, "\"stale\"")]),
            etag,
            Some(now)
        ));
        // No conditional headers at all: serve the body.
        assert!(!not_modified(&HeaderMap::new(), etag, Some(now)));
    }

    #[test]
    fn conditional_if_modified_since() {
        let etag = "\"abc.def.10\"";
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 500_000_000);
        let date = |t: std::time::SystemTime| httpdate::fmt_http_date(t);

        // The exact Last-Modified we handed out comes back: not modified,
        // even though the real mtime carries sub-second precision the HTTP
        // date lost.
        assert!(not_modified(
            &hdrs(&[(header::IF_MODIFIED_SINCE, &date(mtime))]),
            etag,
            Some(mtime)
        ));
        // Client's copy predates the file: modified.
        let hour = std::time::Duration::from_secs(3600);
        assert!(!not_modified(
            &hdrs(&[(header::IF_MODIFIED_SINCE, &date(mtime - hour))]),
            etag,
            Some(mtime)
        ));
        assert!(not_modified(
            &hdrs(&[(header::IF_MODIFIED_SINCE, &date(mtime + hour))]),
            etag,
            Some(mtime)
        ));
        // Garbage date: ignore the header, serve the body.
        assert!(!not_modified(
            &hdrs(&[(header::IF_MODIFIED_SINCE, "not a date")]),
            etag,
            Some(mtime)
        ));
        // If-None-Match present and failing wins over a matching date
        // (RFC 9110 §13.1.3: a recipient MUST ignore If-Modified-Since when
        // the request contains an If-None-Match).
        assert!(!not_modified(
            &hdrs(&[
                (header::IF_NONE_MATCH, "\"stale\""),
                (header::IF_MODIFIED_SINCE, &date(mtime + hour)),
            ]),
            etag,
            Some(mtime)
        ));
    }

    // Range requests must keep working (video seeking) and must not splice
    // bytes of a changed file: a stale If-Range validator downgrades to the
    // full body.
    #[test]
    fn if_range_gates_partial_content() {
        let etag = "\"abc.def.10\"";
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);

        // No If-Range: the Range header applies as before.
        assert!(if_range_matches(&HeaderMap::new(), etag, Some(mtime)));
        // Current validator: still applies.
        assert!(if_range_matches(
            &hdrs(&[(header::IF_RANGE, etag)]),
            etag,
            Some(mtime)
        ));
        assert!(if_range_matches(
            &hdrs(&[(header::IF_RANGE, &httpdate::fmt_http_date(mtime))]),
            etag,
            Some(mtime)
        ));
        // Stale tag, weak tag, or stale date: serve the full body instead.
        assert!(!if_range_matches(
            &hdrs(&[(header::IF_RANGE, "\"stale\"")]),
            etag,
            Some(mtime)
        ));
        assert!(!if_range_matches(
            &hdrs(&[(header::IF_RANGE, "W/\"abc.def.10\"")]),
            etag,
            Some(mtime)
        ));
        let earlier = mtime - std::time::Duration::from_secs(3600);
        assert!(!if_range_matches(
            &hdrs(&[(header::IF_RANGE, &httpdate::fmt_http_date(earlier))]),
            etag,
            Some(mtime)
        ));
    }

    // Issue #28 §2: the rendered thumbnail fits the requested box with the
    // aspect ratio preserved, and the encoding follows the alpha channel —
    // JPEG for opaque sources, PNG when transparency must survive.
    #[test]
    fn thumb_downscales_and_picks_format() {
        let tree = TmpTree::new("thumb-render");
        let root = std::fs::canonicalize(&tree.0).unwrap();

        let opaque = root.join("opaque.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            48,
            image::Rgb([200, 60, 60]),
        ))
        .save(&opaque)
        .unwrap();
        let (bytes, ext) = render_thumb(&opaque, 16).unwrap();
        assert_eq!(ext, "jpg");
        let out = image::load_from_memory(&bytes).unwrap();
        assert_eq!((out.width(), out.height()), (16, 12), "fits 16x16 box");

        let alpha = root.join("alpha.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            48,
            64,
            image::Rgba([0, 120, 0, 128]),
        ))
        .save(&alpha)
        .unwrap();
        let (bytes, ext) = render_thumb(&alpha, 16).unwrap();
        assert_eq!(ext, "png", "alpha source keeps transparency via PNG");
        let out = image::load_from_memory(&bytes).unwrap();
        assert_eq!((out.width(), out.height()), (12, 16));
    }

    // A source already smaller than the box ships at its own size — blowing
    // an 8 px icon up to 256 px would only waste bytes and blur.
    #[test]
    fn thumb_never_upscales() {
        let tree = TmpTree::new("thumb-small");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        let small = root.join("small.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::new(8, 6))
            .save(&small)
            .unwrap();
        let (bytes, _) = render_thumb(&small, 256).unwrap();
        let out = image::load_from_memory(&bytes).unwrap();
        assert_eq!((out.width(), out.height()), (8, 6));
    }

    // The 415-vs-IO split the handler relies on: bytes that don't decode
    // (e.g. a macOS "._" AppleDouble file named .png) are a decode error the
    // frontend turns into a /api/raw fallback, while a missing file stays an
    // IO error mapped through io_err (404).
    #[test]
    fn thumb_separates_decode_errors_from_io() {
        let tree = TmpTree::new("thumb-errors");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("fake.png"), b"this is not an image").unwrap();

        let err = render_thumb(&root.join("fake.png"), 128).unwrap_err();
        assert!(
            !matches!(err, image::ImageError::IoError(_)),
            "decode failure, not IO: {err}"
        );
        let err = render_thumb(&root.join("missing.png"), 128).unwrap_err();
        assert!(matches!(err, image::ImageError::IoError(_)));
    }

    // The disk-cache key is the thumbnail's full identity: any change to the
    // source path, its content (mtime/size), or the requested width must miss
    // — and the key must be a single plain file name, never a path.
    #[test]
    fn thumb_cache_key_tracks_identity() {
        let tree = TmpTree::new("thumb-key");
        let root = std::fs::canonicalize(&tree.0).unwrap();
        std::fs::write(root.join("a.png"), b"aaaa").unwrap();
        std::fs::write(root.join("b.png"), b"aaaa").unwrap();
        let ma = std::fs::metadata(root.join("a.png")).unwrap();
        let mb = std::fs::metadata(root.join("b.png")).unwrap();

        let key = thumb_cache_key(&root.join("a.png"), &ma, 128);
        assert!(safe_name(&key).is_some(), "plain file name: {key}");
        assert_eq!(key, thumb_cache_key(&root.join("a.png"), &ma, 128));
        assert_ne!(key, thumb_cache_key(&root.join("b.png"), &mb, 128));
        assert_ne!(key, thumb_cache_key(&root.join("a.png"), &ma, 256));

        std::fs::write(root.join("a.png"), b"aaaa-changed").unwrap();
        let ma2 = std::fs::metadata(root.join("a.png")).unwrap();
        assert_ne!(key, thumb_cache_key(&root.join("a.png"), &ma2, 128));
    }

    // store_thumb commits atomically (no .part litter) and a second write of
    // the same key — two concurrent first requests — lands cleanly.
    #[tokio::test]
    async fn thumb_cache_store_roundtrip() {
        let tree = TmpTree::new("thumb-store");
        let dir = std::fs::canonicalize(&tree.0).unwrap().join("cache");

        store_thumb(&dir, "k.jpg", b"payload").await;
        assert_eq!(std::fs::read(dir.join("k.jpg")).unwrap(), b"payload");
        store_thumb(&dir, "k.jpg", b"payload2").await;
        assert_eq!(std::fs::read(dir.join("k.jpg")).unwrap(), b"payload2");
        assert!(part_files(&dir).is_empty(), "no temp litter");
    }

    // --- find: the name matcher --------------------------------------------

    // A wildcard-free pattern is the same case-insensitive substring test the
    // toolbar filter has always applied — typing a few letters must mean the
    // same thing whether it searches one folder or the whole tree.
    #[test]
    fn plain_pattern_is_a_substring_match() {
        assert!(name_matches("save", "save01.srm"));
        assert!(name_matches("save", "MySaveFile"));
        assert!(name_matches("SAVE", "my save.dat"));
        assert!(name_matches(".srm", "pokemon.srm"));
        // Substring, so an extension query also matches mid-name — the reason
        // the glob form exists.
        assert!(name_matches(".srm", "notes.srm.bak"));
        assert!(!name_matches("save", "pokemon.gb"));
        // A whole-name pattern still matches the whole name.
        assert!(name_matches("pokemon.gb", "pokemon.gb"));
    }

    // A pattern carrying `*` or `?` switches to a whole-name glob, which is
    // what makes `*.srm` mean "ends in .srm" rather than "contains .srm".
    #[test]
    fn wildcard_pattern_is_a_whole_name_glob() {
        assert!(name_matches("*.srm", "pokemon.srm"));
        assert!(!name_matches("*.srm", "notes.srm.bak"));
        assert!(name_matches("save*.srm", "save01.srm"));
        assert!(!name_matches("save*.srm", "01save.srm"));
        assert!(name_matches("save?.dat", "save1.dat"));
        assert!(!name_matches("save?.dat", "save12.dat"));
        assert!(name_matches("*save*", "my save file"));
        assert!(name_matches("*", "anything"));
        // Case-insensitive in glob mode too.
        assert!(name_matches("*.SRM", "pokemon.srm"));
        // A bare word is not a glob, so it does NOT have to match the whole
        // name — but with a wildcard present, anchoring applies.
        assert!(!name_matches("save*", "my save"));
    }

    // Glob edge cases the backtracking loop has to get right: stars that must
    // match nothing, repeated stars, and a `?` that has no character left.
    #[test]
    fn glob_edges() {
        let g = |p: &str, n: &str| {
            glob_match(
                &p.chars().collect::<Vec<_>>(),
                &n.chars().collect::<Vec<_>>(),
            )
        };
        assert!(g("*", ""));
        assert!(g("**", "ab"));
        assert!(g("a*", "a"));
        assert!(g("*a", "a"));
        assert!(g("a*b*c", "abc"));
        assert!(g("a*b*c", "axxbyyc"));
        assert!(!g("a*b*c", "abd"));
        assert!(!g("?", ""));
        assert!(g("?", "a"));
        assert!(!g("a", ""));
        assert!(!g("", "a"));
        assert!(g("", ""));
        // Multi-byte names are compared by character, so `?` is one glyph.
        assert!(g("?.txt", "é.txt"));
        assert!(g("*é*", "café.txt"));
    }
}
