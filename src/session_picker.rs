//! Session discovery and file deletion shared by the interactive stacks.
//!
//! The classic bubbletea session picker was removed; what remains are the
//! stack-independent helpers: session id/time formatting, the on-disk session
//! scan, and durable session-file deletion.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::session::encode_cwd;
use crate::session_index::session_file_stats;
use crate::session_index::{SessionIndex, SessionMeta, build_meta_from_file, is_session_file_path};
/// Format a timestamp for display.
pub fn format_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp).map_or_else(
        |_| timestamp.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M").to_string(),
    )
}

/// Truncate a session id by character count for display.
#[must_use]
pub fn truncate_session_id(session_id: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    let end = session_id
        .char_indices()
        .nth(max_chars)
        .map_or(session_id.len(), |(idx, _)| idx);
    &session_id[..end]
}

/// The session picker TUI model.

/// List sessions for the current working directory using the session index.
pub fn list_sessions_for_cwd() -> Vec<SessionMeta> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    list_sessions_for_project(&cwd, None)
}


pub fn list_sessions_for_project(cwd: &Path, override_dir: Option<&Path>) -> Vec<SessionMeta> {
    let base_dir = override_dir.map_or_else(Config::sessions_dir, PathBuf::from);
    let project_session_dir = base_dir.join(encode_cwd(cwd));
    let cwd_key = cwd.display().to_string();
    let index = SessionIndex::for_sessions_root(&base_dir);
    let mut sessions = index.list_sessions(Some(&cwd_key)).unwrap_or_default();
    let project_session_dir_missing = indexed_session_path_is_missing(&project_session_dir);

    if !project_session_dir_missing && sessions.is_empty() && index.reindex_all().is_ok() {
        sessions = index.list_sessions(Some(&cwd_key)).unwrap_or_default();
    }

    let mut missing_paths = Vec::new();
    let mut by_path = HashMap::new();
    for meta in sessions {
        let path = PathBuf::from(&meta.path);
        if indexed_session_path_is_missing(&path) {
            missing_paths.push(path);
        } else {
            by_path.insert(meta.path.clone(), meta);
        }
    }

    for path in &missing_paths {
        let _ = index.delete_session_path(path);
    }

    if project_session_dir_missing {
        return Vec::new();
    }

    let scanned = scan_sessions_on_disk(&project_session_dir, &by_path);
    for path in &scanned.failed_paths {
        let _ = index.delete_session_path(path);
        by_path.remove(&path.display().to_string());
    }

    for meta in scanned.metas {
        let _ = index.upsert_session_meta(meta.clone());
        by_path.insert(meta.path.clone(), meta);
    }

    // Issue #199: the map above dedups by raw path STRING only, so one
    // session can surface twice — the index row and the directory scan can
    // spell the same file differently (symlinked sessions root, macOS
    // /tmp vs /private/tmp), and a session persisted under both store
    // backends (`.jsonl` + `.sqlite`) yields two files with the same header
    // id and identical content. Collapse rows sharing a session id, keeping
    // the most recently modified (then largest) representative.
    let mut by_id: HashMap<String, SessionMeta> = HashMap::new();
    let mut anonymous = Vec::new();
    for meta in by_path.into_values() {
        if meta.id.is_empty() {
            anonymous.push(meta);
            continue;
        }
        match by_id.entry(meta.id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let current = entry.get();
                if (meta.last_modified_ms, meta.message_count, meta.size_bytes)
                    > (
                        current.last_modified_ms,
                        current.message_count,
                        current.size_bytes,
                    )
                {
                    entry.insert(meta);
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(meta);
            }
        }
    }

    sessions = by_id.into_values().chain(anonymous).collect();
    // Pagination belongs in the UI, not the data source. Older sessions must
    // remain resumable/searchable, with deterministic ordering for mtime ties.
    sessions.sort_by(|a, b| {
        Reverse(a.last_modified_ms)
            .cmp(&Reverse(b.last_modified_ms))
            .then_with(|| a.path.cmp(&b.path))
    });
    sessions
}

fn indexed_session_path_is_missing(path: &Path) -> bool {
    match crate::session::session_path_try_exists(path) {
        Ok(exists) => !exists,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "Failed to determine whether indexed session path exists; deferring prune"
            );
            false
        }
    }
}

