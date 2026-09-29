//! Managed skills (bd-cv653.4.2).
//!
//! The agent can author skills at runtime into the isolated managed dir
//! (`<global_dir>/skills.managed/<name>/SKILL.md`). Managed skills load
//! dead-last in precedence (resources.rs adds the tier after packages), so
//! user and project skills always win collisions — `manage_skill` can never
//! shadow or mutate user-authored work, and every mutation requires the
//! `managed: true` frontmatter marker plus lands in an audit ledger.
//!
//! `learn` captures a lesson (memory kind=lesson, bd-cv653.4.1) and can
//! promote it into a managed skill; drafts that fail the lint gate (same
//! validators as the skills loader) are kept as lessons only, with the
//! warning surfaced.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};

/// Tool-result schema tag for managed-skill operations.
pub const SKILL_SCHEMA: &str = "ra.managed_skill.v1";

/// Schema tag for skill revision-history records.
pub const SKILL_REVISION_SCHEMA: &str = "ra.skill_revision.v1";

/// Directory the managed tier loads from (dead-last precedence).
#[must_use]
pub fn managed_skills_dir() -> PathBuf {
    crate::config::Config::global_dir().join("skills.managed")
}

/// One managed skill with its provenance.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedSkillInfo {
    pub schema: String,
    pub name: String,
    pub description: String,
    pub path: String,
    pub managed: bool,
}

/// Lint a skill draft with the same validators the skills loader applies.
/// Returns the list of violations (empty = valid).
#[must_use]
pub fn lint_skill_draft(name: &str, description: &str, fields: &[&str]) -> Vec<String> {
    let mut errors = crate::resources::validate_name(name, name);
    errors.extend(crate::resources::validate_description(description));
    let owned: Vec<String> = fields.iter().map(|field| (*field).to_string()).collect();
    errors.extend(crate::resources::validate_frontmatter_fields(owned.iter()));
    errors
}

fn skill_dir(name: &str) -> PathBuf {
    managed_skills_dir().join(name)
}

fn skill_file(name: &str) -> PathBuf {
    skill_dir(name).join("SKILL.md")
}

/// Append-only revision log living beside the skill's `SKILL.md`.
fn revisions_file(name: &str) -> PathBuf {
    skill_dir(name).join(".revisions.jsonl")
}

/// SHA-256 hex digest of skill file content.
fn content_hash(content: &str) -> String {
    use sha2::Digest as _;
    crate::package_manager::hex_encode(&sha2::Sha256::digest(content.as_bytes()))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// One managed-skill revision: the content hash before and after a write.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillRevision {
    pub schema: String,
    pub skill_name: String,
    pub old_content_hash: String,
    pub new_content_hash: String,
    pub timestamp_ms: i64,
}

/// Append one revision record to `.revisions.jsonl` (append-only — prior
/// records are never rewritten).
///
/// # Errors
/// IO or encode failures writing the revision log.
fn append_revision(revision: &SkillRevision) -> Result<()> {
    use std::io::Write as _;
    let mut payload = serde_json::to_vec(revision)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to encode revision: {e}")))?;
    payload.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(revisions_file(&revision.skill_name))
        .map_err(|e| Error::tool("manage_skill", format!("Failed to open revision log: {e}")))?;
    file.write_all(&payload)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to append revision: {e}")))?;
    Ok(())
}

/// Read a skill's revision history, oldest first.
///
/// # Errors
/// IO errors reading the log, or a malformed JSONL record.
pub fn revision_history(name: &str) -> Result<Vec<SkillRevision>> {
    let file = revisions_file(name);
    if !file.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&file)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to read revision log: {e}")))?;
    let mut out = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let revision: SkillRevision = serde_json::from_str(line).map_err(|e| {
            Error::tool(
                "manage_skill",
                format!(
                    "Malformed revision record at {}:{}: {e}",
                    file.display(),
                    idx + 1
                ),
            )
        })?;
        out.push(revision);
    }
    Ok(out)
}

/// Render a SKILL.md with the managed marker.
fn render_skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\nmanaged: true\n---\n\n{body}\n")
}

