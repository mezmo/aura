//! `aura-cli init` — generate a starter configuration.
//!
//! The flow the product spec describes:
//!
//! 1. **Sense** conventional API-key env vars (OPENAI_API_KEY, …).
//! 2. **Provider**: exactly one key found → suggested as the default;
//!    several → list prioritized by the ones found; none → default order.
//! 3. **Verify** the key by querying the provider's live model-list
//!    endpoint (blocking HTTP, short timeout; bedrock has no cheap HTTP
//!    listing and is skipped with a note).
//! 4. **Model**: display the fetched list and suggest a default via a
//!    small per-provider preference table ranked against the live list —
//!    a suggestion the operator accepts or overrides, never a silent pick.
//! 5. Write a minimal **complete** config: a placeholder assistant on the
//!    verified `[agent.llm]` (key as an `{{ env.VAR }}` reference) with
//!    `[bootstrap] enabled = true`, so the aura-bootstrap agent can build
//!    out the real configuration conversationally.
//!
//! Verification is best-effort: network or key failures warn and continue
//! (`--offline` skips the attempt entirely); init never hard-blocks on the
//! network. Output is deterministic given the same choices.

use std::io::{BufRead, IsTerminal, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Default provider order (also the no-keys-found display order).
const PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "bedrock",
    "gemini",
    "ollama",
    "openrouter",
];

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Pinned Anthropic API version header for the models endpoint.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Default API-key env var per provider (None = provider needs no key).
fn default_key_env(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "gemini" => Some("GEMINI_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        _ => None,
    }
}

/// Ordered model-preference patterns per provider: the suggested default is
/// the first live model matching the earliest pattern (exact match preferred,
/// then the shortest prefix match — the base model rather than a variant).
/// These are suggestions ranked against what the key can actually use; the
/// operator always confirms or overrides.
fn preference_patterns(provider: &str) -> &'static [&'static str] {
    match provider {
        "openai" => &["gpt-5.1", "gpt-5", "gpt-4.1", "gpt-4o"],
        "anthropic" => &["claude-sonnet-4", "claude-opus-4", "claude-haiku-4"],
        "gemini" => &["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash"],
        "openrouter" => &[
            "openai/gpt-5.1",
            "anthropic/claude-sonnet-4",
            "openai/gpt-5",
        ],
        "ollama" => &["qwen3", "llama3", "mistral"],
        _ => &[],
    }
}

/// Substrings that mark an entry in a provider's model list as not a chat
/// model (embeddings, audio, images, …) for display purposes.
const NON_CHAT_MARKERS: &[&str] = &[
    "embedding",
    "whisper",
    "tts",
    "dall-e",
    "audio",
    "realtime",
    "moderation",
    "transcribe",
    "image",
    "davinci",
    "babbage",
];

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Output path for the generated config
    #[arg(long, short = 'o', default_value = "config.toml")]
    pub output: PathBuf,

    /// LLM provider (openai, anthropic, bedrock, gemini, ollama, openrouter)
    #[arg(long)]
    pub provider: Option<String>,

    /// Model name (verified against the provider's model list when possible)
    #[arg(long)]
    pub model: Option<String>,

    /// Name of the environment variable holding the API key, written into
    /// the config as a `{{ env.VAR }}` reference. Defaults per provider
    /// (e.g. OPENAI_API_KEY); not used for bedrock/ollama.
    #[arg(long)]
    pub api_key_env: Option<String>,

    /// AWS region (bedrock only)
    #[arg(long)]
    pub region: Option<String>,

    /// Base URL (ollama only; default http://localhost:11434)
    #[arg(long)]
    pub base_url: Option<String>,

    /// Agent name written to the config
    #[arg(long, default_value = "assistant")]
    pub name: String,

    /// Skip live model-list verification entirely (air-gapped / CI)
    #[arg(long)]
    pub offline: bool,

    /// Fail on missing required values instead of prompting (automatic
    /// when stdin is not a terminal)
    #[arg(long)]
    pub non_interactive: bool,

    /// Overwrite the output file if it exists
    #[arg(long)]
    pub force: bool,
}

