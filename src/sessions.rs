//! `sessions` tool: list, read, search, soft-delete and restore stored sessions.
//!
//! The live conversation is already in the model's context, so this tool exists
//! for the sessions *around* it: browsing the store, reading another transcript
//! (or recovering the head of this one after compaction dropped it), searching
//! across sessions, moving one to the GC trash, and restoring it from there.
//!
//! Two deliberate properties:
//!
//! - **Deletion is soft.** A delete renames the file into the same trash
//!   directory `ra gc` uses and appends an `ra.gc.v1` ledger row, so an agent
//!   mistake is recoverable. Named, pinned, active and live sessions are
//!   refused.
//! - **`scope: "current"` is fail-closed.** When the caller scopes a call to
//!   the live session, every action is pinned to it: `list` returns only it,
//!   `read`/`search` never leave it, and `delete` refuses outright. A scoped
//!   call can therefore never reach another conversation, even by passing a
//!   path.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::gc::{GC_LEDGER_SCHEMA, GarbageCollector, GcLedgerRecord, GcStoreKind};
use crate::model::{ContentBlock, TextContent};
use crate::session_index::{SessionIndex, SessionMeta};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};

/// Cap on sessions returned by `list`.
const SESSIONS_MAX_LIST: usize = 100;
/// Cap on messages returned by `read`.
const SESSIONS_MAX_READ_MESSAGES: usize = 200;
/// Cap on characters rendered for a single message by `read`.
const SESSIONS_MAX_READ_CHARS: usize = 4000;
/// Cap on files scanned by `search`.
const SESSIONS_MAX_SEARCH_FILES: usize = 200;
/// Cap on matches returned by `search`.
const SESSIONS_MAX_SEARCH_MATCHES: usize = 50;
/// Skip files larger than this in `search` rather than slurping them.
const SESSIONS_MAX_SEARCH_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// The `sessions` built-in tool.
pub struct SessionsTool {
    sessions_root: PathBuf,
    trash_dir: PathBuf,
    ledger_path: PathBuf,
    job_session_scope: crate::jobs::JobSessionScope,
}

impl SessionsTool {
    /// Build with the process-wide session store and GC trash/ledger paths.
    #[must_use]
    pub fn new() -> Self {
        Self::with_paths(
            crate::config::Config::sessions_dir(),
            GarbageCollector::default_trash_dir(),
            GarbageCollector::default_ledger_path(),
        )
    }

    /// Build against explicit paths (used by tests and embedders).
    #[must_use]
    pub fn with_paths(sessions_root: PathBuf, trash_dir: PathBuf, ledger_path: PathBuf) -> Self {
        Self {
            sessions_root,
            trash_dir,
            ledger_path,
            job_session_scope: crate::jobs::JobSessionScope::default(),
        }
    }

    /// Resolve the live session id, if the host bound a resolver.
    async fn live_session_id(&self) -> Option<String> {
        self.job_session_scope.session_id().await.ok()
    }

    /// Resolve the target session file for a `read`/`delete` selector.
    fn resolve_target_path(
        &self,
        selector: Option<&str>,
        scoped_current: bool,
        live_id: Option<&str>,
    ) -> Result<PathBuf> {
        let want_current =
            scoped_current || selector.is_some_and(|s| s.eq_ignore_ascii_case("current"));
        if want_current {
            let live = live_id.ok_or_else(|| {
                Error::tool(
                    "sessions",
                    "scope=current requires a live session, but none is available",
                )
            })?;
            return self.find_path_by_id(live).ok_or_else(|| {
                Error::tool(
                    "sessions",
                    format!("the live session {live} is not saved to the store yet"),
                )
            });
        }

        let selector = selector.ok_or_else(|| {
            Error::validation("sessions: this action needs a `session` selector".to_string())
        })?;

        if looks_like_path(selector) {
            let path = PathBuf::from(selector);
            if !path.is_file() {
                return Err(Error::validation(format!(
                    "sessions: no session file at {selector}"
                )));
            }
            if !self.path_is_under_root(&path) {
                return Err(Error::validation(format!(
                    "sessions: {selector} is outside the session store"
                )));
            }
            return Ok(path);
        }

        self.find_path_by_id(selector)
            .ok_or_else(|| Error::validation(format!("sessions: no session matches {selector:?}")))
    }