/// Read the frontmatter map of an existing SKILL.md (light parse: the
/// `key: value` lines inside the first `---` fence).
fn frontmatter_of(path: &Path) -> Option<std::collections::HashMap<String, String>> {
    let raw = crate::resources::read_resource_file_bounded(path, "skill").ok()?;
    let mut fields = std::collections::HashMap::new();
    let mut inside = false;
    for line in raw.lines() {
        if line.trim() == "---" {
            if inside {
                break;
            }
            inside = true;
            continue;
        }
        if inside && let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    Some(fields)
}

fn is_managed(path: &Path) -> bool {
    frontmatter_of(path)
        .and_then(|fields| fields.get("managed").cloned())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

fn audit(op: &str, name: &str, rationale: Option<&str>, session_id: Option<&str>) {
    static AUDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = AUDIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = managed_skills_dir();
    let _ = std::fs::create_dir_all(&dir);
    let entry = serde_json::json!({
        "op": op,
        "name": name,
        "rationale": rationale,
        "sessionId": session_id,
        "atMs": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
    });
    let mut payload = serde_json::to_vec(&entry).unwrap_or_default();
    payload.push(b'\n');
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("audit.jsonl"))
    {
        use std::io::Write as _;
        let _ = file.write_all(&payload);
    }
}

/// Create a managed skill. The draft is linted first; invalid drafts are
/// refused with the violations listed (the caller keeps the lesson).
///
/// # Errors
/// Named `RECUR_AGENT_SKILL_EXISTS` when a live skill already has the name;
/// `RECUR_AGENT_SKILL_INVALID` with the lint violations.
pub fn create(name: &str, description: &str, body: &str) -> Result<ManagedSkillInfo> {
    let violations = lint_skill_draft(name, description, &["name", "description", "managed"]);
    if !violations.is_empty() {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "RECUR_AGENT_SKILL_INVALID: skill draft failed the lint gate: {}",
                violations.join("; ")
            ),
        ));
    }
    let dir = skill_dir(name);
    let file = skill_file(name);
    if file.exists() {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "RECUR_AGENT_SKILL_EXISTS: a skill named '{name}' already exists at {}",
                file.display()
            ),
        ));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to create skill dir: {e}")))?;
    let rendered = render_skill_md(name, description, body);
    std::fs::write(&file, &rendered)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to write skill: {e}")))?;
    // Baseline revision: creation has no prior content, so the old hash is
    // the digest of the empty string.
    append_revision(&SkillRevision {
        schema: SKILL_REVISION_SCHEMA.to_string(),
        skill_name: name.to_string(),
        old_content_hash: content_hash(""),
        new_content_hash: content_hash(&rendered),
        timestamp_ms: now_ms(),
    })?;
    audit("create", name, None, None);
    Ok(ManagedSkillInfo {
        schema: SKILL_SCHEMA.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        path: file.display().to_string(),
        managed: true,
    })
}

/// Update a managed skill's body (description kept unless provided).
///
/// # Errors
/// `RECUR_AGENT_SKILL_UNKNOWN` when absent; `RECUR_AGENT_SKILL_NOT_MANAGED` when the marker
/// is missing (user-authored content — never touched).
pub fn update(name: &str, description: Option<&str>, body: &str) -> Result<ManagedSkillInfo> {
    let file = skill_file(name);
    if !file.exists() {
        return Err(Error::tool(
            "manage_skill",
            format!("RECUR_AGENT_SKILL_UNKNOWN: no managed skill named '{name}'"),
        ));
    }
    if !is_managed(&file) {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "RECUR_AGENT_SKILL_NOT_MANAGED: '{}' lacks the managed marker — refusing to mutate \
                 user-authored content",
                file.display()
            ),
        ));
    }
    let existing = frontmatter_of(&file).unwrap_or_default();
    let description = description
        .map(str::to_string)
        .or_else(|| existing.get("description").cloned())
        .unwrap_or_default();
    let old_raw = std::fs::read_to_string(&file)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to read skill: {e}")))?;
    write_revision(name, &old_raw, &description, body, "update")
}

