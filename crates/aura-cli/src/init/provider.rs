//! Provider identity and per-provider metadata: the canonical id, the
//! conventional API-key env var, the recommended model families, and the
//! small constants shared across the `init` flow.

/// The LLM providers `init` supports. Variant order is the canonical display
/// order (also the no-keys-found order). `clap::ValueEnum` parses and validates
/// `--provider` and lists the choices in `--help`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Provider {
    OpenAI,
    Anthropic,
    Bedrock,
    Gemini,
    Ollama,
    OpenRouter,
}

impl Provider {
    /// All providers in canonical (display) order.
    pub(crate) const ALL: &'static [Provider] = &[
        Provider::OpenAI,
        Provider::Anthropic,
        Provider::Bedrock,
        Provider::Gemini,
        Provider::Ollama,
        Provider::OpenRouter,
    ];

    /// Canonical lowercase id — matches the config's `provider` tag.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Provider::OpenAI => "openai",
            Provider::Anthropic => "anthropic",
            Provider::Bedrock => "bedrock",
            Provider::Gemini => "gemini",
            Provider::Ollama => "ollama",
            Provider::OpenRouter => "openrouter",
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub(crate) const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Whether the provider's model-list endpoint authenticates with the API key,
/// so a successful response actually verifies it. OpenRouter's `/models` is
/// public and Ollama's `/api/tags` is local — neither checks a key.
pub(crate) fn list_verifies_key(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::OpenAI | Provider::Anthropic | Provider::Gemini
    )
}

/// Default API-key env var per provider (None = provider needs no key).
pub(crate) fn default_key_env(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::OpenAI => Some("OPENAI_API_KEY"),
        Provider::Anthropic => Some("ANTHROPIC_API_KEY"),
        Provider::Gemini => Some("GEMINI_API_KEY"),
        Provider::OpenRouter => Some("OPENROUTER_API_KEY"),
        Provider::Bedrock | Provider::Ollama => None,
    }
}

/// Recommended model ids per provider, best-first — the first is the suggested
/// default. Each entry is matched as a prefix against the live list, and within
/// a match `rank_shortlist` prefers the clean (non-dated) id, else the newest.
/// OpenRouter and Ollama are intentionally uncurated (see `rank_shortlist`):
/// OpenRouter users have their own opinions over a huge catalog, and Ollama
/// lists whatever is installed locally. Updating these lists is a deliberate
/// editorial choice — keep them current as providers ship new flagships.
pub(crate) fn family_roots(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::OpenAI => &["gpt-5.5"],
        Provider::Anthropic => &["claude-sonnet-4-6", "claude-opus-4-8", "claude-haiku-4-5"],
        Provider::Gemini => &["gemini-3.5-flash", "gemini-3.1-pro"],
        // Uncurated providers (and bedrock, which has no list endpoint).
        Provider::OpenRouter | Provider::Ollama | Provider::Bedrock => &[],
    }
}
