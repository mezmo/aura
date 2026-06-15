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
//! 5. Write a minimal **complete** config: a placeholder assistant on an
//!    `[agent.llm]` whose provider/model/key are referenced from a sibling
//!    `.env` via `{{ env.LLM_* }}` (so no secret lands in the toml), plus
//!    `[bootstrap] enabled = true` so the aura-bootstrap agent can build out
//!    the real configuration conversationally. The actual values are written
//!    to `.env` (merged non-destructively if one already exists).
//!
//! Verification is best-effort: network or key failures warn and continue
//! (`--offline` skips the attempt entirely); init never hard-blocks on the
//! network. Output is deterministic given the same choices.

use std::io::{BufRead, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
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

    /// Environment variable whose value seeds the API key (its value is
    /// written to `.env` as `LLM_API_KEY`). Defaults to the provider's
    /// conventional var (e.g. OPENAI_API_KEY); not used for bedrock/ollama.
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

    /// Ask the operator to pick one of `n` numbered choices (1-based on
    /// screen), returning the 0-based index. Only a number in `1..=n` is
    /// accepted — anything else re-prompts. An empty line accepts `default`
    /// when one is given (otherwise it too re-prompts). Returns `None` in
    /// non-interactive mode (the caller decides whether that's fatal) and on
    /// EOF, so scripted/exhausted input terminates instead of looping.
    fn ask_choice(
        &mut self,
        question: &str,
        n: usize,
        default: Option<usize>,
    ) -> Result<Option<usize>> {
        if !self.interactive {
            return Ok(default);
        }
        loop {
            match default {
                Some(d) => print!("{question} [{}]: ", d + 1),
                None => print!("{question}: "),
            }
            std::io::stdout().flush()?;
            let mut line = String::new();
            if self.stdin.read_line(&mut line)? == 0 {
                return Ok(default); // EOF — don't spin forever
            }
            let answer = line.trim();
            if answer.is_empty() {
                if default.is_some() {
                    return Ok(default);
                }
            } else if let Ok(i) = answer.parse::<usize>()
                && (1..=n).contains(&i)
            {
                return Ok(Some(i - 1));
            }
            eprintln!("Please enter a number between 1 and {n}.");
        }
    }

    /// Prompt for the API key *value* to persist. When `detected_value` is
    /// present (the conventional env var is set in this shell) it is shown as
    /// `[detected_var]` and used on an empty line; typed text is taken
    /// literally. Returns `detected_value` in non-interactive mode.
    fn ask_secret(
        &mut self,
        detected_var: Option<&str>,
        detected_value: Option<&str>,
    ) -> Result<Option<String>> {
        if !self.interactive {
            return Ok(detected_value.map(String::from));
        }
        match detected_var {
            Some(v) if detected_value.is_some() => print!("API key [{v}]: "),
            _ => print!("API key: "),
        }
        std::io::stdout().flush()?;
        let mut line = String::new();
        self.stdin.read_line(&mut line)?;
        let answer = line.trim();
        if answer.is_empty() {
            Ok(detected_value.map(String::from))
        } else {
            Ok(Some(answer.to_string()))
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
    /// Env var the key is read from (drives verification + warnings). The
    /// generated toml references `LLM_API_KEY`, not this name.
    api_key_env: Option<String>,
    /// Actual API key value to write into `.env` as `LLM_API_KEY`.
    api_key_value: Option<String>,
    region: Option<String>,
    base_url: Option<String>,
    name: String,
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The `.env` keys init owns, in canonical write order. Shared by
/// `render_env` (fresh file) and `merge_env` (existing file) so both agree on
/// exactly which lines belong to init.
const MANAGED_ENV_KEYS: &[&str] = &[
    "LLM_PROVIDER",
    "LLM_MODEL",
    "LLM_API_KEY",
    "LLM_REGION",
    "LLM_BASE_URL",
];

/// The managed `.env` key/value pairs for this spec, in `MANAGED_ENV_KEYS`
/// order. Keys not applicable to the provider are omitted (e.g. no
/// `LLM_API_KEY` for bedrock/ollama, no `LLM_REGION` outside bedrock).
fn managed_env_pairs(spec: &ConfigSpec) -> Vec<(&'static str, String)> {
    let mut pairs = vec![
        ("LLM_PROVIDER", spec.provider.clone()),
        ("LLM_MODEL", spec.model.clone()),
    ];
    if spec.api_key_env.is_some() {
        pairs.push((
            "LLM_API_KEY",
            spec.api_key_value.clone().unwrap_or_default(),
        ));
    }
    if let Some(region) = &spec.region {
        pairs.push(("LLM_REGION", region.clone()));
    }
    if let Some(base_url) = &spec.base_url {
        pairs.push(("LLM_BASE_URL", base_url.clone()));
    }
    pairs
}

/// Render the `[agent.llm]` block — every value referenced from `.env` via
/// `{{ env.LLM_* }}` so no provider settings (and no secret) live in the toml.
fn render_config(spec: &ConfigSpec) -> String {
    let mut llm = String::from("provider = \"{{ env.LLM_PROVIDER }}\"\n");
    if spec.api_key_env.is_some() {
        llm.push_str("api_key = \"{{ env.LLM_API_KEY }}\"\n");
    }
    llm.push_str("model = \"{{ env.LLM_MODEL }}\"\n");
    if spec.region.is_some() {
        llm.push_str("region = \"{{ env.LLM_REGION }}\"\n");
    }
    if spec.base_url.is_some() {
        llm.push_str("base_url = \"{{ env.LLM_BASE_URL }}\"\n");
    }

    format!(
        "# Generated by `aura-cli init`.\n\
         # This is a minimal starting point: a placeholder assistant plus the\n\
         # aura-bootstrap agent, which builds out the real configuration\n\
         # conversationally and applies changes without a restart.\n\
         #\n\
         # Provider, model, and API key are read from the generated .env file.\n\
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

/// Render a fresh `.env` holding the actual provider/model/key values that
/// `config.toml` references. Used when no `.env` exists yet; see `merge_env`
/// for the update-in-place path.
fn render_env(spec: &ConfigSpec) -> String {
    let mut out = String::from(
        "# Generated by `aura-cli init`. Values referenced by config.toml via\n\
         # {{ env.LLM_* }}. Gitignored — do not commit.\n",
    );
    for (key, value) in managed_env_pairs(spec) {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

/// Returns true if `line` assigns one of the managed `LLM_*` keys
/// (`KEY=` / `KEY =`), so `merge_env` can replace just those lines.
fn is_managed_env_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    MANAGED_ENV_KEYS.iter().any(|k| {
        trimmed
            .strip_prefix(k)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// Upsert the managed `LLM_*` keys into an existing `.env`: drop every managed
/// line (wherever it sits), keep all other lines/comments/blanks in order, then
/// append the current spec's managed pairs in canonical order. Idempotent.
fn merge_env(existing: &str, spec: &ConfigSpec) -> String {
    let mut out = String::new();
    for line in existing.lines() {
        if !is_managed_env_line(line) {
            out.push_str(line);
            out.push('\n');
        }
    }
    for (key, value) in managed_env_pairs(spec) {
        out.push_str(&format!("{key}={value}\n"));
    }
    out
}

/// Resolve all inputs. Network access only through `lister`; environment
/// reads only through `key_is_set`.
fn resolve_spec<R: BufRead>(
    args: &InitArgs,
    prompter: &mut Prompter<R>,
    lister: &dyn ModelLister,
    key_is_set: &dyn Fn(&str) -> bool,
    key_value: &dyn Fn(&str) -> Option<String>,
) -> Result<ConfigSpec> {
    // ---- provider (sense keys → suggest) ----
    let sensed = sensed_providers(key_is_set);
    let provider = match &args.provider {
        Some(p) => p.trim().to_lowercase(),
        None => {
            let order = provider_display_order(&sensed);
            // Default to the sole sensed provider (if exactly one), located by
            // its position in the displayed order.
            let default_index = if sensed.len() == 1 {
                order.iter().position(|p| *p == sensed[0])
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
            match prompter.ask_choice("Provider (number)", order.len(), default_index)? {
                Some(i) => order[i].to_string(),
                None => bail!("--provider is required in non-interactive mode"),
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
    // The env var the key is read from: `--api-key-env` override, else the
    // provider's conventional var. Not prompted for — the *value* is collected
    // after model selection (and written to `.env` as `LLM_API_KEY`).
    let api_key_env = default_key_env(&provider).map(|default_var| {
        args.api_key_env
            .clone()
            .unwrap_or_else(|| default_var.to_string())
    });
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

    // ---- api key value (asked BEFORE verification, so the key the operator
    // provides — not just the detected env var — is the one used to verify and
    // list models; written to `.env` as LLM_API_KEY). The detected env var's
    // value is the default: an empty answer accepts it, a typed answer
    // overrides it. Skipped for keyless providers (bedrock, ollama).
    let api_key_value = if api_key_env.is_some() {
        let detected = api_key_env.as_deref().and_then(key_value);
        prompter.ask_secret(api_key_env.as_deref(), detected.as_deref())?
    } else {
        None
    };

    // ---- verify key + fetch models ----
    let live_models: Option<Vec<String>> = if args.offline {
        None
    } else {
        match lister.list(&provider, api_key_value.as_deref(), base_url.as_deref()) {
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
        api_key_value,
        region,
        base_url,
        name: args.name.clone(),
    })
}

/// Validate the generated config through the real parser. The toml references
/// `{{ env.LLM_* }}`, which aren't in this process's environment during init,
/// so substitute the spec's literal values first — this avoids mutating the
/// process env and avoids `resolve_env_vars`' missing-variable error — then
/// parse, validate, and confirm bootstrap is enabled.
#[cfg(feature = "standalone-cli")]
fn validate_rendered(spec: &ConfigSpec, rendered: &str) -> Result<()> {
    let mut literal = rendered.to_string();
    for (key, value) in managed_env_pairs(spec) {
        literal = literal.replace(&format!("{{{{ env.{key} }}}}"), &value);
    }
    let config = aura_config::load_config_from_str(&literal)
        .map_err(|e| anyhow::anyhow!("generated config failed validation (bug): {e}"))?;
    anyhow::ensure!(
        config.bootstrap.is_some_and(|b| b.enabled),
        "generated config does not enable [bootstrap] (bug)"
    );
    Ok(())
}

/// Human-readable next-steps shown after writing the files. The run command
/// `cd`s into the config's directory when it has one, so the sibling `.env` is
/// on dotenv's (cwd-based) search path.
fn next_steps(config_path: &Path, env_path: &Path) -> String {
    let dir = config_path.parent().filter(|p| !p.as_os_str().is_empty());
    let (run_prefix, config_for_run) = match dir {
        Some(d) => {
            let name = config_path.file_name().map_or_else(
                || config_path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            (format!("cd {} && ", d.display()), name)
        }
        None => (String::new(), config_path.display().to_string()),
    };
    format!(
        "\nNext steps:\n  \
           1. (optional) export AURA_BOOTSTRAP_TOKEN=<token> — otherwise a token \
         is generated and printed in the server's startup logs\n  \
           2. {run_prefix}CONFIG_PATH={config_for_run} aura-web-server\n  \
           3. aura-cli --api-url http://localhost:8080 --model aura-bootstrap \
         --api-key <token>\n\n\
         Provider, model, and API key were written to {env} (gitignored — do not \
         commit it). The server reads it automatically; a shell export of the same \
         variable takes precedence. The aura-bootstrap agent then builds out the \
         configuration conversationally and applies changes without a restart.",
        env = env_path.display(),
    )
}

pub fn run_init(args: &InitArgs) -> Result<()> {
    let interactive = !args.non_interactive && std::io::stdin().is_terminal();
    let mut prompter = Prompter {
        interactive,
        stdin: std::io::stdin().lock(),
    };
    let key_is_set = |var: &str| std::env::var(var).is_ok_and(|v| !v.trim().is_empty());
    let key_value = |var: &str| std::env::var(var).ok().filter(|v| !v.trim().is_empty());
    let spec = resolve_spec(
        args,
        &mut prompter,
        &HttpModelLister,
        &key_is_set,
        &key_value,
    )?;
    let rendered = render_config(&spec);

    // The generated config must at minimum be valid TOML; with the standalone
    // feature, also run it through the real config parser.
    toml::from_str::<toml::Value>(&rendered).context("generated config is not valid TOML (bug)")?;
    #[cfg(feature = "standalone-cli")]
    validate_rendered(&spec, &rendered)?;

    if args.output.exists() && !args.force {
        bail!(
            "{} already exists — pass --force to overwrite",
            args.output.display()
        );
    }

    // Write the .env first (non-destructive merge): if config.toml then fails,
    // the secret-bearing file is left consistent and the toml is regenerable.
    let env_path = args
        .output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from(".env"), |dir| dir.join(".env"));
    let env_contents = if env_path.exists() {
        let existing = std::fs::read_to_string(&env_path)
            .with_context(|| format!("failed to read {}", env_path.display()))?;
        merge_env(&existing, &spec)
    } else {
        render_env(&spec)
    };
    std::fs::write(&env_path, &env_contents)
        .with_context(|| format!("failed to write {}", env_path.display()))?;

    std::fs::write(&args.output, &rendered)
        .with_context(|| format!("failed to write {}", args.output.display()))?;

    println!("Wrote {} and {}", args.output.display(), env_path.display());
    if spec.api_key_env.is_some() && spec.api_key_value.as_deref().unwrap_or_default().is_empty() {
        println!(
            "note: no API key was captured — set LLM_API_KEY in {} before starting.",
            env_path.display()
        );
    }
    println!("{}", next_steps(&args.output, &env_path));
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

    /// Lister that records the API key it was asked to verify with, so tests
    /// can assert which key actually drove verification.
    struct RecordingLister {
        seen_key: std::cell::RefCell<Option<String>>,
        models: Vec<&'static str>,
    }
    impl ModelLister for RecordingLister {
        fn list(
            &self,
            _: &str,
            api_key: Option<&str>,
            _: Option<&str>,
        ) -> Result<ModelList, String> {
            *self.seen_key.borrow_mut() = api_key.map(String::from);
            Ok(ModelList::Verified(
                self.models.iter().map(|s| s.to_string()).collect(),
            ))
        }
    }

    fn no_keys(_: &str) -> bool {
        false
    }

    /// Key-value reader that reports nothing set (companion to `no_keys`).
    fn no_values(_: &str) -> Option<String> {
        None
    }

    fn resolve(a: &InitArgs) -> Result<ConfigSpec> {
        resolve_spec(
            a,
            &mut non_interactive(),
            &FailingLister,
            &no_keys,
            &no_values,
        )
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
        // Provider/model/key are referenced from .env, not written literally.
        let rendered = render_config(&resolve(&args()).unwrap());
        assert!(rendered.contains("provider = \"{{ env.LLM_PROVIDER }}\""));
        assert!(rendered.contains("api_key = \"{{ env.LLM_API_KEY }}\""));
        assert!(rendered.contains("model = \"{{ env.LLM_MODEL }}\""));
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
        let spec = resolve(&a).unwrap();
        let rendered = render_config(&spec);
        assert!(rendered.contains("region = \"{{ env.LLM_REGION }}\""));
        assert!(!rendered.contains("api_key"));
        // The literal region lands in .env.
        assert!(render_env(&spec).contains("LLM_REGION=us-east-1"));
    }

    #[test]
    fn ollama_gets_base_url_and_no_key() {
        let mut a = args();
        a.provider = Some("ollama".to_string());
        let spec = resolve(&a).unwrap();
        let rendered = render_config(&spec);
        assert!(rendered.contains("base_url = \"{{ env.LLM_BASE_URL }}\""));
        assert!(!rendered.contains("api_key"));
        assert!(render_env(&spec).contains(&format!("LLM_BASE_URL={DEFAULT_OLLAMA_URL}")));
    }

    #[test]
    fn render_env_openai() {
        let mut spec = resolve(&args()).unwrap();
        spec.api_key_value = Some("sk-xyz".to_string());
        let env = render_env(&spec);
        assert!(env.contains("LLM_PROVIDER=openai"));
        assert!(env.contains("LLM_MODEL=gpt-5.1"));
        assert!(env.contains("LLM_API_KEY=sk-xyz"));
        assert!(!env.contains("LLM_REGION"));
        assert!(!env.contains("LLM_BASE_URL"));
    }

    #[test]
    fn render_env_deterministic() {
        let spec1 = resolve(&args()).unwrap();
        let spec2 = resolve(&args()).unwrap();
        assert_eq!(render_env(&spec1), render_env(&spec2));
    }

    fn openai_spec() -> ConfigSpec {
        let mut s = resolve(&args()).unwrap();
        s.api_key_value = Some("sk-new".to_string());
        s
    }

    #[test]
    fn merge_env_into_empty() {
        let env = merge_env("", &openai_spec());
        assert!(env.contains("LLM_PROVIDER=openai"));
        assert!(env.contains("LLM_MODEL=gpt-5.1"));
        assert!(env.contains("LLM_API_KEY=sk-new"));
    }

    #[test]
    fn merge_env_preserves_unrelated() {
        let existing = "GITHUB_PERSONAL_ACCESS_TOKEN=ghp_abc\nLLM_API_KEY=old\n";
        let env = merge_env(existing, &openai_spec());
        assert!(env.contains("GITHUB_PERSONAL_ACCESS_TOKEN=ghp_abc"));
        assert!(env.contains("LLM_API_KEY=sk-new"));
        assert!(!env.contains("LLM_API_KEY=old"));
    }

    #[test]
    fn merge_env_switches_provider() {
        // openai -> ollama: LLM_API_KEY drops, LLM_BASE_URL appears, the
        // unrelated token survives.
        let existing =
            "LLM_PROVIDER=openai\nLLM_MODEL=gpt-5.1\nLLM_API_KEY=sk-old\nGITHUB_TOKEN=abc\n";
        let mut a = args();
        a.provider = Some("ollama".to_string());
        let spec = resolve(&a).unwrap();
        let env = merge_env(existing, &spec);
        assert!(env.contains("GITHUB_TOKEN=abc"));
        assert!(env.contains("LLM_PROVIDER=ollama"));
        assert!(env.contains("LLM_BASE_URL="));
        assert!(!env.contains("LLM_API_KEY"));
    }

    #[test]
    fn merge_env_idempotent() {
        let spec = openai_spec();
        let once = merge_env("", &spec);
        assert_eq!(merge_env(&once, &spec), once);
    }

    #[test]
    fn next_steps_mentions_env() {
        let s = next_steps(Path::new("config.toml"), Path::new(".env"));
        assert!(s.contains(".env"), "got: {s}");
        assert!(s.contains("config.toml"), "got: {s}");
        assert!(s.contains("aura-bootstrap"), "got: {s}");
    }

    #[test]
    fn next_steps_cds_into_config_dir() {
        // When the config lives in a subdir, the run command cd's there so the
        // sibling .env is on dotenv's search path.
        let s = next_steps(Path::new("proj/config.toml"), Path::new("proj/.env"));
        assert!(s.contains("cd proj"), "got: {s}");
        assert!(s.contains("CONFIG_PATH=config.toml"), "got: {s}");
    }

    #[cfg(feature = "standalone-cli")]
    #[test]
    fn validate_rendered_accepts_generated() {
        let spec = openai_spec();
        let rendered = render_config(&spec);
        validate_rendered(&spec, &rendered).unwrap();
    }

    #[cfg(feature = "standalone-cli")]
    #[test]
    fn validate_rendered_requires_bootstrap() {
        let spec = openai_spec();
        let rendered = render_config(&spec).replace("[bootstrap]\nenabled = true\n", "");
        let err = validate_rendered(&spec, &rendered).unwrap_err().to_string();
        assert!(err.contains("bootstrap"), "got: {err}");
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
        let spec = resolve_spec(
            &a,
            &mut non_interactive(),
            &FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
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
        let detected = |var: &str| (var == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let lister = FixedLister(vec!["gpt-4o", "gpt-5.1", "text-embedding-3-small"]);
        // Prompts in order: provider, model, API key — three empty lines.
        let spec = resolve_spec(
            &a,
            &mut scripted("\n\n\n"),
            &lister,
            &only_openai,
            &detected,
        )
        .unwrap();
        assert_eq!(spec.provider, "openai");
        assert_eq!(spec.model, "gpt-5.1");
        assert_eq!(spec.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(spec.api_key_value.as_deref(), Some("sk-detected"));
    }

    #[test]
    fn interactive_provider_by_number_uses_display_order() {
        // With a gemini key sensed, "1" selects gemini (found-first order).
        let mut a = args();
        a.provider = None;
        a.api_key_env = Some("GEMINI_API_KEY".to_string());
        let gemini_only = |var: &str| var == "GEMINI_API_KEY";
        let spec = resolve_spec(
            &a,
            &mut scripted("1\n"),
            &FailingLister,
            &gemini_only,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.provider, "gemini");
    }

    // ------------------------------------------------------------------
    // provider menu (numeric-only, re-prompts on bad input)
    // ------------------------------------------------------------------

    #[test]
    fn ask_choice_rejects_until_valid_number() {
        // out-of-range high, zero, non-numeric, then a valid choice.
        let mut p = scripted("9\n0\nfoo\n2\n");
        assert_eq!(p.ask_choice("Provider", 6, None).unwrap(), Some(1));
    }

    #[test]
    fn ask_choice_empty_uses_default() {
        let mut p = scripted("\n");
        assert_eq!(p.ask_choice("Provider", 6, Some(3)).unwrap(), Some(3));
    }

    #[test]
    fn ask_choice_non_interactive_returns_default() {
        assert_eq!(
            non_interactive()
                .ask_choice("Provider", 6, Some(2))
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            non_interactive().ask_choice("Provider", 6, None).unwrap(),
            None
        );
    }

    #[test]
    fn provider_reprompt_then_selects() {
        // Free-text and out-of-range entries are rejected; only a number in
        // range is accepted. "2" -> anthropic (default PROVIDERS order).
        let mut a = args();
        a.provider = None;
        a.offline = true;
        let spec = resolve_spec(
            &a,
            &mut scripted("9\n0\nfoo\n2\n"),
            &FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.provider, "anthropic");
    }

    #[test]
    fn provider_empty_uses_single_sensed_default() {
        let mut a = args();
        a.provider = None;
        a.offline = true;
        let only_openai = |v: &str| v == "OPENAI_API_KEY";
        let spec = resolve_spec(
            &a,
            &mut scripted("\n"),
            &FailingLister,
            &only_openai,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.provider, "openai");
    }

    #[test]
    fn provider_empty_no_default_reprompts() {
        // No sensed key -> no default; an empty line is rejected, not silently
        // accepted, and the prompt repeats until a valid number arrives.
        let mut a = args();
        a.provider = None;
        a.offline = true;
        let spec = resolve_spec(
            &a,
            &mut scripted("\n2\n"),
            &FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.provider, "anthropic");
    }

    // ------------------------------------------------------------------
    // api key value (asked before verification, written to .env)
    // ------------------------------------------------------------------

    #[test]
    fn typed_api_key_is_used_for_verification() {
        // A key typed at the prompt must drive verification — not the detected
        // env var — so the model list reflects the operator's chosen key.
        let mut a = args();
        a.model = Some("gpt-5.1".to_string()); // skip the model prompt
        a.offline = false; // actually verify
        let lister = RecordingLister {
            seen_key: std::cell::RefCell::new(None),
            models: vec!["gpt-5.1"],
        };
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let spec = resolve_spec(
            &a,
            &mut scripted("sk-override\n"),
            &lister,
            &no_keys,
            &detected,
        )
        .unwrap();
        assert_eq!(spec.api_key_value.as_deref(), Some("sk-override"));
        assert_eq!(lister.seen_key.borrow().as_deref(), Some("sk-override"));
    }

    #[test]
    fn empty_api_key_verifies_with_detected_value() {
        let mut a = args();
        a.model = Some("gpt-5.1".to_string());
        a.offline = false;
        let lister = RecordingLister {
            seen_key: std::cell::RefCell::new(None),
            models: vec!["gpt-5.1"],
        };
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let spec = resolve_spec(&a, &mut scripted("\n"), &lister, &no_keys, &detected).unwrap();
        assert_eq!(spec.api_key_value.as_deref(), Some("sk-detected"));
        assert_eq!(lister.seen_key.borrow().as_deref(), Some("sk-detected"));
    }

    #[test]
    fn api_key_empty_uses_detected_value() {
        // openai + offline + model from flags -> the only prompt is the key.
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let spec = resolve_spec(
            &args(),
            &mut scripted("\n"),
            &FailingLister,
            &no_keys,
            &detected,
        )
        .unwrap();
        assert_eq!(spec.api_key_value.as_deref(), Some("sk-detected"));
        assert_eq!(spec.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn api_key_typed_is_literal() {
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let spec = resolve_spec(
            &args(),
            &mut scripted("sk-typed\n"),
            &FailingLister,
            &no_keys,
            &detected,
        )
        .unwrap();
        assert_eq!(spec.api_key_value.as_deref(), Some("sk-typed"));
    }

    #[test]
    fn api_key_no_detected_var_typed() {
        // No value in the shell -> plain "API key:" prompt; typed value used.
        let spec = resolve_spec(
            &args(),
            &mut scripted("sk-manual\n"),
            &FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.api_key_value.as_deref(), Some("sk-manual"));
    }

    #[test]
    fn ollama_has_no_api_key_value() {
        let mut a = args();
        a.provider = Some("ollama".to_string());
        let spec = resolve_spec(
            &a,
            &mut non_interactive(),
            &FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.api_key_value, None);
        assert_eq!(spec.api_key_env, None);
    }

    #[test]
    fn bedrock_has_no_api_key_value() {
        let mut a = args();
        a.provider = Some("bedrock".to_string());
        a.region = Some("us-east-1".to_string());
        let spec = resolve_spec(
            &a,
            &mut non_interactive(),
            &FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.api_key_value, None);
    }

    #[test]
    fn non_interactive_api_key_from_flag() {
        // --api-key-env names the var whose *value* seeds the key.
        let mut a = args();
        a.api_key_env = Some("MY_KEY".to_string());
        let from_flag = |v: &str| (v == "MY_KEY").then(|| "sk-flag".to_string());
        let spec = resolve_spec(
            &a,
            &mut non_interactive(),
            &FailingLister,
            &no_keys,
            &from_flag,
        )
        .unwrap();
        assert_eq!(spec.api_key_value.as_deref(), Some("sk-flag"));
        assert_eq!(spec.api_key_env.as_deref(), Some("MY_KEY"));
    }

    #[test]
    fn explicit_model_not_in_list_is_kept_with_warning() {
        let mut a = args();
        a.offline = false;
        a.model = Some("my-finetune".to_string());
        let lister = FixedLister(vec!["gpt-5.1"]);
        let spec = resolve_spec(&a, &mut non_interactive(), &lister, &no_keys, &no_values).unwrap();
        assert_eq!(spec.model, "my-finetune");
    }
}