    /// Whether `path` lives under this tool's session store.
    fn path_is_under_root(&self, path: &Path) -> bool {
        let root = self
            .sessions_root
            .canonicalize()
            .unwrap_or_else(|_| self.sessions_root.clone());
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        path.starts_with(&root)
    }

    /// Find a stored session by full id or unique id prefix.
    fn find_path_by_id(&self, needle: &str) -> Option<PathBuf> {
        let index = SessionIndex::for_sessions_root(&self.sessions_root);
        let mut metas = index.list_sessions(None).unwrap_or_default();
        if metas.is_empty() {
            let _ = index.reindex_all();
            metas = index.list_sessions(None).unwrap_or_default();
        }
        if let Some(meta) = metas.iter().find(|meta| meta.id == needle) {
            return Some(PathBuf::from(&meta.path));
        }
        let mut prefix_matches: Vec<SessionMeta> = metas
            .into_iter()
            .filter(|meta| meta.id.starts_with(needle))
            .collect();
        match prefix_matches.len() {
            0 => None,
            1 => prefix_matches.pop().map(|meta| PathBuf::from(meta.path)),
            _ => None,
        }
    }

    /// `list`: the store's session metadata, newest first.
    fn run_list(
        &self,
        input: &SessionsInput,
        scoped_current: bool,
        live_id: Option<&str>,
    ) -> Result<ToolOutput> {
        let index = SessionIndex::for_sessions_root(&self.sessions_root);
        let mut metas = index.list_sessions(None).unwrap_or_default();
        if metas.is_empty() {
            let _ = index.reindex_all();
            metas = index.list_sessions(None).unwrap_or_default();
        }
        let since_ms = input.since.as_deref().map(parse_time_bound).transpose()?;
        let until_ms = input.until.as_deref().map(parse_time_bound).transpose()?;

        let mut metas: Vec<SessionMeta> = if scoped_current {
            let live = live_id.ok_or_else(|| {
                Error::tool(
                    "sessions",
                    "scope=current requires a live session, but none is available",
                )
            })?;
            metas.into_iter().filter(|meta| meta.id == live).collect()
        } else {
            metas
                .into_iter()
                .filter(|meta| meta_matches(meta, input, since_ms, until_ms))
                .collect()
        };

        let total = metas.len();
        let limit = input
            .limit
            .unwrap_or(SESSIONS_MAX_LIST)
            .min(SESSIONS_MAX_LIST);
        metas.truncate(limit);

        let text = if metas.is_empty() {
            "No sessions matched.".to_string()
        } else {
            metas
                .iter()
                .map(render_meta_line)
                .collect::<Vec<_>>()
                .join("\n")
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(serde_json::json!({
                "schema": "ra.sessions.list.v1",
                "totalMatched": total,
                "returned": metas.len(),
                "scope": if scoped_current { "current" } else { "all" },
            })),
            is_error: false,
        })
    }

    /// `read`: render a bounded slice of one session's transcript.
    async fn run_read(
        &self,
        input: &SessionsInput,
        scoped_current: bool,
        live_id: Option<&str>,
    ) -> Result<ToolOutput> {
        let path = self.resolve_target_path(input.session.as_deref(), scoped_current, live_id)?;
        let session = crate::session::Session::open(&path.to_string_lossy()).await?;
        let messages = session.to_messages_for_current_path();
        let total = messages.len();
        let offset = input.offset.unwrap_or(0);
        let limit = input
            .limit
            .unwrap_or(SESSIONS_MAX_READ_MESSAGES)
            .min(SESSIONS_MAX_READ_MESSAGES);

        let mut lines = Vec::new();
        for (idx, message) in messages.iter().enumerate().skip(offset).take(limit) {
            lines.push(format!("[{idx}] {}", clip(&render_message(message))));
        }
        let text = if lines.is_empty() {
            format!("(no messages at offset {offset}; session has {total})")
        } else {
            lines.join("\n")
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(serde_json::json!({
                "schema": "ra.sessions.read.v1",
                "id": session.header.id,
                "path": path.display().to_string(),
                "totalMessages": total,
                "offset": offset,
                "returned": lines.len(),
            })),
            is_error: false,
        })
    }

    /// `search`: substring match across session files (bounded).
    fn run_search(
        &self,
        input: &SessionsInput,
        scoped_current: bool,
        live_id: Option<&str>,
    ) -> Result<ToolOutput> {
        let query = input
            .query
            .as_deref()
            .filter(|query| !query.is_empty())
            .ok_or_else(|| {
                Error::validation("sessions: search needs a non-empty `query`".to_string())
            })?;

        let files: Vec<PathBuf> = if scoped_current {
            vec![self.resolve_target_path(Some("current"), true, live_id)?]
        } else {
            crate::stats::collect_session_files(&self.sessions_root, input.project.as_deref())
        };

        let since_ms = input.since.as_deref().map(parse_time_bound).transpose()?;
        let mut matches = Vec::new();
        let mut scanned = 0usize;
        for path in files {
            if matches.len() >= SESSIONS_MAX_SEARCH_MATCHES || scanned >= SESSIONS_MAX_SEARCH_FILES
            {
                break;
            }
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if metadata.len() > SESSIONS_MAX_SEARCH_FILE_BYTES {
                continue;
            }
            if let Some(since) = since_ms {
                let modified_ms = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as i64)
                    .unwrap_or(0);
                if modified_ms < since {
                    continue;
                }
            }
            scanned += 1;
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (lineno, line) in contents.lines().enumerate() {
                if line.contains(query) {
                    matches.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        lineno + 1,
                        clip_line(line, 240)
                    ));
                    if matches.len() >= SESSIONS_MAX_SEARCH_MATCHES {
                        break;
                    }
                }
            }
        }

        let text = if matches.is_empty() {
            format!("No matches for {query:?}.")
        } else {
            matches.join("\n")
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(serde_json::json!({
                "schema": "ra.sessions.search.v1",
                "query": query,
                "filesScanned": scanned,
                "matches": matches.len(),
                "scope": if scoped_current { "current" } else { "all" },
            })),
            is_error: false,
        })
    }

    /// `delete`: move one session into the trash (never unlink).
    fn run_delete(
        &self,
        input: &SessionsInput,
        scoped_current: bool,
        live_id: Option<&str>,
    ) -> Result<ToolOutput> {
        if scoped_current {
            return Err(Error::tool(
                "sessions",
                "scope=current refuses to delete the live conversation",
            ));
        }
        let selector = input.session.as_deref().ok_or_else(|| {
            Error::validation("sessions: delete needs a `session` selector".to_string())
        })?;
        let path = self.resolve_target_path(Some(selector), false, live_id)?;

        if let Some(live) = live_id {
            if header_id(&path).as_deref() == Some(live) {
                return Err(Error::tool(
                    "sessions",
                    "refusing to delete the live conversation",
                ));
            }
        }
        if GarbageCollector::is_session_protected(&path) {
            return Err(Error::tool(
                "sessions",
                format!(
                    "refusing to delete a named, pinned, or active session: {}",
                    path.display()
                ),
            ));
        }
        if !input.confirm {
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(format!(
                    "Dry run: would move {} to the trash.\nRe-run with confirm=true to apply.",
                    path.display()
                )))],
                details: Some(serde_json::json!({
                    "schema": "ra.sessions.delete.v1",
                    "dryRun": true,
                    "path": path.display().to_string(),
                })),
                is_error: false,
            });
        }

        let run_dir = self.trash_dir.join(format!("run_{}", now_secs()));
        std::fs::create_dir_all(&run_dir)
            .map_err(|err| Error::tool("sessions", format!("cannot create trash dir: {err}")))?;
        let dest = unique_destination(&run_dir, &path)?;
        let size = std::fs::metadata(&path).map_or(0, |meta| meta.len());
        std::fs::rename(&path, &dest).map_err(|err| {
            Error::tool("sessions", format!("cannot move session to trash: {err}"))
        })?;
        self.append_ledger(&GcLedgerRecord {
            schema: GC_LEDGER_SCHEMA.to_string(),
            timestamp_ms: now_ms(),
            original_path: path.display().to_string(),
            trash_path: Some(dest.display().to_string()),
            store: GcStoreKind::Sessions,
            size_bytes: size,
            action: "trashed".to_string(),
            reason: "sessions tool delete".to_string(),
        })?;

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Moved {} to the trash ({}). Restore with action=restore.",
                path.display(),
                dest.display()
            )))],
            details: Some(serde_json::json!({
                "schema": "ra.sessions.delete.v1",
                "dryRun": false,
                "path": path.display().to_string(),
                "trashPath": dest.display().to_string(),
                "sizeBytes": size,
            })),
            is_error: false,
        })
    }

    /// `restore`: move a trashed session back to its original directory.
    fn run_restore(&self, input: &SessionsInput) -> Result<ToolOutput> {
        let Some(selector) = input.session.as_deref() else {
            let mut items = collect_trash_files(&self.trash_dir);
            items.sort();
            let text = if items.is_empty() {
                "The trash is empty.".to_string()
            } else {
                items
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            return Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                details: Some(serde_json::json!({
                    "schema": "ra.sessions.restore.v1",
                    "trashed": items.len(),
                })),
                is_error: false,
            });
        };

        let found = find_in_trash(&self.trash_dir, selector).ok_or_else(|| {
            Error::validation(format!("sessions: no trashed session matches {selector:?}"))
        })?;
        // Restore to the recorded origin so the session lands back under its
        // project directory; only an unrecorded trash item falls back to the
        // store root.
        let (dest, used_ledger) = match self.ledger_original_path(&found) {
            Some(original) => (original, true),
            None => (
                self.sessions_root
                    .join(found.file_name().unwrap_or_default()),
                false,
            ),
        };
        if dest.exists() {
            return Err(Error::validation(format!(
                "sessions: refusing to overwrite existing {}",
                dest.display()
            )));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                Error::tool("sessions", format!("cannot create restore dir: {err}"))
            })?;
        }
        let size = std::fs::metadata(&found).map_or(0, |meta| meta.len());
        std::fs::rename(&found, &dest)
            .map_err(|err| Error::tool("sessions", format!("cannot restore session: {err}")))?;
        self.append_ledger(&GcLedgerRecord {
            schema: GC_LEDGER_SCHEMA.to_string(),
            timestamp_ms: now_ms(),
            original_path: found.display().to_string(),
            trash_path: None,
            store: GcStoreKind::Sessions,
            size_bytes: size,
            action: "restored".to_string(),
            reason: format!("restored to {}", dest.display()),
        })?;

        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Restored {} to {}.",
                found.display(),
                dest.display()
            )))],
            details: Some(serde_json::json!({
                "schema": "ra.sessions.restore.v1",
                "from": found.display().to_string(),
                "to": dest.display().to_string(),
                "usedLedger": used_ledger,
            })),
            is_error: false,
        })
    }

    fn append_ledger(&self, record: &GcLedgerRecord) -> Result<()> {
        if let Some(parent) = self.ledger_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                Error::tool("sessions", format!("cannot create ledger dir: {err}"))
            })?;
        }
        let line = serde_json::to_string(record)
            .map_err(|err| Error::tool("sessions", format!("cannot encode ledger row: {err}")))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.ledger_path)
            .map_err(|err| Error::tool("sessions", format!("cannot open ledger: {err}")))?;
        writeln!(file, "{line}")
            .map_err(|err| Error::tool("sessions", format!("cannot write ledger: {err}")))
    }

    /// Look up the recorded origin of a trashed file in the GC ledger.
    fn ledger_original_path(&self, trash_path: &Path) -> Option<PathBuf> {
        let contents = std::fs::read_to_string(&self.ledger_path).ok()?;
        let needle = trash_path.display().to_string();
        let mut found = None;
        for line in contents.lines() {
            let Ok(record) = serde_json::from_str::<GcLedgerRecord>(line) else {
                continue;
            };
            if record.action == "trashed" && record.trash_path.as_deref() == Some(needle.as_str()) {
                found = Some(PathBuf::from(record.original_path));
            }
        }
        found
    }
}