// ============================================================================
// Model listing (behind a trait so tests inject fakes — no live HTTP in
// `cargo test`)
// ============================================================================

/// Outcome of a model-list attempt.
pub enum ModelList {
    /// Models fetched — the key works.
    Verified(Vec<String>),
    /// This provider has no cheap HTTP listing (bedrock).
    Unsupported,
}

pub trait ModelLister {
    /// List model ids for the provider. `Err` carries a human-readable
    /// reason (bad key, no network, …) — callers warn and continue.
    fn list(
        &self,
        provider: &str,
        api_key: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<ModelList, String>;
}

/// Live `reqwest::blocking` implementation (5s timeout).
pub struct HttpModelLister;

impl HttpModelLister {
    fn client() -> Result<reqwest::blocking::Client, String> {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("http client: {e}"))
    }

    fn get_json(request: reqwest::blocking::RequestBuilder) -> Result<serde_json::Value, String> {
        let response = request.send().map_err(|e| format!("request failed: {e}"))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(format!(
                "the provider rejected the API key ({status}) — is the right \
                 variable exported in this shell?"
            ));
        }
        if !status.is_success() {
            return Err(format!("unexpected response: {status}"));
        }
        response
            .json::<serde_json::Value>()
            .map_err(|e| format!("invalid JSON response: {e}"))
    }

    /// Pull a list of ids out of `json[field][*][id_key]`.
    fn extract(json: &serde_json::Value, field: &str, id_key: &str) -> Vec<String> {
        json[field]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|m| m[id_key].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl ModelLister for HttpModelLister {
    fn list(
        &self,
        provider: &str,
        api_key: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<ModelList, String> {
        let key = api_key.unwrap_or_default();
        let models = match provider {
            "openai" => Self::extract(
                &Self::get_json(
                    Self::client()?
                        .get("https://api.openai.com/v1/models")
                        .bearer_auth(key),
                )?,
                "data",
                "id",
            ),
            "anthropic" => Self::extract(
                &Self::get_json(
                    Self::client()?
                        .get("https://api.anthropic.com/v1/models")
                        .header("x-api-key", key)
                        .header("anthropic-version", ANTHROPIC_VERSION),
                )?,
                "data",
                "id",
            ),
            "openrouter" => Self::extract(
                &Self::get_json(Self::client()?.get("https://openrouter.ai/api/v1/models"))?,
                "data",
                "id",
            ),
            "gemini" => Self::extract(
                &Self::get_json(
                    Self::client()?
                        .get("https://generativelanguage.googleapis.com/v1beta/models")
                        .header("x-goog-api-key", key),
                )?,
                "models",
                "name",
            )
            .into_iter()
            .map(|name| name.trim_start_matches("models/").to_string())
            .collect(),
            "ollama" => Self::extract(
                &Self::get_json(Self::client()?.get(format!(
                    "{}/api/tags",
                    base_url.unwrap_or(DEFAULT_OLLAMA_URL).trim_end_matches('/')
                )))?,
                "models",
                "name",
            ),
            // Bedrock needs the AWS SDK (ListFoundationModels); skipped in v1.
            "bedrock" => return Ok(ModelList::Unsupported),
            other => return Err(format!("unknown provider '{other}'")),
        };
        if models.is_empty() {
            return Err("the provider returned an empty model list".to_string());
        }
        Ok(ModelList::Verified(models))
    }
}

// ============================================================================
// Pure selection logic (unit-tested)
// ============================================================================

/// Providers whose conventional key env var is set, in PROVIDERS order.
/// `is_set` is injected so tests don't touch the process environment.
fn sensed_providers(is_set: &dyn Fn(&str) -> bool) -> Vec<&'static str> {
    PROVIDERS
        .iter()
        .copied()
        .filter(|p| default_key_env(p).is_some_and(is_set))
        .collect()
}

/// Provider display order: sensed providers first, then the rest.
fn provider_display_order(sensed: &[&'static str]) -> Vec<&'static str> {
    let mut order: Vec<&'static str> = sensed.to_vec();
    order.extend(PROVIDERS.iter().copied().filter(|p| !sensed.contains(p)));
    order
}

/// Drop obviously-non-chat entries from a provider's model list (display
/// only; the operator can still type any model).
fn filter_chat_models(models: &[String]) -> Vec<String> {
    models
        .iter()
        .filter(|m| {
            let lower = m.to_lowercase();
            !NON_CHAT_MARKERS.iter().any(|marker| lower.contains(marker))
        })
        .cloned()
        .collect()
}

/// The suggested default: first pattern with a live match; exact match
/// preferred, then the shortest id with the pattern as prefix (the base
/// model rather than a dated/variant id).
fn suggest_model(provider: &str, models: &[String]) -> Option<String> {
    for pattern in preference_patterns(provider) {
        if let Some(exact) = models.iter().find(|m| m == pattern) {
            return Some(exact.clone());
        }
        if let Some(shortest) = models
            .iter()
            .filter(|m| m.starts_with(pattern))
            .min_by_key(|m| m.len())
        {
            return Some(shortest.clone());
        }
    }
    None
}

// ============================================================================
// Interactive prompting
// ============================================================================

/// Interactive prompt helper. All prompts go through here so the resolution
/// logic stays testable without a terminal.
struct Prompter<R: BufRead> {
    interactive: bool,
    stdin: R,
}

impl<R: BufRead> Prompter<R> {
    /// Ask a question with an optional default. Returns `None` when
    /// non-interactive (the caller decides whether that's fatal).
    fn ask(&mut self, question: &str, default: Option<&str>) -> Result<Option<String>> {
        if !self.interactive {
            return Ok(default.map(String::from));
        }
        match default {
            Some(d) => print!("{question} [{d}]: "),
            None => print!("{question}: "),
        }
        std::io::stdout().flush()?;
        let mut line = String::new();
        self.stdin.read_line(&mut line)?;
        let answer = line.trim();
        if answer.is_empty() {
            Ok(default.map(String::from))
        } else {
            Ok(Some(answer.to_string()))
        }
    }

    /// Ask with no default; in non-interactive mode a missing value is an
    /// error naming the flag that would have provided it.
    fn require(&mut self, question: &str, flag: &str) -> Result<String> {
        match self.ask(question, None)? {
            Some(v) => Ok(v),
            None => bail!("{flag} is required in non-interactive mode"),
        }
    }
}

// ============================================================================
// Resolution + rendering
// ============================================================================

/// Fully resolved inputs (after flags, sensing, verification, prompts).
#[derive(Debug)]
struct ConfigSpec {
    provider: String,
    model: String,
    api_key_env: Option<String>,
    region: Option<String>,
    base_url: Option<String>,
    name: String,
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn render_config(spec: &ConfigSpec) -> String {
    let mut llm = format!("provider = \"{}\"\n", toml_escape(&spec.provider));
    if let Some(var) = &spec.api_key_env {
        llm.push_str(&format!("api_key = \"{{{{ env.{var} }}}}\"\n"));
    }
    llm.push_str(&format!("model = \"{}\"\n", toml_escape(&spec.model)));
    if let Some(region) = &spec.region {
        llm.push_str(&format!("region = \"{}\"\n", toml_escape(region)));
    }
    if let Some(base_url) = &spec.base_url {
        llm.push_str(&format!("base_url = \"{}\"\n", toml_escape(base_url)));
    }

    format!(
        "# Generated by `aura-cli init`.\n\
         # This is a minimal starting point: a placeholder assistant plus the\n\
         # aura-bootstrap agent, which builds out the real configuration\n\
         # conversationally and applies changes without a restart.\n\
         \n\
         [agent]\n\
         name = \"{name}\"\n\
         system_prompt = \"\"\"\n\
         You are a helpful general-purpose assistant.\n\
         \n\
         (This is a placeholder configuration. Chat with the aura-bootstrap\n\
         agent to replace it with a real one, or edit this file directly.)\n\
         \"\"\"\n\
         \n\
         [agent.llm]\n\
         {llm}\
         \n\
         [bootstrap]\n\
         enabled = true\n",
        name = toml_escape(&spec.name),
    )
}

/// Resolve all inputs. Network access only through `lister`; environment
/// reads only through `key_is_set`.
fn resolve_spec<R: BufRead>(
    args: &InitArgs,
    prompter: &mut Prompter<R>,
    lister: &dyn ModelLister,
    key_is_set: &dyn Fn(&str) -> bool,
) -> Result<ConfigSpec> {
    // ---- provider (sense keys → suggest) ----
    let sensed = sensed_providers(key_is_set);
    let provider = match &args.provider {
        Some(p) => p.trim().to_lowercase(),
        None => {
            let order = provider_display_order(&sensed);
            let default = if sensed.len() == 1 {
                Some(sensed[0])
            } else {
                None
            };
            if prompter.interactive {
                println!("Providers:");
                for (i, p) in order.iter().enumerate() {
                    let marker = if sensed.contains(p) {
                        "  (key detected)"
                    } else {
                        ""
                    };
                    println!("  {}. {p}{marker}", i + 1);
                }
            }
            let answer = match prompter.ask("Provider (number or name)", default)? {
                Some(a) => a,
                None => bail!("--provider is required in non-interactive mode"),
            };
            match answer.trim().parse::<usize>() {
                Ok(n) if (1..=order.len()).contains(&n) => order[n - 1].to_string(),
                _ => answer.trim().to_lowercase(),
            }
        }
    };
    if !PROVIDERS.contains(&provider.as_str()) {
        bail!(
            "unknown provider '{provider}' (expected one of: {})",
            PROVIDERS.join(", ")
        );
    }

    // ---- provider-specific connection details ----
    let api_key_env = match default_key_env(&provider) {
        Some(default_var) => {
            let var = match &args.api_key_env {
                Some(v) => v.clone(),
                None => prompter
                    .ask("API key env var", Some(default_var))?
                    .unwrap_or_else(|| default_var.to_string()),
            };
            Some(var)
        }
        None => None,
    };
    let region = if provider == "bedrock" {
        Some(match &args.region {
            Some(r) => r.clone(),
            None => prompter.require("AWS region (e.g. us-east-1)", "--region")?,
        })
    } else {
        None
    };
    let base_url = if provider == "ollama" {
        Some(match &args.base_url {
            Some(u) => u.clone(),
            None => prompter
                .ask("Ollama base URL", Some(DEFAULT_OLLAMA_URL))?
                .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string()),
        })
    } else {
        None
    };

    if let Some(var) = &api_key_env
        && !key_is_set(var)
    {
        eprintln!(
            "warning: {var} is not set in this shell — the model list cannot \
             be verified, and the variable must be set wherever this config runs"
        );
    }

    // ---- verify key + fetch models ----
    let live_models: Option<Vec<String>> = if args.offline {
        None
    } else {
        let key_value = api_key_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok());
        match lister.list(&provider, key_value.as_deref(), base_url.as_deref()) {
            Ok(ModelList::Verified(models)) => {
                println!(
                    "Verified: {provider} answered with {} model(s).",
                    models.len()
                );
                Some(models)
            }
            Ok(ModelList::Unsupported) => {
                println!(
                    "note: {provider} has no quick model-list endpoint — the model \
                     is written unverified"
                );
                None
            }
            Err(reason) => {
                eprintln!(
                    "warning: could not verify against {provider}: {reason} — \
                     continuing without verification"
                );
                None
            }
        }
    };

    // ---- model (display list, suggest a default) ----
    let model = match &args.model {
        Some(m) => {
            if let Some(models) = &live_models
                && !models.iter().any(|x| x == m)
            {
                eprintln!(
                    "warning: '{m}' is not in {provider}'s model list — \
                     continuing anyway"
                );
            }
            m.clone()
        }
        None => {
            let (display, suggested) = match &live_models {
                Some(models) => {
                    let chat = filter_chat_models(models);
                    let display = if chat.is_empty() {
                        models.clone()
                    } else {
                        chat
                    };
                    let suggested =
                        suggest_model(&provider, &display).or_else(|| display.first().cloned());
                    (display, suggested)
                }
                None => (Vec::new(), None),
            };
            if prompter.interactive && !display.is_empty() {
                println!("Models available to this key:");
                const MAX_SHOWN: usize = 20;
                for m in display.iter().take(MAX_SHOWN) {
                    let marker = if Some(m) == suggested.as_ref() {
                        "  (suggested)"
                    } else {
                        ""
                    };
                    println!("  - {m}{marker}");
                }
                if display.len() > MAX_SHOWN {
                    println!(
                        "  … and {} more (type any of them)",
                        display.len() - MAX_SHOWN
                    );
                }
            }
            let answer = prompter.ask("Model", suggested.as_deref())?;
            let model = match answer {
                Some(m) => m,
                None => bail!("--model is required in non-interactive mode"),
            };
            if !display.is_empty() && !display.iter().any(|x| x == &model) {
                eprintln!(
                    "warning: '{model}' is not in {provider}'s model list — \
                     continuing anyway"
                );
            }
            model
        }
    };

    Ok(ConfigSpec {
        provider,
        model,
        api_key_env,
        region,
        base_url,
        name: args.name.clone(),
    })
}

pub fn run_init(args: &InitArgs) -> Result<()> {
    let interactive = !args.non_interactive && std::io::stdin().is_terminal();
    let mut prompter = Prompter {
        interactive,
        stdin: std::io::stdin().lock(),
    };
    let key_is_set = |var: &str| std::env::var(var).is_ok_and(|v| !v.trim().is_empty());
    let spec = resolve_spec(args, &mut prompter, &HttpModelLister, &key_is_set)?;
    let rendered = render_config(&spec);

    // The generated config must at minimum be valid TOML; with the standalone
    // feature, also run it through the real config parser when the referenced
    // key resolves locally (env resolution needs the variable present).
    toml::from_str::<toml::Value>(&rendered).context("generated config is not valid TOML (bug)")?;
    #[cfg(feature = "standalone-cli")]
    if spec.api_key_env.as_deref().is_none_or(&key_is_set) {
        let config = aura_config::load_config_from_str(&rendered)
            .map_err(|e| anyhow::anyhow!("generated config failed validation (bug): {e}"))?;
        anyhow::ensure!(
            config.bootstrap.is_some_and(|b| b.enabled),
            "generated config does not enable [bootstrap] (bug)"
        );
    }

    if args.output.exists() && !args.force {
        bail!(
            "{} already exists — pass --force to overwrite",
            args.output.display()
        );
    }
    std::fs::write(&args.output, &rendered)
        .with_context(|| format!("failed to write {}", args.output.display()))?;

    println!("Wrote {}", args.output.display());
    if let Some(var) = &spec.api_key_env
        && !key_is_set(var)
    {
        println!(
            "note: {var} is not set in this shell. It must be set on the \
             instance that runs this config — the file only references it."
        );
    }
    println!(
        "\nNext steps:\n\
           1. (optional) export AURA_BOOTSTRAP_TOKEN=<token> — otherwise a \
         token is generated and printed in the server's startup logs\n\
           2. CONFIG_PATH={} aura-web-server\n\
           3. aura-cli --api-url http://localhost:8080 --model aura-bootstrap \
         --api-key <token>\n\
         The aura-bootstrap agent builds out the configuration conversationally \
         and applies changes without a restart.",
        args.output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> InitArgs {
        InitArgs {
            output: PathBuf::from("config.toml"),
            provider: Some("openai".to_string()),
            model: Some("gpt-5.1".to_string()),
            api_key_env: None,
            region: None,
            base_url: None,
            name: "assistant".to_string(),
            offline: true,
            non_interactive: true,
            force: false,
        }
    }

    fn non_interactive() -> Prompter<std::io::Empty> {
        Prompter {
            interactive: false,
            stdin: std::io::empty(),
        }
    }

    fn scripted(input: &'static str) -> Prompter<&'static [u8]> {
        Prompter {
            interactive: true,
            stdin: input.as_bytes(),
        }
    }

    /// Lister that always fails (network down / bad key).
    struct FailingLister;
    impl ModelLister for FailingLister {
        fn list(&self, _: &str, _: Option<&str>, _: Option<&str>) -> Result<ModelList, String> {
            Err("connection refused".to_string())
        }
    }

    /// Lister returning a fixed list.
    struct FixedLister(Vec<&'static str>);
    impl ModelLister for FixedLister {
        fn list(&self, _: &str, _: Option<&str>, _: Option<&str>) -> Result<ModelList, String> {
            Ok(ModelList::Verified(
                self.0.iter().map(|s| s.to_string()).collect(),
            ))
        }
    }

    fn no_keys(_: &str) -> bool {
        false
    }

    fn resolve(a: &InitArgs) -> Result<ConfigSpec> {
        resolve_spec(a, &mut non_interactive(), &FailingLister, &no_keys)
    }

    // ------------------------------------------------------------------
    // sensing + ordering
    // ------------------------------------------------------------------

    #[test]
    fn sensing_orders_found_providers_first() {
        let only_gemini = |var: &str| var == "GEMINI_API_KEY";
        let sensed = sensed_providers(&only_gemini);
        assert_eq!(sensed, vec!["gemini"]);
        assert_eq!(
            provider_display_order(&sensed),
            vec![
                "gemini",
                "openai",
                "anthropic",
                "bedrock",
                "ollama",
                "openrouter"
            ]
        );
    }

    #[test]
    fn sensing_none_keeps_default_order() {
        let sensed = sensed_providers(&no_keys);
        assert!(sensed.is_empty());
        assert_eq!(provider_display_order(&sensed), PROVIDERS);
    }

    #[test]
    fn sensing_multiple_keys_preserves_provider_order() {
        let two = |var: &str| var == "OPENROUTER_API_KEY" || var == "ANTHROPIC_API_KEY";
        assert_eq!(sensed_providers(&two), vec!["anthropic", "openrouter"]);
    }

    // ------------------------------------------------------------------
    // model suggestion
    // ------------------------------------------------------------------

    #[test]
    fn suggest_prefers_exact_then_shortest_prefix() {
        let models = vec![
            "gpt-4o".to_string(),
            "gpt-5.1-mini".to_string(),
            "gpt-5.1".to_string(),
        ];
        assert_eq!(
            suggest_model("openai", &models),
            Some("gpt-5.1".to_string())
        );

        // No exact gpt-5.1: shortest prefix match wins over a longer variant.
        let models = vec![
            "gpt-5.1-mini-2026-01".to_string(),
            "gpt-5.1-mini".to_string(),
        ];
        assert_eq!(
            suggest_model("openai", &models),
            Some("gpt-5.1-mini".to_string())
        );
    }

    #[test]
    fn suggest_falls_through_patterns_and_can_miss() {
        let models = vec!["gpt-4o".to_string()];
        assert_eq!(suggest_model("openai", &models), Some("gpt-4o".to_string()));
        assert_eq!(suggest_model("openai", &["weird".to_string()]), None);
    }

    #[test]
    fn chat_filter_drops_non_chat_entries() {
        let models = vec![
            "gpt-5.1".to_string(),
            "text-embedding-3-small".to_string(),
            "whisper-1".to_string(),
            "gpt-4o-audio-preview".to_string(),
        ];
        assert_eq!(filter_chat_models(&models), vec!["gpt-5.1".to_string()]);
    }

    // ------------------------------------------------------------------
    // resolution + rendering
    // ------------------------------------------------------------------

    #[test]
    fn deterministic_output() {
        let spec1 = resolve(&args()).unwrap();
        let spec2 = resolve(&args()).unwrap();
        assert_eq!(render_config(&spec1), render_config(&spec2));
    }

    #[test]
    fn openai_config_shape() {
        let rendered = render_config(&resolve(&args()).unwrap());
        assert!(rendered.contains("provider = \"openai\""));
        assert!(rendered.contains("api_key = \"{{ env.OPENAI_API_KEY }}\""));
        assert!(rendered.contains("model = \"gpt-5.1\""));
        assert!(rendered.contains("[bootstrap]\nenabled = true"));
        assert!(rendered.contains("name = \"assistant\""));
        // Parses as plain TOML.
        toml::from_str::<toml::Value>(&rendered).unwrap();
    }

    #[test]
    fn bedrock_gets_region_and_no_key() {
        let mut a = args();
        a.provider = Some("bedrock".to_string());
        a.region = Some("us-east-1".to_string());
        let rendered = render_config(&resolve(&a).unwrap());
        assert!(rendered.contains("region = \"us-east-1\""));
        assert!(!rendered.contains("api_key"));
    }

    #[test]
    fn ollama_gets_base_url_and_no_key() {
        let mut a = args();
        a.provider = Some("ollama".to_string());
        let rendered = render_config(&resolve(&a).unwrap());
        assert!(rendered.contains(&format!("base_url = \"{DEFAULT_OLLAMA_URL}\"")));
        assert!(!rendered.contains("api_key"));
    }

    #[test]
    fn non_interactive_missing_model_errors() {
        let mut a = args();
        a.model = None;
        let err = resolve(&a).unwrap_err().to_string();
        assert!(err.contains("--model"), "got: {err}");
    }

    #[test]
    fn non_interactive_missing_provider_errors() {
        let mut a = args();
        a.provider = None;
        let err = resolve(&a).unwrap_err().to_string();
        assert!(err.contains("--provider"), "got: {err}");
    }

    #[test]
    fn unknown_provider_rejected() {
        let mut a = args();
        a.provider = Some("closedai".to_string());
        let err = resolve(&a).unwrap_err().to_string();
        assert!(err.contains("unknown provider"), "got: {err}");
    }

    #[test]
    fn lister_failure_warns_and_continues() {
        // offline = false with a failing lister must still resolve.
        let mut a = args();
        a.offline = false;
        let spec = resolve_spec(&a, &mut non_interactive(), &FailingLister, &no_keys).unwrap();
        assert_eq!(spec.model, "gpt-5.1");
    }

    #[test]
    fn interactive_model_defaults_to_suggestion() {
        // Empty answers accept the defaults: provider list default (single
        // sensed key) and the suggested model from the live list.
        let mut a = args();
        a.provider = None;
        a.model = None;
        a.offline = false;
        let only_openai = |var: &str| var == "OPENAI_API_KEY";
        let lister = FixedLister(vec!["gpt-4o", "gpt-5.1", "text-embedding-3-small"]);
        let spec = resolve_spec(&a, &mut scripted("\n\n\n"), &lister, &only_openai).unwrap();
        assert_eq!(spec.provider, "openai");
        assert_eq!(spec.model, "gpt-5.1");
        assert_eq!(spec.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn interactive_provider_by_number_uses_display_order() {
        // With a gemini key sensed, "1" selects gemini (found-first order).
        let mut a = args();
        a.provider = None;
        a.api_key_env = Some("GEMINI_API_KEY".to_string());
        let gemini_only = |var: &str| var == "GEMINI_API_KEY";
        let spec = resolve_spec(&a, &mut scripted("1\n"), &FailingLister, &gemini_only).unwrap();
        assert_eq!(spec.provider, "gemini");
    }

    #[test]
    fn explicit_model_not_in_list_is_kept_with_warning() {
        let mut a = args();
        a.offline = false;
        a.model = Some("my-finetune".to_string());
        let lister = FixedLister(vec!["gpt-5.1"]);
        let spec = resolve_spec(&a, &mut non_interactive(), &lister, &no_keys).unwrap();
        assert_eq!(spec.model, "my-finetune");
    }
}
