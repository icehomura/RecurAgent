//! Skill merger (M4, project-specific — Hermes has no `merge` operation).
//!
//! RecurAgent can accumulate near-duplicate skills as the agent authors more
//! over time. This module clusters candidates by name/description similarity,
//! flags conflicts, and previews a merge. It is deliberately conservative:
//! the dead-priority rule stands — managed skills never shadow user skills,
//! and a merge is *never* performed implicitly. [`plan_merge`] produces a
//! **dry-run preview only**; nothing is written here.
//!
//! Two merge strategies are offered by the preview:
//! - [`MergeStrategy::KeepBoth`] — the safe default. Both skills survive; the
//!   secondary name becomes an alias that resolves to the primary.
//! - [`MergeStrategy::Synthesize`] — fold the secondary body into the primary
//!   (with an attribution section) and leave the secondary name as an alias.
//!
//! "Secondary name still resolves to the merged version" is the acceptance
//! criterion: an alias table (`aliases: {old -> merged}`) records it.

use serde::Serialize;

use crate::resources::Skill;
use crate::skills_managed::ManagedSkillInfo;

/// Tool-result schema tag for merge previews.
pub const SKILL_MERGE_SCHEMA: &str = "ra.skill_merge_dry_run.v1";

/// Default similarity threshold at which two skills are considered merge
/// candidates. Tuned for name tokens rather than prose.
pub const DEFAULT_MERGE_THRESHOLD: f64 = 0.80;

/// How a merge would resolve the two skill bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// Keep both bodies; only record the alias. Never destructive.
    KeepBoth,
    /// Fold the secondary body into the primary under an attribution header.
    Synthesize,
}

/// A pair of skills judged similar enough to merge.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeCandidate {
    /// Skill that would win and keep its name.
    pub primary: String,
    /// Skill whose name would become an alias of the primary.
    pub secondary: String,
    /// Name similarity in `[0.0, 1.0]`.
    pub name_similarity: f64,
    /// Description similarity in `[0.0, 1.0]`.
    pub description_similarity: f64,
    /// Combined score used for ranking.
    pub score: f64,
}

/// A description-level difference worth a human's attention before merging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeConflict {
    /// Machine tag: `description_divergence` or `same_source_tier`.
    pub kind: String,
    /// Human-readable explanation.
    pub detail: String,
}

/// A dry-run merge plan: what *would* happen, with nothing written.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergePlan {
    /// Schema tag.
    pub schema: String,
    /// Always false — this is the contract that nothing was persisted.
    pub applied: bool,
    /// Primary skill name (kept).
    pub primary: String,
    /// Secondary skill name (becomes an alias).
    pub secondary: String,
    /// Strategy the plan would use.
    pub strategy: MergeStrategy,
    /// Combined similarity score.
    pub score: f64,
    /// Conflicts a caller must acknowledge.
    pub conflicts: Vec<MergeConflict>,
    /// Alias mapping (`secondary -> primary`) the applied merge would install.
    pub aliases: Vec<(String, String)>,
    /// Merged body that would be written under the primary name.
    pub merged_body: String,
}

/// Cosine-free similarity on whitespace-delimited lowercase tokens: the
/// Jaccard index over token sets, with a name-equality shortcut so identical
/// names always score 1.0.
///
/// Chosen over edit distance because skill names are
/// short, hyphenated, and token-oriented (`web-search` vs `search-web`).
///
/// The shortcut also compares the *separator-free* form, because the canonical
/// duplicate this feature exists to catch — `web-search` and `websearch`, one
/// concept spelled two ways — shares no token at all and would otherwise score
/// 0.0 on the name.
#[must_use]
pub fn token_similarity(a: &str, b: &str) -> f64 {
    let na = normalize(a);
    let nb = normalize(b);
    if na == nb || strip_separators(a) == strip_separators(b) {
        return 1.0;
    }
    let sa: std::collections::HashSet<&str> = na.split_whitespace().collect();
    let sb: std::collections::HashSet<&str> = nb.split_whitespace().collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let intersection = f64::from(u32::try_from(sa.intersection(&sb).count()).unwrap_or(u32::MAX));
    let union = f64::from(u32::try_from(sa.union(&sb).count()).unwrap_or(u32::MAX));
    intersection / union
}