impl Default for SessionsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for SessionsTool {
    fn name(&self) -> &str {
        "sessions"
    }

    fn label(&self) -> &str {
        "sessions"
    }

    fn description(&self) -> &str {
        "Manage stored agent sessions: list, read, search, soft-delete to trash, or restore. \
         Deletion is always recoverable (the file is moved into the GC trash, never \
         unlinked) and refuses named, pinned, active, or live sessions. Reading includes \
         the head of a conversation that compaction dropped from context. Set \
         scope=\"current\" to pin an action to the live conversation so it cannot reach any \
         other session; set session to an id, an id prefix, \"current\", or a session file path."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "read", "search", "delete", "restore"],
                    "description": "Operation to perform."
                },
                "scope": {
                    "type": "string",
                    "enum": ["all", "current"],
                    "description": "`current` pins the call to the live session and forbids cross-session access; `all` (default) may reach the whole store."
                },
                "session": {
                    "type": "string",
                    "description": "Target selector for read/delete/restore: a session id, a unique id prefix, \"current\", or a session file path. Omit on restore to list the trash."
                },
                "query": {
                    "type": "string",
                    "description": "Substring to search for with action=search."
                },
                "project": {
                    "type": "string",
                    "description": "Only sessions whose working directory contains this text (list/search)."
                },
                "since": {
                    "type": "string",
                    "description": "Only sessions modified at/after this RFC 3339 timestamp or YYYY-MM-DD date."
                },
                "until": {
                    "type": "string",
                    "description": "Only sessions modified at/before this RFC 3339 timestamp or YYYY-MM-DD date."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "First message index for action=read (0 reads from the start)."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum rows/matches/messages to return."
                },
                "confirm": {
                    "type": "boolean",
                    "description": "Required true for action=delete; without it the call is a dry run."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn effects(&self) -> ToolEffects {
        // list/read/search only read, but delete/restore mutate the store; the
        // tool declares the write class so the scheduler serializes it.
        ToolEffects::write()
    }

    fn bind_job_session_scope(&mut self, scope: crate::jobs::JobSessionScope) {
        self.job_session_scope = scope;
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: SessionsInput = serde_json::from_value(input)
            .map_err(|err| Error::validation(format!("sessions: invalid input: {err}")))?;
        let scoped_current = match input.scope.as_deref() {
            None => false,
            Some(scope) if scope.eq_ignore_ascii_case("all") => false,
            Some(scope) if scope.eq_ignore_ascii_case("current") => true,
            Some(other) => {
                return Err(Error::validation(format!(
                    "sessions: scope must be \"all\" or \"current\", got {other:?}"
                )));
            }
        };
        let live_id = self.live_session_id().await;
        let live = live_id.as_deref();

        match input.action.to_ascii_lowercase().as_str() {
            "list" => self.run_list(&input, scoped_current, live),
            "read" => self.run_read(&input, scoped_current, live).await,
            "search" => self.run_search(&input, scoped_current, live),
            "delete" => self.run_delete(&input, scoped_current, live),
            "restore" => self.run_restore(&input),
            other => Err(Error::validation(format!(
                "sessions: unknown action {other:?}; expected list, read, search, delete, or restore"
            ))),
        }
    }
}