struct ScanSessionsResult {
    metas: Vec<SessionMeta>,
    failed_paths: Vec<PathBuf>,
}

#[cfg(test)]
thread_local! {
    static SESSION_SCAN_PARSE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_session_scan_parse_count() {
    SESSION_SCAN_PARSE_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn take_session_scan_parse_count() -> usize {
    SESSION_SCAN_PARSE_COUNT.with(|count| {
        let value = count.get();
        count.set(0);
        value
    })
}

fn build_scanned_meta(path: &Path) -> crate::error::Result<SessionMeta> {
    #[cfg(test)]
    SESSION_SCAN_PARSE_COUNT.with(|count| count.set(count.get().saturating_add(1)));

    build_meta_from_file(path)
}

fn cached_meta_matches_disk(meta: &SessionMeta, path: &Path) -> bool {
    let Ok((last_modified_ms, size_bytes)) = session_file_stats(path) else {
        return false;
    };
    meta.last_modified_ms == last_modified_ms && meta.size_bytes == size_bytes
}

fn scan_sessions_on_disk(
    project_session_dir: &Path,
    cached_by_path: &HashMap<String, SessionMeta>,
) -> ScanSessionsResult {
    let mut out = Vec::new();
    let mut failed_paths = Vec::new();
    if let Err(err) = crate::session::ensure_session_directory_readable(project_session_dir) {
        tracing::warn!(
            path = %project_session_dir.display(),
            error = %err,
            "Failed to read project session directory; retaining indexed rows"
        );
        return ScanSessionsResult {
            metas: out,
            failed_paths,
        };
    }
    let Ok(entries) = fs::read_dir(project_session_dir) else {
        return ScanSessionsResult {
            metas: out,
            failed_paths,
        };
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if is_session_file_path(&path) {
            let path_key = path.display().to_string();
            if cached_by_path
                .get(&path_key)
                .is_some_and(|meta| cached_meta_matches_disk(meta, &path))
            {
                continue;
            }

            match build_scanned_meta(&path) {
                Ok(meta) => out.push(meta),
                Err(_) => failed_paths.push(path),
            }
        }
    }

    ScanSessionsResult {
        metas: out,
        failed_paths,
    }
}

pub(crate) fn delete_session_file(path: &Path) -> Result<()> {
    delete_session_file_with_trash_cmd(path, "trash")
}

fn delete_session_file_with_trash_cmd(path: &Path, trash_cmd: &str) -> Result<()> {
    if !session_artifacts_exist(path)? {
        return Ok(());
    }

    // Writers for JSONL, SQLite, and their sidecars all participate in this
    // persistent per-session lock. Re-check after acquisition so a delete
    // waiting behind a writer cannot operate on a stale artifact inventory.
    let _lock = crate::session::lock_session_persistence(path)?;
    if !session_artifacts_exist(path)? {
        return Ok(());
    }
    crate::session::ensure_session_parent_writable(path).map_err(|err| Error::Io(Box::new(err)))?;

    if try_trash_with_cmd(path, trash_cmd) {
        if crate::session::session_path_entry_exists(path)
            .map_err(|error| Error::Io(Box::new(error)))?
        {
            return Err(Error::session(format!(
                "Trash command reported success but left the session in place; sidecars were preserved: {}",
                path.display()
            )));
        }
        remove_sqlite_sidecars_best_effort(path, trash_cmd)?;
        remove_sidecar_dir_best_effort(&crate::session_store_v2::v2_sidecar_path(path), trash_cmd)?;
        return ensure_session_artifacts_removed(path);
    }

    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(Error::session(format!(
                "Failed to delete session {}: {err}",
                path.display()
            )));
        }
    }

    remove_sqlite_sidecars_best_effort(path, trash_cmd)?;
    remove_sidecar_dir_best_effort(&crate::session_store_v2::v2_sidecar_path(path), trash_cmd)?;
    ensure_session_artifacts_removed(path)
}

fn ensure_session_artifacts_removed(path: &Path) -> Result<()> {
    if session_artifacts_exist(path)? {
        return Err(Error::session(format!(
            "Session deletion left one or more artifacts behind: {}",
            path.display()
        )));
    }
    Ok(())
}

