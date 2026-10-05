use std::sync::Arc;

use eyre::Result;

use crate::anthropic::AnthropicProvider;
use crate::gemini::GeminiProvider;
use crate::openai::OpenAIProvider;
use crate::openai_responses::OpenAIResponsesProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// The default OpenAI-compatible API root for OpenCode Zen. ONE const behind
/// the ENTRY declaration and the `create` fallback so the family default can
/// never drift between serving and probing.
const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/v1";

/// The wire protocol OpenCode routes a model over.
///
/// OpenCode is a GATEWAY: which protocol a model speaks is a property of the
/// model id, not of the family — so construction (and the cache-pricing
/// classifier in `crate::pricing`) switch on this, never on the family alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dialect {
    /// Anthropic Messages API (`{root}/v1/messages`).
    Anthropic,
    /// OpenAI Responses API (`{root}/responses`).
    Responses,
    /// Google Gemini `generateContent` (`{root}/models/{model}:{action}`).
    Gemini,
    /// OpenAI Chat Completions (`{root}/chat/completions`).
    OpenAi,
}

/// Zen model ids OpenCode serves over the Anthropic Messages API besides the
/// `claude-*` family. Exact-match by design: these are whole ids, so a future
/// sibling like `qwen3.7-max-preview` must NOT inherit the Anthropic lane.
const ZEN_ANTHROPIC_IDS: &[&str] = &[
    "qwen3.5-plus",
    "qwen3.6-plus",
    "qwen3.7-plus",
    "qwen3.7-max",
    "qwen3.8-flash",
];

/// Go-tier model ids OpenCode serves over the Anthropic Messages API. The Go
/// tier declares no `claude-*` / `gemini-*` lanes of its own — its Claude
/// traffic is served over the OpenAI-compatible endpoint.
const GO_ANTHROPIC_IDS: &[&str] = &["minimax-m3", "qwen3.8-flash"];

/// Whether OpenCode serves `model` over the OpenAI Responses API. Shared by
/// both tiers: the Responses prefix set is the same on Zen and Go.
fn serves_responses(model: &str) -> bool {
    model.starts_with("gpt-") || model.starts_with("grok-") || model.starts_with("muse-spark-")
}

/// The protocol OpenCode serves `model` over for `family`.
///
/// Keyed on the RAW model id (case-sensitive): OpenCode's routing is
/// case-sensitive, so a mixed-case `Claude-3` is NOT the `claude-*` lane and
/// must be built as an OpenAI-protocol request. Pricing classifies the lane
/// through the same function — case-folding here would hand a model Anthropic
/// cache rates it never earns.
pub(super) fn dialect(family: &str, model: &str) -> Dialect {
    let anthropic = match family {
        "opencode" => model.starts_with("claude-") || ZEN_ANTHROPIC_IDS.contains(&model),
        "opencode-go" => GO_ANTHROPIC_IDS.contains(&model),
        // Only the two OpenCode tiers carry these tables; anything else keeps
        // the OpenAI-compatible default rather than guessing a lane.
        _ => false,
    };
    if anthropic {
        Dialect::Anthropic
    } else if serves_responses(model) {
        Dialect::Responses
    } else if family == "opencode" && model.starts_with("gemini-") {
        Dialect::Gemini
    } else {
        Dialect::OpenAi
    }
}

/// Whether OpenCode serves `model` over the Anthropic Messages API for
/// `family`.
///
/// The SINGLE source of truth for that split so provider construction (below)
/// and the cache-pricing classifier (`crate::pricing`) can never diverge — a
/// divergence would price an OpenAI-protocol model at Anthropic cache rates
/// (or vice versa).
pub(crate) fn prefers_anthropic(family: &str, model: &str) -> bool {
    dialect(family, model) == Dialect::Anthropic
}