/// Input accepted by [`SessionsTool`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionsInput {
    action: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    since: Option<String>,
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    confirm: bool,
}

fn looks_like_path(selector: &str) -> bool {
    selector.ends_with(".jsonl") || selector.contains('/') || selector.contains('\\')
}

/// Read the `id` from a session file's first (header) line.
fn header_id(path: &Path) -> Option<String> {
    use std::io::BufRead as _;
    let file = std::fs::File::open(path).ok()?;
    let mut first_line = String::new();
    std::io::BufReader::new(file)
        .read_line(&mut first_line)
        .ok()?;
    let value: serde_json::Value = serde_json::from_str(&first_line).ok()?;
    value
        .get("id")
        .and_then(|id| id.as_str())
        .map(str::to_string)
}

fn meta_matches(
    meta: &SessionMeta,
    input: &SessionsInput,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
) -> bool {
    if let Some(project) = &input.project {
        if !meta.cwd.to_lowercase().contains(&project.to_lowercase()) {
            return false;
        }
    }
    if let Some(since) = since_ms {
        if meta.last_modified_ms < since {
            return false;
        }
    }
    if let Some(until) = until_ms {
        if meta.last_modified_ms > until {
            return false;
        }
    }
    true
}

fn render_meta_line(meta: &SessionMeta) -> String {
    let name = meta.name.as_deref().unwrap_or("-");
    let when = chrono::DateTime::from_timestamp_millis(meta.last_modified_ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| meta.timestamp.clone());
    format!(
        "{id}  name={name}  msgs={msgs}  {size}B  {when}  cwd={cwd}\n    {path}",
        id = meta.id,
        msgs = meta.message_count,
        size = meta.size_bytes,
        cwd = meta.cwd,
        path = meta.path,
    )
}