/// Lowercase and drop every separator, so `web-search` and `websearch` agree.
fn strip_separators(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Lowercase, split on non-alphanumerics so `web-search` and `Web Search`
/// both tokenize to `web search`.
fn normalize(input: &str) -> String {
    input
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Combined similarity: description carries more signal than the name for
/// "same behaviour, different label" duplicates, but a matching name is
/// strong evidence too. Weighted 60/40.
#[must_use]
pub fn candidate_score(name_similarity: f64, description_similarity: f64) -> f64 {
    name_similarity.mul_add(0.4, description_similarity * 0.6)
}

/// Cluster merge candidates across a skill set: for every pair above
/// `threshold`, emit one [`MergeCandidate`] (the higher-precedence skill wins
/// as primary, ties broken by name for determinism).
///
/// Precedence is the array order in `skills` — loaders emit user/project
/// skills first and managed skills dead-last, so an earlier skill is always a
/// safe primary.
#[must_use]
pub fn find_candidates(skills: &[Skill], threshold: f64) -> Vec<MergeCandidate> {
    let mut out = Vec::new();
    for i in 0..skills.len() {
        for j in (i + 1)..skills.len() {
            let a = &skills[i];
            let b = &skills[j];
            let name_similarity = token_similarity(&a.name, &b.name);
            let description_similarity = token_similarity(&a.description, &b.description);
            let score = candidate_score(name_similarity, description_similarity);
            if score < threshold {
                continue;
            }
            // Earlier index wins (higher precedence) — the array is in load
            // order (user/project first, managed dead-last), so index i is
            // always the safe primary.
            let (primary, secondary) = (a.name.clone(), b.name.clone());
            out.push(MergeCandidate {
                primary,
                secondary,
                name_similarity,
                description_similarity,
                score,
            });
        }
    }
    out.sort_by(|x, y| {
        y.score
            .partial_cmp(&x.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| x.primary.cmp(&y.primary))
    });
    out
}

/// Detect conflicts a caller should resolve before merging.
///
/// Managed skills must never shadow user skills, so a cross-tier pair is
/// flagged. A large description divergence means the two skills likely do
/// different things despite a similar name.
#[must_use]
pub fn detect_conflicts(primary: &Skill, secondary: &Skill) -> Vec<MergeConflict> {
    let mut conflicts = Vec::new();
    let desc_sim = token_similarity(&primary.description, &secondary.description);
    if desc_sim < 0.5 {
        conflicts.push(MergeConflict {
            kind: "description_divergence".to_string(),
            detail: format!(
                "descriptions only {:.0}% similar — verify the skills really overlap",
                desc_sim * 100.0
            ),
        });
    }
    if primary.source != secondary.source {
        conflicts.push(MergeConflict {
            kind: "same_source_tier".to_string(),
            detail: format!(
                "sources differ ('{}' vs '{}'); the merge must keep the {} skill as primary \
                 so managed content never shadows user content",
                primary.source, secondary.source, primary.source
            ),
        });
    }
    conflicts
}

/// Build a **dry-run** merge plan for one candidate.
///
/// Nothing is written: the returned [`MergePlan`] has `applied == false` and
/// `aliases` describing the mapping a subsequent (out-of-band) apply would
/// install. When `synthesize` is false the body is left untouched
/// ([`MergeStrategy::KeepBoth`]); when true, `secondary_body` is folded into
/// `primary_body` under an attribution header.
#[must_use]
pub fn plan_merge(
    primary: &Skill,
    secondary: &Skill,
    primary_body: &str,
    secondary_body: &str,
    synthesize: bool,
) -> MergePlan {
    let name_similarity = token_similarity(&primary.name, &secondary.name);
    let description_similarity = token_similarity(&primary.description, &secondary.description);
    let strategy = if synthesize {
        MergeStrategy::Synthesize
    } else {
        MergeStrategy::KeepBoth
    };
    let merged_body = if synthesize {
        format!(
            "{}\n\n---\n\n## Merged from `{}`\n\n{}",
            primary_body.trim_end(),
            secondary.name,
            secondary_body.trim()
        )
    } else {
        primary_body.to_string()
    };
    MergePlan {
        schema: SKILL_MERGE_SCHEMA.to_string(),
        applied: false,
        primary: primary.name.clone(),
        secondary: secondary.name.clone(),
        strategy,
        score: candidate_score(name_similarity, description_similarity),
        conflicts: detect_conflicts(primary, secondary),
        aliases: vec![(secondary.name.clone(), primary.name.clone())],
        merged_body,
    }
}

/// Resolve a skill by name, honouring aliases produced by a prior merge.
///
/// If `name` is an alias key the primary is returned. This is what makes
/// "the old skill name still resolves to the merged version" true.
#[must_use]
pub fn resolve_alias<'a>(name: &str, aliases: &'a [(String, String)]) -> Option<&'a str> {
    aliases
        .iter()
        .find(|(from, _)| from == name)
        .map(|(_, to)| to.as_str())
}

