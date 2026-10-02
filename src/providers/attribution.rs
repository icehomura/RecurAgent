//! Provider attribution headers that every transport shares.
//!
//! Mirrors pi's `mergeProviderAttributionHeaders`: transports ask this module
//! whether a model needs a provider-specific routing/attribution header rather
//! than each growing its own copy of the rules.
//!
//! Today it carries the OpenCode per-conversation session header, which the
//! OpenCode Zen and OpenCode Go gateways require before they will route a
//! request.

/// The OpenCode per-conversation routing header, when `model` belongs to
/// OpenCode (by provider id or by `opencode.ai` host).
///
/// `session_id` is the live conversation's id; the header is omitted when there
/// is no session to attribute.
pub(super) fn opencode_session_header(
    provider: &str,
    base_url: &str,
    session_id: Option<&str>,
) -> Option<(&'static str, String)> {
    let session_id = session_id?;
    let is_opencode = provider.eq_ignore_ascii_case("opencode")
        || provider.eq_ignore_ascii_case("opencode-go")
        || matches_host(base_url, "opencode.ai");
    is_opencode.then(|| ("x-opencode-session", session_id.to_string()))
}

fn matches_host(base_url: &str, expected: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| host.eq_ignore_ascii_case(expected))
}
