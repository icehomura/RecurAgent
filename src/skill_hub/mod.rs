//! M8 skill hub: remote skill discovery, install-time safety and hot reload.
//!
//! One-tier source is agentskills.io (static, token-free); GitHub-class
//! sources authenticate in three tiers; installation is gated by
//! [`quarantine`]; prompt refresh flows through [`reload`].

pub mod github_auth;
pub mod hub;
pub mod quarantine;
pub mod reload;
pub mod source;

pub use hub::{InstallOutcome, SearchHit, SkillHub, rank_matches};
pub use quarantine::{QuarantineReport, Verdict, scan_skill};
pub use reload::{SkillsReloadHandle, SkillsReloadOutcome};
pub use source::{AgentskillsSource, SkillIndex, SkillMeta, SkillPackage, SkillSource};

/// Settings under the `skillHub` key (M8). Off by default: the hub tools
/// reach the network and write to the shared skills dir, so they never join
/// the registry unless `enable` is explicitly true.
///
/// ```json
/// "skillHub": { "enable": true }
/// ```
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SkillHubSettings {
    /// Master switch for `skill_hub_search` / `skill_hub_install`.
    #[serde(alias = "enable")]
    pub enable: Option<bool>,
}

impl SkillHubSettings {
    /// Whether the hub tools are enabled (default: false).
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        matches!(self.enable, Some(true))
    }
}

/// Whether the M8 hub tools should join the registry.
///
/// Opt-in by construction: neither `skill_hub_search` nor `skill_hub_install`
/// exists unless the caller turns one of these on.
///
/// * `settings` — the parsed `skillHub` block, when the config carries one.
/// * `RECUR_AGENT_SKILL_HUB` — `1`/`true`/`on` forces the tools on (environment escape
///   hatch for hosts that do not surface the settings file).
#[must_use]
pub fn hub_tools_enabled(settings: Option<&SkillHubSettings>) -> bool {
    if settings.is_some_and(SkillHubSettings::is_enabled) {
        return true;
    }
    std::env::var("RECUR_AGENT_SKILL_HUB").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        )
    })
}

/// Read the `skillHub` block from the project (then global) `settings.json`.
///
/// The hub gate must be cheap and must not depend on the fully-merged
/// `Config` (the tool registry is built before some config consumers run), so
/// it reads the two settings files directly and takes the first that carries a
/// `skillHub` key — project wins, matching the normal precedence.
#[must_use]
pub fn settings_from_disk(cwd: &std::path::Path) -> Option<SkillHubSettings> {
    use crate::config::{Config, SettingsScope};
    let project =
        Config::settings_path_with_roots(SettingsScope::Project, &Config::global_dir(), cwd);
    let global =
        Config::settings_path_with_roots(SettingsScope::Global, &Config::global_dir(), cwd);
    for path in [project, global] {
        if let Some(block) = read_skill_hub_block(&path) {
            return Some(block);
        }
    }
    None
}

/// Parse the `skillHub` key out of one settings file, if present.
fn read_skill_hub_block(path: &std::path::Path) -> Option<SkillHubSettings> {
    let body = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    let block = value.get("skillHub")?;
    serde_json::from_value(block.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_defaults_to_false() {
        assert!(!SkillHubSettings::default().is_enabled());
        assert!(!SkillHubSettings { enable: None }.is_enabled());
        assert!(
            !SkillHubSettings {
                enable: Some(false)
            }
            .is_enabled()
        );
        assert!(SkillHubSettings { enable: Some(true) }.is_enabled());
    }

    #[test]
    fn block_without_settings_is_off() {
        assert!(!hub_tools_enabled(None));
        assert!(!hub_tools_enabled(Some(&SkillHubSettings {
            enable: Some(false)
        })));
    }

    #[test]
    fn reads_skill_hub_block_from_settings_file() {
        let dir = std::env::temp_dir().join(format!("pi_hub_settings_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{ "skillHub": { "enable": true } }"#).expect("write");
        let block = read_skill_hub_block(&path).expect("block");
        assert!(block.is_enabled());
        std::fs::write(&path, r#"{ "theme": "dark" }"#).expect("write");
        assert!(read_skill_hub_block(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