/// The Anthropic-protocol API root: OpenCode serves the Messages API at
/// `{root}/v1/messages`, while the declared base URL already ends in the
/// OpenAI-compatible `/v1` segment — drop that suffix and let
/// `AnthropicProvider` re-append `/v1/messages`. ONE rewrite shared by both
/// tiers so Zen (`.../zen`) and Go (`.../zen/go`) can never drift.
pub(super) fn anthropic_root(base_url: &str) -> String {
    base_url.strip_suffix("/v1").unwrap_or(base_url).to_string()
}

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "opencode",
    // `opencode-zen` is how the Zen tier is written in omp-generated configs;
    // keep it resolving to the canonical gateway family.
    aliases: &["opencode-zen"],
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

    // The provider TYPE is the wire protocol, so the per-model choice happens
    // here (see [`dialect`]) — at the same seam `crate::pricing` reads the
    // label back from.
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
        Dialect::Gemini => {
            let mut provider = GeminiProvider::new(&key, &model).with_base_url(&url);
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
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ApiStyle;
    use crate::registry::lookup;

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

    /// The Zen split: `claude-*` + the exact qwen ids speak Anthropic, the
    /// vendor prefixes speak Responses, `gemini-*` speaks Gemini, everything
    /// else stays OpenAI-compatible.
    #[test]
    fn zen_dialect_split_covers_every_lane() {
        for model in [
            "claude-sonnet-4-6",
            "claude-opus-4-1",
            "qwen3.5-plus",
            "qwen3.6-plus",
            "qwen3.7-plus",
            "qwen3.7-max",
            "qwen3.8-flash",
        ] {
            assert_eq!(dialect("opencode", model), Dialect::Anthropic, "{model}");
        }
        for model in ["gpt-5.1", "gpt-5.1-codex", "grok-4", "muse-spark-1"] {
            assert_eq!(dialect("opencode", model), Dialect::Responses, "{model}");
        }
        for model in ["gemini-2.5-flash", "gemini-3-pro"] {
            assert_eq!(dialect("opencode", model), Dialect::Gemini, "{model}");
        }
        // `minimax-m3` is an Anthropic id on the Go tier only — the family
        // argument must rule, never the id alone.
        for model in [
            "deepseek-v4-flash",
            "kimi-k2.5",
            "qwen3-coder",
            "minimax-m3",
        ] {
            assert_eq!(dialect("opencode", model), Dialect::OpenAi, "{model}");
        }
    }

    /// A mixed-case id is NOT the `claude-*` lane: construction and pricing
    /// must agree that `Claude-3` is served over OpenAI.
    #[test]
    fn zen_dialect_split_is_case_sensitive() {
        assert_eq!(dialect("opencode", "Claude-3"), Dialect::OpenAi);
        assert_eq!(dialect("opencode", "Qwen3.7-Max"), Dialect::OpenAi);
        assert_eq!(dialect("opencode", "GPT-5"), Dialect::OpenAi);
        assert_eq!(dialect("opencode", "Gemini-2.5-flash"), Dialect::OpenAi);
        assert!(!prefers_anthropic("opencode", "Claude-3"));
        assert!(prefers_anthropic("opencode", "claude-3"));
    }

    /// The OpenAI-compatible root carries `/v1`; the Anthropic root does not
    /// (the provider re-appends `/v1/messages`).
    #[test]
    fn anthropic_root_drops_the_openai_version_suffix() {
        assert_eq!(anthropic_root(DEFAULT_BASE_URL), "https://opencode.ai/zen");
        // A root without the suffix is left alone — no double rewrite.
        assert_eq!(
            anthropic_root("https://opencode.ai/zen"),
            "https://opencode.ai/zen"
        );
    }

    /// The omp registry id `opencode-zen` must resolve to the canonical
    /// `opencode` family with the Zen root and credential.
    #[test]
    fn the_zen_alias_resolves_to_the_opencode_family() {
        let alias = lookup("opencode-zen").expect("opencode-zen alias registered");
        assert_eq!(alias.name, "opencode");
        assert_eq!(alias.default_base_url, Some(DEFAULT_BASE_URL));
        assert_eq!(alias.api_key_env, Some("OPENCODE_API_KEY"));
        assert_eq!(lookup("opencode").map(|e| e.name), Some("opencode"));
    }

    /// Each dialect builds a DIFFERENT provider type — the wire protocol is
    /// chosen at construction, and the label stays `opencode` where the
    /// transport carries one.
    #[test]
    fn create_builds_the_provider_that_speaks_the_selected_dialect() {
        let anthropic = build("claude-sonnet-4-6");
        assert_eq!(anthropic.provider_name(), "opencode");
        assert_eq!(anthropic.api_style(), Some(ApiStyle::AnthropicMessages));

        let responses = build("gpt-5.1");
        assert_eq!(responses.api_style(), Some(ApiStyle::OpenAiResponses));

        let gemini = build("gemini-2.5-flash");
        assert_eq!(gemini.api_style(), Some(ApiStyle::GeminiGenerateContent));

        let openai = build("deepseek-v4-flash");
        // `OpenAIProvider::with_base_url` tags a non-default root onto the
        // label for router identification (`opencode@opencode`); the Anthropic
        // lane keeps the bare label.
        assert_eq!(openai.provider_name(), "opencode@opencode");
        assert_eq!(openai.api_style(), Some(ApiStyle::OpenAiChatCompletions));
    }

    /// A missing credential fails construction with the OpenCode env var name,
    /// not a generic message — the operator needs to know which key to set.
    #[test]
    fn create_without_a_key_names_the_opencode_env_var() {
        let error = create(CreateParams {
            api_key: None,
            model: Some("claude-sonnet-4-6".into()),
            base_url: None,
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        })
        .err()
        .expect("no key must fail");
        assert!(error.to_string().contains("OPENCODE_API_KEY"), "{error}");
    }
}