fn session_artifacts_exist(path: &Path) -> Result<bool> {
    let primary_exists =
        crate::session::session_path_entry_exists(path).map_err(|err| Error::Io(Box::new(err)))?;
    let v2_exists =
        crate::session::session_path_entry_exists(&crate::session_store_v2::v2_sidecar_path(path))
            .map_err(|err| Error::Io(Box::new(err)))?;
    #[cfg(feature = "sqlite-sessions")]
    let sqlite_sidecar_exists = sqlite_auxiliary_paths(path)
        .into_iter()
        .try_fold(false, |found, auxiliary_path| {
            crate::session::session_path_entry_exists(&auxiliary_path).map(|exists| found || exists)
        })
        .map_err(|err| Error::Io(Box::new(err)))?;
    #[cfg(not(feature = "sqlite-sessions"))]
    let sqlite_sidecar_exists = false;
    Ok(primary_exists || v2_exists || sqlite_sidecar_exists)
}

fn sqlite_auxiliary_paths(path: &Path) -> [PathBuf; 7] {
    crate::session_sqlite::SQLITE_SIDECAR_SUFFIXES.map(|suffix| {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        PathBuf::from(candidate)
    })
}

#[cfg(feature = "sqlite-sessions")]
fn remove_sqlite_sidecars_best_effort(path: &Path, trash_cmd: &str) -> Result<()> {
    if path.extension().and_then(|ext| ext.to_str()) == Some("sqlite") {
        for auxiliary_path in sqlite_auxiliary_paths(path) {
            if !crate::session::session_path_entry_exists(&auxiliary_path)
                .map_err(|error| Error::Io(Box::new(error)))?
            {
                continue;
            }
            if try_trash_with_cmd(&auxiliary_path, trash_cmd)
                && !crate::session::session_path_entry_exists(&auxiliary_path)
                    .map_err(|error| Error::Io(Box::new(error)))?
            {
                continue;
            }
            if let Err(err) = fs::remove_file(&auxiliary_path)
                && err.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(
                    path = %auxiliary_path.display(),
                    error = %err,
                    "Failed to remove SQLite sidecar"
                );
            }
        }
    }
    Ok(())
}

#[cfg(not(feature = "sqlite-sessions"))]
fn remove_sqlite_sidecars_best_effort(_path: &Path, _trash_cmd: &str) -> Result<()> {
    Ok(())
}

fn remove_sidecar_dir_best_effort(sidecar_path: &Path, trash_cmd: &str) -> Result<()> {
    if !crate::session::session_path_entry_exists(sidecar_path)
        .map_err(|error| Error::Io(Box::new(error)))?
    {
        return Ok(());
    }

    if try_trash_with_cmd(sidecar_path, trash_cmd)
        && !crate::session::session_path_entry_exists(sidecar_path)
            .map_err(|error| Error::Io(Box::new(error)))?
    {
        return Ok(());
    }

    let metadata =
        fs::symlink_metadata(sidecar_path).map_err(|error| Error::Io(Box::new(error)))?;
    let removal = if metadata.file_type().is_symlink() || !metadata.is_dir() {
        fs::remove_file(sidecar_path)
    } else {
        fs::remove_dir_all(sidecar_path)
    };
    if let Err(err) = removal {
        tracing::warn!(
            path = %sidecar_path.display(),
            error = %err,
            "Failed to remove session sidecar"
        );
    }
    Ok(())
}

fn try_trash_with_cmd(path: &Path, trash_cmd: &str) -> bool {
    let child = std::process::Command::new(trash_cmd)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return false,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "trash command invocation failed; falling back to direct file removal"
            );
            return false;
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return true,
            Ok(Some(status)) => {
                tracing::warn!(
                    path = %path.display(),
                    exit = status.code().unwrap_or(-1),
                    "trash command failed; falling back to direct file removal"
                );
                return false;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    path = %path.display(),
                    "trash command timed out; falling back to direct file removal"
                );
                return false;
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "trash command wait failed; falling back to direct file removal"
                );
                return false;
            }
        }
    }
}