/// Parse an RFC 3339 timestamp or a `YYYY-MM-DD` day prefix into epoch ms.
fn parse_time_bound(raw: &str) -> Result<i64> {
    let raw = raw.trim();
    if let Ok(datetime) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(datetime.timestamp_millis());
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let datetime = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| Error::validation(format!("sessions: invalid time bound {raw:?}")))?;
        return Ok(datetime.and_utc().timestamp_millis());
    }
    Err(Error::validation(format!(
        "sessions: {raw:?} is not an RFC 3339 timestamp or YYYY-MM-DD date"
    )))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn unique_destination(dir: &Path, source: &Path) -> Result<PathBuf> {
    let file_name = source
        .file_name()
        .ok_or_else(|| Error::tool("sessions", "session file has no name".to_string()))?;
    let mut dest = dir.join(file_name);
    let mut suffix = 1u32;
    while dest.exists() {
        dest = dir.join(format!("{}.{suffix}", file_name.to_string_lossy()));
        suffix += 1;
    }
    Ok(dest)
}

fn collect_trash_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_trash_files_into(dir, &mut out, 0);
    out
}

fn collect_trash_files_into(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_trash_files_into(&path, out, depth + 1);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

fn find_in_trash(dir: &Path, selector: &str) -> Option<PathBuf> {
    let selector = Path::new(selector).file_name().map_or_else(
        || selector.to_string(),
        |name| name.to_string_lossy().to_string(),
    );
    let mut exact = None;
    let mut prefix = None;
    for path in collect_trash_files(dir) {
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().to_string());
        if name == selector {
            return Some(path);
        }
        if name.starts_with(&selector) || name.contains(&selector) {
            if prefix.is_none() {
                prefix = Some(path.clone());
            }
        }
        if exact.is_none() && name.trim_end_matches(".jsonl") == selector {
            exact = Some(path);
        }
    }
    exact.or(prefix)
}

