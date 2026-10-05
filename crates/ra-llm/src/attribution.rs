//! Provider attribution headers that every transport shares.
//!
//! Transports ask this module whether an endpoint needs a provider-specific
//! routing/attribution header rather than each growing its own copy of the
//! rules.
//!
//! Today it carries the OpenCode per-conversation session header, which the
//! OpenCode Zen and OpenCode Go gateways require before they will route a
//! request: a request missing it may error outright, and unattributed traffic
//! risks account bans or service degradation, so every request must carry the
//! live conversation's id.

/// The OpenCode per-conversation routing header, when the endpoint belongs to
/// OpenCode (by provider label or by `opencode.ai` host).
pub const OPENCODE_SESSION_HEADER: &str = "x-opencode-session";

/// Whether `provider` or `base_url` identifies an OpenCode gateway (Zen or Go).
///
/// The host check catches lanes fronting `opencode.ai` under a provider label
/// this module does not otherwise know.
pub fn is_opencode_endpoint(provider: &str, base_url: &str) -> bool {
    is_opencode_provider(provider) || matches_host(base_url, "opencode.ai")
}

/// Attribution headers the target endpoint requires; empty for every other
/// endpoint.
///
/// `session_id` is the live conversation's id; the header is omitted when
/// there is no session to attribute (a fabricated id would misattribute
/// unrelated conversations).
pub fn opencode_headers(
    provider: &str,
    base_url: &str,
    session_id: Option<&str>,
) -> Vec<(&'static str, String)> {
    if !is_opencode_endpoint(provider, base_url) {
        return Vec::new();
    }
    session_id
        .filter(|id| !id.is_empty())
        .map(|id| vec![(OPENCODE_SESSION_HEADER, id.to_string())])
        .unwrap_or_default()
}

/// The live conversation's session id, when a router turn scope wraps the
/// caller. `None` outside such a scope, so transports simply omit the
/// attribution header instead of failing.
pub fn current_session_id() -> Option<String> {
    crate::adaptive::current_router_context()
        .session_id
        .filter(|id| !id.is_empty())
}

fn is_opencode_provider(provider: &str) -> bool {
    matches!(
        provider.trim().to_ascii_lowercase().as_str(),
        "opencode" | "opencode-go" | "opencode-zen"
    )
}

fn matches_host(base_url: &str, expected: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| host.eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_detect_opencode_provider_labels_case_insensitively() {
        for provider in ["opencode", "OPENCODE-GO", "opencode-zen"] {
            assert!(
                is_opencode_endpoint(provider, "https://example.com/v1"),
                "{provider} must be treated as OpenCode"
            );
        }
    }

    #[test]
    fn should_detect_opencode_by_host_for_unknown_provider_labels() {
        for base_url in ["https://opencode.ai/zen/v1", "https://opencode.ai/zen/go/v1"] {
            assert!(
                is_opencode_endpoint("unrelated-lane", base_url),
                "{base_url} must be treated as OpenCode"
            );
        }
    }

    #[test]
    fn should_not_flag_unrelated_endpoints() {
        assert!(!is_opencode_endpoint("openai", "https://api.openai.com/v1"));
        assert!(!is_opencode_endpoint("anthropic", ""));
        assert!(!is_opencode_endpoint("opencode-ish", "not a url"));
    }

    #[test]
    fn should_emit_session_header_only_when_a_session_exists() {
        assert_eq!(
            opencode_headers("opencode", "https://opencode.ai/zen/v1", Some("sess-1")),
            vec![(OPENCODE_SESSION_HEADER, "sess-1".to_string())]
        );
        assert!(opencode_headers("opencode", "", None).is_empty());
        assert!(opencode_headers("opencode", "", Some("")).is_empty());
    }

    #[test]
    fn should_emit_no_headers_for_non_opencode_endpoints() {
        assert!(
            opencode_headers("openai", "https://api.openai.com/v1", Some("sess-1")).is_empty()
        );
        assert!(
            opencode_headers("anthropic", "https://api.anthropic.com", Some("sess-1")).is_empty()
        );
    }
}
