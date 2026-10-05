use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::openai::OpenAIProvider;
use crate::openai_responses::OpenAIResponsesProvider;
use crate::provider::LlmProvider;

use super::opencode::{Dialect, anthropic_root, dialect};
use super::{CreateParams, ProviderEntry};

/// The default OpenAI-compatible API root for the OpenCode Go tier: the same
/// gateway as Zen, one path segment deeper. ONE const behind the ENTRY
/// declaration and the `create` fallback.
const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "opencode-go",
    aliases: &[],
    // The Go tier shares the gateway's single credential.
    api_key_env: Some("OPENCODE_API_KEY"),
    key_env_aliases: &[],
    default_base_url: Some(DEFAULT_BASE_URL),
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    // OpenCode hosts many vendors' models behind one gateway — no simple
    // detect pattern.
    detect_patterns: &[],
    model_discovery: crate::discovery::OPENAI_MODELS,
    model_discovery_for_model: None,
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let key = p
        .api_key
        .ok_or_else(|| eyre::eyre!("OPENCODE_API_KEY not set"))?;
    let model = p
        .model
        .or_else(|| ENTRY.default_model().map(str::to_string))
        .ok_or_else(|| {
            eyre::eyre!(
                "{}: no model given and the catalog declares no default for this family",
                ENTRY.name
            )
        })?;
    let url = p.base_url.unwrap_or_else(|| DEFAULT_BASE_URL.into());

    // Dialect tables and the `/v1` rewrite live in `super::opencode`: Zen and
    // Go are the same gateway under two tier roots, so re-encoding the split
    // here would let the tiers drift apart.
    let provider: Arc<dyn LlmProvider> = match dialect(ENTRY.name, &model) {
        Dialect::Anthropic => {
            let mut provider = AnthropicProvider::new(&key, &model)
                .with_provider_label(ENTRY.name)
                .with_base_url(anthropic_root(&url));
            if let Some((t, c)) = http_timeout {
                provider = provider.with_http_timeout(t, c);
            }
            Arc::new(provider)
        }
        Dialect::Responses => {
            let mut provider = OpenAIResponsesProvider::new(&key, &model).with_base_url(&url);
            if let Some((t, c)) = http_timeout {
                provider = provider.with_http_timeout(t, c);
            }
            Arc::new(provider)
        }
        Dialect::OpenAi => {
            let mut provider = OpenAIProvider::new(&key, &model)
                .with_provider_label(ENTRY.name)
                .with_base_url(&url);
            if let Some(hints) = p.model_hints {
                provider = provider.with_hints(hints);
            }
            if let Some((t, c)) = http_timeout {
                provider = provider.with_http_timeout(t, c);
            }
            Arc::new(provider)
        }
        // The Go tier declares no Gemini dialect — `dialect` cannot return
        // this for `opencode-go` (pinned by `go_tier_has_no_gemini_dialect`).
        // Refuse rather than guess if a future table change makes it reachable.
        Dialect::Gemini => eyre::bail!("{} does not serve a Gemini dialect", ENTRY.name),
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ApiStyle;
    use crate::registry::lookup;
    use crate::registry::opencode::prefers_anthropic;

    fn build(model: &str) -> Arc<dyn LlmProvider> {
        create(CreateParams {
            api_key: Some("test-key".into()),
            model: Some(model.into()),
            base_url: None,
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        })
        .unwrap_or_else(|e| panic!("create({model}) must succeed: {e}"))
    }

    /// The Go-tier split: the exact Anthropic ids, the Responses prefixes, and
    /// everything else OpenAI-compatible. Notably the tier has NO `claude-*`
    /// Anthropic lane and no Gemini lane — ids that speak those protocols on
    /// Zen stay on the OpenAI-compatible endpoint here.
    #[test]
    fn go_dialect_split_covers_every_lane() {
        for model in ["minimax-m3", "qwen3.8-flash"] {
            assert_eq!(dialect("opencode-go", model), Dialect::Anthropic, "{model}");
        }
        for model in ["gpt-5.1", "gpt-5.1-codex", "grok-4", "muse-spark-1"] {
            assert_eq!(dialect("opencode-go", model), Dialect::Responses, "{model}");
        }
        for model in [
            "gemini-2.5-flash",
            "claude-sonnet-4-6",
            "deepseek-v4-flash",
            "minimax-m2.5",
        ] {
            assert_eq!(dialect("opencode-go", model), Dialect::OpenAi, "{model}");
        }
    }

    /// The Anthropic id table is exact and case-sensitive, like the Zen
    /// `claude-*` prefix — pricing classifies the lane with this same
    /// predicate.
    #[test]
    fn go_dialect_split_is_case_sensitive() {
        assert_eq!(dialect("opencode-go", "MiniMax-M3"), Dialect::OpenAi);
        assert_eq!(dialect("opencode-go", "Qwen3.8-Flash"), Dialect::OpenAi);
        assert_eq!(dialect("opencode-go", "GPT-5"), Dialect::OpenAi);
        assert!(prefers_anthropic("opencode-go", "minimax-m3"));
        assert!(!prefers_anthropic("opencode-go", "MiniMax-M3"));
    }

    /// The tier must never resolve to the Gemini provider — the shared
    /// classifier has no Go Gemini lane, and `create` refuses that arm rather
    /// than guessing a protocol.
    #[test]
    fn go_tier_has_no_gemini_dialect() {
        for model in ["gemini-2.5-flash", "gemini-3-pro", "gemini-3-pro-preview"] {
            assert_ne!(dialect("opencode-go", model), Dialect::Gemini, "{model}");
        }
    }

    /// The Go Anthropic root drops the OpenAI-compatible `/v1` segment, so the
    /// provider speaks `https://opencode.ai/zen/go/v1/messages`.
    #[test]
    fn go_anthropic_root_drops_the_openai_version_suffix() {
        assert_eq!(
            anthropic_root(DEFAULT_BASE_URL),
            "https://opencode.ai/zen/go"
        );
    }

    /// The family resolves by its canonical name and keeps the Zen alias
    /// pointing at the Zen tier.
    #[test]
    fn the_go_family_resolves_by_name() {
        let entry = lookup("opencode-go").expect("opencode-go registered");
        assert_eq!(entry.name, "opencode-go");
        assert_eq!(entry.default_base_url, Some(DEFAULT_BASE_URL));
        assert_eq!(entry.api_key_env, Some("OPENCODE_API_KEY"));
        assert_eq!(lookup("opencode-zen").map(|e| e.name), Some("opencode"));
    }

    /// Each dialect builds a different provider type, and the label carried by
    /// the transports that have one is exactly `opencode-go`.
    #[test]
    fn create_builds_the_provider_that_speaks_the_selected_dialect() {
        let anthropic = build("minimax-m3");
        assert_eq!(anthropic.provider_name(), "opencode-go");
        assert_eq!(anthropic.api_style(), Some(ApiStyle::AnthropicMessages));

        let responses = build("gpt-5.1");
        assert_eq!(responses.api_style(), Some(ApiStyle::OpenAiResponses));

        let openai = build("deepseek-v4-flash");
        // `OpenAIProvider::with_base_url` tags the non-default root onto the
        // label (`opencode-go@opencode`); the Anthropic lane keeps it bare.
        assert_eq!(openai.provider_name(), "opencode-go@opencode");
        assert_eq!(openai.api_style(), Some(ApiStyle::OpenAiChatCompletions));
    }
}