fn clip(text: &str) -> String {
    if text.chars().count() <= SESSIONS_MAX_READ_CHARS {
        return text.to_string();
    }
    let mut out: String = text.chars().take(SESSIONS_MAX_READ_CHARS).collect();
    out.push_str("… (truncated)");
    out
}

fn clip_line(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

fn render_message(message: &crate::model::Message) -> String {
    match message {
        crate::model::Message::User(user) => match &user.content {
            crate::model::UserContent::Text(text) => format!("user: {text}"),
            crate::model::UserContent::Blocks(blocks) => {
                format!("user: {}", blocks_text(blocks))
            }
        },
        crate::model::Message::Assistant(assistant) => {
            format!("assistant: {}", blocks_text(&assistant.content))
        }
        crate::model::Message::ToolResult(result) => {
            format!(
                "tool[{}]: {}",
                result.tool_name,
                blocks_text(&result.content)
            )
        }
        crate::model::Message::Custom(custom) => {
            format!("custom[{}]: {}", custom.custom_type, custom.content)
        }
    }
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                let _ = write!(out, "{}", text.text);
            }
            ContentBlock::ToolCall(call) => {
                let _ = write!(out, "[tool_call {} {}]", call.name, call.arguments);
            }
            ContentBlock::Thinking(_)
            | ContentBlock::RedactedThinking(_)
            | ContentBlock::Image(_)
            | ContentBlock::Media(_) => {}
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(root: &Path, id: &str, body: &str) -> PathBuf {
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join(format!("{id}.jsonl"));
        let header = format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/proj"}}"#
        );
        std::fs::write(&path, format!("{header}\n{body}\n")).expect("write");
        path
    }

    fn tool_in(root: &Path) -> SessionsTool {
        SessionsTool::with_paths(
            root.to_path_buf(),
            root.join("trash"),
            root.join("gc_ledger.jsonl"),
        )
    }

    #[test]
    fn parse_time_bound_accepts_rfc3339_and_day_prefix() {
        assert!(parse_time_bound("2026-01-01T00:00:00Z").is_ok());
        assert!(parse_time_bound("2026-01-01").is_ok());
        assert!(parse_time_bound("yesterday").is_err());
    }

    #[test]
    fn delete_without_confirm_is_a_dry_run_and_leaves_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_session(dir.path(), "abc", r#"{"type":"message"}"#);
        let tool = tool_in(dir.path());
        let input: SessionsInput = serde_json::from_value(serde_json::json!({
            "action": "delete",
            "session": "abc"
        }))
        .expect("input");
        let out = tool.run_delete(&input, false, None).expect("dry run");
        assert!(path.exists(), "dry run must not move the file");
        let text = match &out.content[0] {
            ContentBlock::Text(text) => text.text.clone(),
            _ => String::new(),
        };
        assert!(text.contains("Dry run"), "{text}");
    }

    #[test]
    fn delete_then_restore_round_trips_to_the_original_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_session(dir.path(), "abc", r#"{"type":"message"}"#);
        let tool = tool_in(dir.path());

        let delete: SessionsInput = serde_json::from_value(serde_json::json!({
            "action": "delete",
            "session": "abc",
            "confirm": true
        }))
        .expect("input");
        tool.run_delete(&delete, false, None).expect("delete");
        assert!(!path.exists(), "confirmed delete moves the file out");

        let restore: SessionsInput = serde_json::from_value(serde_json::json!({
            "action": "restore",
            "session": "abc"
        }))
        .expect("input");
        tool.run_restore(&restore).expect("restore");
        assert!(path.exists(), "restore returns the file to its project dir");
    }

    #[test]
    fn delete_refuses_a_pinned_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_session(dir.path(), "abc", r#"{"type":"message"}"#);
        let mut pinned = path.clone();
        pinned.set_extension("pinned");
        std::fs::write(&pinned, b"").expect("pin");
        let tool = tool_in(dir.path());
        let input: SessionsInput = serde_json::from_value(serde_json::json!({
            "action": "delete",
            "session": "abc",
            "confirm": true
        }))
        .expect("input");
        assert!(tool.run_delete(&input, false, None).is_err());
        assert!(path.exists(), "a refused delete must leave the file");
    }

    #[test]
    fn scope_current_fails_closed_without_a_live_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_session(dir.path(), "abc", r#"{"type":"message"}"#);
        let tool = tool_in(dir.path());
        let input: SessionsInput = serde_json::from_value(serde_json::json!({
            "action": "list",
            "scope": "current"
        }))
        .expect("input");
        let err = tool
            .run_list(&input, true, None)
            .expect_err("must fail closed");
        assert!(err.to_string().contains("live session"), "{err}");
    }

    #[test]
    fn blocks_text_renders_text_and_tool_calls() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("hello")),
            ContentBlock::ToolCall(crate::model::ToolCall {
                id: "t1".to_string(),
                name: "read".to_string(),
                arguments: serde_json::json!({"path": "a"}),
                thought_signature: None,
            }),
        ];
        let text = blocks_text(&blocks);
        assert!(text.contains("hello"));
        assert!(text.contains("read"));
    }
}
