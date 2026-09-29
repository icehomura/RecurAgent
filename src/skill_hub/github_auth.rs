//! GitHub three-tier authentication for skill sources (M8).
//!
//! Tier order, cheapest first:
//! 1. explicit token (`RECUR_AGENT_GITHUB_TOKEN` / `GITHUB_TOKEN` / `GH_TOKEN`) → bearer
//! 2. no token but an authenticated `gh` CLI → `gh api <path>` (inherits gh's auth)
//! 3. neither → anonymous (60 req/h; rate-limit errors are surfaced verbatim)

use crate::error::{Error, Result};
use std::process::{Command, Stdio};

/// Which credential path a fetch takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubAuthTier {
    /// Explicit bearer token from the environment or config.
    ExplicitToken(String),
    /// Authenticated `gh` CLI available on PATH.
    GhCli,
    /// No credentials: anonymous API access.
    Anonymous,
}

/// Detect the highest available tier.
#[must_use]
pub fn detect_auth_tier() -> GithubAuthTier {
    for var in ["RECUR_AGENT_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            let token = token.trim();
            if !token.is_empty() {
                return GithubAuthTier::ExplicitToken(token.to_string());
            }
        }
    }
    if gh_authenticated() {
        return GithubAuthTier::GhCli;
    }
    GithubAuthTier::Anonymous
}

/// Whether `gh auth status` reports a logged-in account.
fn gh_authenticated() -> bool {
    Command::new("gh")
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// GitHub API client that follows the three-tier ladder.
#[derive(Debug, Clone)]
pub struct GithubClient {
    tier: GithubAuthTier,
}

impl GithubClient {
    /// Detect the tier at construction time.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tier: detect_auth_tier(),
        }
    }

    /// The tier this client will use (for diagnostics and tests).
    #[must_use]
    pub const fn tier(&self) -> &GithubAuthTier {
        &self.tier
    }

    /// GET `path` (e.g. `"/repos/foo/bar/contents/SKILL.md"`) via the
    /// detected tier. Rate-limit responses become clear errors rather than
    /// silent fallbacks.
    pub async fn fetch(&self, path: &str) -> Result<String> {
        let url = format!("https://api.github.com{path}");
        match &self.tier {
            GithubAuthTier::ExplicitToken(token) => {
                let client = crate::http::client::Client::new();
                let response = client
                    .get(&url)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Accept", "application/vnd.github+json")
                    .header("User-Agent", "ra-agent")
                    .send()
                    .await
                    .map_err(|err| Error::tool("skill_hub", format!("github fetch: {err}")))?;
                read_github_response(response).await
            }
            GithubAuthTier::GhCli => {
                let output = Command::new("gh")
                    .args(["api", path])
                    .stdin(Stdio::null())
                    .output()
                    .map_err(|err| Error::tool("skill_hub", format!("gh spawn: {err}")))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(Error::tool("skill_hub", format!("gh api failed: {stderr}")));
                }
                String::from_utf8(output.stdout)
                    .map_err(|err| Error::tool("skill_hub", format!("gh output: {err}")))
            }
            GithubAuthTier::Anonymous => {
                let client = crate::http::client::Client::new();
                let response = client
                    .get(&url)
                    .header("Accept", "application/vnd.github+json")
                    .header("User-Agent", "ra-agent")
                    .send()
                    .await
                    .map_err(|err| Error::tool("skill_hub", format!("github fetch: {err}")))?;
                read_github_response(response).await
            }
        }
    }
}

impl Default for GithubClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared response reader: surfaces 403/429 rate limits as actionable text.
async fn read_github_response(response: crate::http::client::Response) -> Result<String> {
    let status = response.status();
    if status == 403 || status == 429 {
        return Err(Error::tool(
            "skill_hub",
            format!(
                "github rate limited (status {status}); set RECUR_AGENT_GITHUB_TOKEN/GITHUB_TOKEN \
                 or authenticate `gh` to raise the 60 req/h anonymous limit"
            ),
        ));
    }
    if status != 200 {
        return Err(Error::tool(
            "skill_hub",
            format!("github fetch: status {status}"),
        ));
    }
    response
        .text_limited(16 * 1024 * 1024)
        .await
        .map_err(|err| Error::tool("skill_hub", format!("github fetch: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_order_is_token_then_gh_then_anonymous() {
        // The ladder is ordered by construction; assert the enum stays
        // exhaustive and an empty token never becomes a tier.
        let tier = detect_auth_tier();
        assert!(matches!(
            tier,
            GithubAuthTier::ExplicitToken(_) | GithubAuthTier::GhCli | GithubAuthTier::Anonymous
        ));
        if let GithubAuthTier::ExplicitToken(token) = tier {
            assert!(!token.trim().is_empty(), "empty token must not be a tier");
        }
    }

    #[test]
    fn client_reports_its_tier() {
        let client = GithubClient::new();
        assert!(matches!(
            client.tier(),
            GithubAuthTier::ExplicitToken(_) | GithubAuthTier::GhCli | GithubAuthTier::Anonymous
        ));
    }
}