/// Convenience: turn managed-skill metadata into the shallow [`Skill`] shape
/// the clustering functions consume (paths point at the managed dir).
#[must_use]
pub fn skill_from_managed(info: &ManagedSkillInfo) -> Skill {
    let path = std::path::PathBuf::from(&info.path);
    let base_dir = path.parent().map_or_else(
        || std::path::PathBuf::from("."),
        std::path::Path::to_path_buf,
    );
    crate::resources::Skill {
        name: info.name.clone(),
        description: info.description.clone(),
        file_path: path,
        base_dir,
        source: "managed".to_string(),
        disable_model_invocation: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skill(name: &str, description: &str, source: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            file_path: PathBuf::from(format!("/tmp/{name}/SKILL.md")),
            base_dir: PathBuf::from(format!("/tmp/{name}")),
            source: source.to_string(),
            disable_model_invocation: false,
        }
    }

    #[test]
    fn token_similarity_handles_hyphens_and_case() {
        assert_eq!(token_similarity("web-search", "web search"), 1.0);
        assert_eq!(token_similarity("Web-Search", "web-search"), 1.0);
        assert!(token_similarity("web-search", "web-scrape") < 1.0);
        assert!(token_similarity("web-search", "web-scrape") > 0.0);
        assert_eq!(token_similarity("", "x"), 0.0);
    }

    #[test]
    fn clustering_finds_near_duplicates_only() {
        let skills = vec![
            skill("web-search", "search the web for information", "user"),
            skill("websearch", "search the web for information", "user"),
            skill("csv-export", "export tabular data to csv files", "user"),
        ];
        let found = find_candidates(&skills, DEFAULT_MERGE_THRESHOLD);
        assert_eq!(
            found.len(),
            1,
            "only the web pair should cluster: {found:?}"
        );
        assert_eq!(found[0].primary, "web-search");
        assert_eq!(found[0].secondary, "websearch");

        // A permissive threshold pulls in the unrelated skill too. The
        // unrelated pair shares no token in either name or description, so its
        // score is exactly 0.0 — only a threshold at or below that admits it,
        // which is why 0.1 could never satisfy this assertion.
        let all = find_candidates(&skills, 0.0);
        assert!(all.len() >= 2, "permissive threshold found {all:?}");
    }

    #[test]
    fn candidate_keeps_higher_precedence_skill_as_primary() {
        // Managed skill is second in load order → user skill stays primary.
        let skills = vec![
            skill("deploy-app", "deploy the app to production", "user"),
            skill("deploy-app", "deploy the app to production", "managed"),
        ];
        let found = find_candidates(&skills, DEFAULT_MERGE_THRESHOLD);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].primary, "deploy-app");
        assert_eq!(found[0].secondary, "deploy-app");
        assert_eq!(found[0].score, 1.0);
    }

    #[test]
    fn dry_run_plan_writes_nothing_and_lists_alias() {
        let primary = skill("web-search", "search the web for information", "user");
        let secondary = skill("websearch", "search the web for information", "managed");
        let plan = plan_merge(&primary, &secondary, "Primary body", "Secondary body", true);
        assert!(!plan.applied, "dry-run must not apply");
        assert_eq!(plan.schema, SKILL_MERGE_SCHEMA);
        assert_eq!(
            plan.aliases,
            vec![("websearch".to_string(), "web-search".to_string())]
        );
        assert!(plan.merged_body.contains("Primary body"));
        assert!(plan.merged_body.contains("Merged from `websearch`"));
        assert!(plan.merged_body.contains("Secondary body"));
        // Cross-tier: the user skill must be primary, with a conflict noted.
        assert_eq!(plan.primary, "web-search");
        assert!(plan.conflicts.iter().any(|c| c.kind == "same_source_tier"));
    }

    #[test]
    fn keep_both_strategy_leaves_body_untouched() {
        let primary = skill("a-skill", "does a thing", "user");
        let secondary = skill("a-skill-2", "does a thing", "user");
        let plan = plan_merge(&primary, &secondary, "body a", "body b", false);
        assert_eq!(plan.strategy, MergeStrategy::KeepBoth);
        assert_eq!(plan.merged_body, "body a");
        assert_eq!(
            plan.aliases,
            vec![("a-skill-2".to_string(), "a-skill".to_string())]
        );
        assert!(
            plan.conflicts.is_empty(),
            "same tier, same desc → no conflict"
        );
    }

    #[test]
    fn description_divergence_is_flagged() {
        let primary = skill("data-tool", "export tabular data to csv files", "user");
        let secondary = skill("data-tool", "train a neural network model", "user");
        let conflicts = detect_conflicts(&primary, &secondary);
        assert!(conflicts.iter().any(|c| c.kind == "description_divergence"));
    }

    #[test]
    fn alias_resolves_old_name_to_merged() {
        let aliases = vec![("websearch".to_string(), "web-search".to_string())];
        assert_eq!(resolve_alias("websearch", &aliases), Some("web-search"));
        assert_eq!(resolve_alias("web-search", &aliases), None);
        assert_eq!(resolve_alias("other", &aliases), None);
    }
}