/// Read a managed skill's current description and body (frontmatter stripped).
///
/// # Errors
/// `RECUR_AGENT_SKILL_UNKNOWN` when absent.
fn read_current(name: &str) -> Result<(String, String)> {
    let file = skill_file(name);
    if !file.exists() {
        return Err(Error::tool(
            "manage_skill",
            format!("RECUR_AGENT_SKILL_UNKNOWN: no managed skill named '{name}'"),
        ));
    }
    let raw = std::fs::read_to_string(&file)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to read skill: {e}")))?;
    let description = frontmatter_of(&file)
        .and_then(|fields| fields.get("description").cloned())
        .unwrap_or_default();
    Ok((description, strip_frontmatter_body(&raw)))
}

/// Strip the leading `---` frontmatter fence, returning the body only.
fn strip_frontmatter_body(raw: &str) -> String {
    let mut it = raw.lines();
    if it.next().map(str::trim) != Some("---") {
        return raw.to_string();
    }
    let mut body = Vec::new();
    let mut closed = false;
    for line in it {
        if !closed && line.trim() == "---" {
            closed = true;
            continue;
        }
        if closed {
            body.push(line);
        }
    }
    if !closed {
        return raw.to_string();
    }
    // Trim the single leading blank line render_skill_md inserts.
    body.join("\n").trim_start_matches('\n').to_string()
}

/// Shared write path for `update`/`patch`: lint, snapshot, write, log revision.
fn write_revision(
    name: &str,
    old_raw: &str,
    description: &str,
    body: &str,
    op: &str,
) -> Result<ManagedSkillInfo> {
    let violations = lint_skill_draft(name, description, &["name", "description", "managed"]);
    if !violations.is_empty() {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "RECUR_AGENT_SKILL_INVALID: {op} draft failed the lint gate: {}",
                violations.join("; ")
            ),
        ));
    }
    let file = skill_file(name);
    let new_raw = render_skill_md(name, description, body);
    std::fs::write(&file, &new_raw)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to write skill: {e}")))?;
    append_revision(&SkillRevision {
        schema: SKILL_REVISION_SCHEMA.to_string(),
        skill_name: name.to_string(),
        old_content_hash: content_hash(old_raw),
        new_content_hash: content_hash(&new_raw),
        timestamp_ms: now_ms(),
    })?;
    audit(op, name, None, None);
    Ok(ManagedSkillInfo {
        schema: SKILL_SCHEMA.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        path: file.display().to_string(),
        managed: true,
    })
}

/// Patch a managed skill in place: replace the first exact occurrence of
/// `old_text` with `new_text`, leaving the rest — and the description —
/// untouched.
///
/// This is the token-cheap revision path (Hermes' preferred `patch`): the
/// caller sends only the diff, not the whole body. The prior revision is
/// snapshotted in `.revisions.jsonl`, so an erroneous patch is rollback-able.
///
/// # Errors
/// `RECUR_AGENT_SKILL_UNKNOWN` / `RECUR_AGENT_SKILL_NOT_MANAGED`; `RECUR_AGENT_SKILL_PATCH_NO_MATCH`
/// when `old_text` is absent or not unique; `RECUR_AGENT_SKILL_INVALID` on lint.
pub fn patch(name: &str, old_text: &str, new_text: &str) -> Result<ManagedSkillInfo> {
    let file = skill_file(name);
    if !file.exists() {
        return Err(Error::tool(
            "manage_skill",
            format!("RECUR_AGENT_SKILL_UNKNOWN: no managed skill named '{name}'"),
        ));
    }
    if !is_managed(&file) {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "RECUR_AGENT_SKILL_NOT_MANAGED: '{}' lacks the managed marker — refusing to mutate \
                 user-authored content",
                file.display()
            ),
        ));
    }
    if old_text.is_empty() {
        return Err(Error::tool(
            "manage_skill",
            "RECUR_AGENT_SKILL_PATCH_NO_MATCH: oldText must not be empty".to_string(),
        ));
    }
    let (description, body) = read_current(name)?;
    let occurrences = body.matches(old_text).count();
    if occurrences == 0 {
        return Err(Error::tool(
            "manage_skill",
            format!("RECUR_AGENT_SKILL_PATCH_NO_MATCH: oldText not found in '{name}'"),
        ));
    }
    if occurrences > 1 {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "RECUR_AGENT_SKILL_PATCH_NO_MATCH: oldText appears {occurrences} times in '{name}' — \
                 provide a larger, unique context"
            ),
        ));
    }
    let patched = body.replacen(old_text, new_text, 1);
    let old_raw = std::fs::read_to_string(&file)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to read skill: {e}")))?;
    write_revision(name, &old_raw, &description, &patched, "patch")
}

/// Delete a managed skill directory. Refuses anything lacking the managed
/// marker (user-authored content is untouchable).
///
/// # Errors
/// `RECUR_AGENT_SKILL_UNKNOWN` / `RECUR_AGENT_SKILL_NOT_MANAGED`.
pub fn delete(name: &str) -> Result<()> {
    let dir = skill_dir(name);
    let file = skill_file(name);
    if !file.exists() {
        return Err(Error::tool(
            "manage_skill",
            format!("RECUR_AGENT_SKILL_UNKNOWN: no managed skill named '{name}'"),
        ));
    }
    if !is_managed(&file) {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "RECUR_AGENT_SKILL_NOT_MANAGED: '{}' lacks the managed marker — refusing to delete \
                 user-authored content",
                file.display()
            ),
        ));
    }
    std::fs::remove_dir_all(&dir)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to delete skill: {e}")))?;
    audit("delete", name, None, None);
    Ok(())
}

/// List managed skills with provenance.
///
/// # Errors
/// IO errors reading the managed dir.
pub fn list() -> Result<Vec<ManagedSkillInfo>> {
    let dir = managed_skills_dir();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to read managed dir: {e}")))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let file = path.join("SKILL.md");
        if !file.exists() {
            continue;
        }
        let fields = frontmatter_of(&file).unwrap_or_default();
        let name = fields
            .get("name")
            .cloned()
            .or_else(|| entry.file_name().to_str().map(str::to_string))
            .unwrap_or_default();
        out.push(ManagedSkillInfo {
            schema: SKILL_SCHEMA.to_string(),
            name,
            description: fields.get("description").cloned().unwrap_or_default(),
            path: file.display().to_string(),
            managed: is_managed(&file),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_name(tag: &str) -> String {
        format!("pi-test-{tag}-{}", std::process::id())
    }

    #[test]
    fn lint_gate_rejects_bad_names_and_descriptions() {
        assert!(!lint_skill_draft("Bad Name", "ok", &["name", "description"]).is_empty());
        assert!(!lint_skill_draft("ok-name", "", &["name", "description"]).is_empty());
        assert!(!lint_skill_draft(&"x".repeat(65), "ok", &["name", "description"]).is_empty());
        assert!(
            lint_skill_draft("good-name", "a real description", &["name", "description"])
                .is_empty()
        );
    }

    #[test]
    fn create_list_update_delete_cycle() {
        let name = unique_name("cycle");
        let info = create(&name, "cycle skill", "body one").expect("create");
        assert!(info.managed);
        assert!(std::path::Path::new(&info.path).exists());

        let listed = list().expect("list");
        assert!(listed.iter().any(|skill| skill.name == name));

        let updated = update(&name, None, "body two").expect("update");
        assert_eq!(updated.description, "cycle skill");
        let raw = std::fs::read_to_string(&info.path).expect("read");
        assert!(raw.contains("body two"));
        assert!(raw.contains("managed: true"));

        delete(&name).expect("delete");
        assert!(!std::path::Path::new(&info.path).exists());
    }

    #[test]
    fn delete_refuses_unmanaged_content() {
        let name = unique_name("unmanaged");
        let dir = skill_dir(&name);
        std::fs::create_dir_all(&dir).expect("dir");
        // User-authored-looking SKILL.md: no managed marker.
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: user skill\n---\n\nbody\n"),
        )
        .expect("write");
        let err = delete(&name).unwrap_err();
        assert!(
            err.to_string().contains("RECUR_AGENT_SKILL_NOT_MANAGED"),
            "expected refusal: {err}"
        );
        let err = update(&name, None, "hijack").unwrap_err();
        assert!(
            err.to_string().contains("RECUR_AGENT_SKILL_NOT_MANAGED"),
            "expected refusal: {err}"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn create_refuses_invalid_drafts() {
        let err = create("Bad Name", "desc", "body").unwrap_err();
        assert!(
            err.to_string().contains("RECUR_AGENT_SKILL_INVALID"),
            "expected lint refusal: {err}"
        );
    }

    #[test]
    fn create_then_update_appends_two_revisions() {
        let name = unique_name("revision");
        create(&name, "revision skill", "body one").expect("create");
        update(&name, None, "body two").expect("update");

        let history = revision_history(&name).expect("revision history");
        assert_eq!(history.len(), 2, "expected baseline + update revisions");
        assert!(
            history
                .iter()
                .all(|rev| rev.schema == SKILL_REVISION_SCHEMA)
        );
        assert!(history.iter().all(|rev| rev.skill_name == name));
        // The update's old hash chains onto the creation's new hash.
        assert_eq!(history[1].old_content_hash, history[0].new_content_hash);
        assert_ne!(history[0].old_content_hash, history[0].new_content_hash);
        assert_ne!(history[1].old_content_hash, history[1].new_content_hash);
        assert!(history.iter().all(|rev| rev.timestamp_ms > 0));
    }

    #[test]
    fn patch_replaces_only_the_named_snippet() {
        let name = unique_name("patch");
        create(&name, "patch skill", "alpha beta gamma").expect("create");
        patch(&name, "beta", "BETA").expect("patch");

        let raw = std::fs::read_to_string(skill_file(&name)).expect("read");
        assert!(
            raw.contains("alpha BETA gamma"),
            "patch should be surgical: {raw}"
        );
        // Description survives the patch untouched.
        assert!(raw.contains("description: patch skill"));

        // The patch is recorded, chaining onto the create revision.
        let history = revision_history(&name).expect("history");
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].old_content_hash, history[0].new_content_hash);
        assert_ne!(history[1].old_content_hash, history[1].new_content_hash);

        delete(&name).expect("delete");
    }

    #[test]
    fn patch_refuses_missing_or_ambiguous_text() {
        let name = unique_name("patch-miss");
        create(&name, "patch skill", "one two two three").expect("create");

        let err = patch(&name, "absent", "x").unwrap_err();
        assert!(err.to_string().contains("RECUR_AGENT_SKILL_PATCH_NO_MATCH"));
        let err = patch(&name, "two", "2").unwrap_err();
        assert!(err.to_string().contains("appears 2 times"), "got {err}");
        let err = patch(&name, "", "x").unwrap_err();
        assert!(err.to_string().contains("RECUR_AGENT_SKILL_PATCH_NO_MATCH"));
        let err = patch(&name, "  ", "x").unwrap_err();
        assert!(err.to_string().contains("RECUR_AGENT_SKILL_PATCH_NO_MATCH"));

        delete(&name).expect("delete");
    }

    #[test]
    fn patch_refuses_unmanaged_and_unknown() {
        let name = unique_name("patch-unmanaged");
        let dir = skill_dir(&name);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: user skill\n---\n\nbody\n"),
        )
        .expect("write");
        let err = patch(&name, "body", "hijack").unwrap_err();
        assert!(err.to_string().contains("RECUR_AGENT_SKILL_NOT_MANAGED"));
        std::fs::remove_dir_all(&dir).expect("cleanup");

        let err = patch("pi-test-nonexistent", "a", "b").unwrap_err();
        assert!(err.to_string().contains("RECUR_AGENT_SKILL_UNKNOWN"));
    }

    #[test]
    fn read_current_strips_frontmatter() {
        let name = unique_name("read-current");
        create(&name, "read skill", "body line one\nbody line two").expect("create");
        let (description, body) = read_current(&name).expect("read");
        assert_eq!(description, "read skill");
        assert_eq!(body, "body line one\nbody line two");
        assert!(!body.contains("---"));
        assert!(!body.contains("managed"));
        delete(&name).expect("delete");
    }
}
