//! `aura-cli init` — generate a starter configuration.
//!
//! The flow:
//!
//! 1. **Sense** conventional API-key env vars (OPENAI_API_KEY, …).
//! 2. **Provider**: exactly one key found → suggested as the default;
//!    several → list prioritized by the ones found; none → default order.
//! 3. **API key**: if the provider's conventional env var is set, tell the
//!    user and ask whether to use it. If not set, prompt for the key value
//!    (masked input). The generated config references the provider's native
//!    env var directly (`{{ env.OPENAI_API_KEY }}`), not an intermediate
//!    `LLM_*` name. A `.env` is only written when the user provides a new
//!    key that isn't already in the environment.
//! 4. **Verify** the key by querying the provider's live model-list
//!    endpoint (blocking HTTP, short timeout; bedrock has no cheap HTTP
//!    listing and is skipped with a note).
//! 5. **Model**: rank the fetched list into a short, best-first shortlist via
//!    a per-provider table of stable family roots (newest version per family,
//!    snapshots hidden). The user picks by number, accepts the suggested
//!    default, or types any id — never a silent pick.
//! 6. Write a minimal **complete** config referencing the provider's native
//!    env vars.
//!
//! Verification is best-effort: network or key failures warn and continue
//! (`--offline` skips the attempt entirely); init never hard-blocks on the
//! network. Output is deterministic given the same choices.

use std::io::{BufRead, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

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
    const ALL: &'static [Provider] = &[
        Provider::OpenAI,
        Provider::Anthropic,
        Provider::Bedrock,
        Provider::Gemini,
        Provider::Ollama,
        Provider::OpenRouter,
    ];

    /// Canonical lowercase id — matches the config's `provider` tag.
    fn as_str(self) -> &'static str {
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

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Pinned Anthropic API version header for the models endpoint.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Whether the provider's model-list endpoint authenticates with the API key,
/// so a successful response actually verifies it. OpenRouter's `/models` is
/// public and Ollama's `/api/tags` is local — neither checks a key.
fn list_verifies_key(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::OpenAI | Provider::Anthropic | Provider::Gemini
    )
}

/// Default API-key env var per provider (None = provider needs no key).
fn default_key_env(provider: Provider) -> Option<&'static str> {
    match provider {
        Provider::OpenAI => Some("OPENAI_API_KEY"),
        Provider::Anthropic => Some("ANTHROPIC_API_KEY"),
        Provider::Gemini => Some("GEMINI_API_KEY"),
        Provider::OpenRouter => Some("OPENROUTER_API_KEY"),
        Provider::Bedrock | Provider::Ollama => None,
    }
}

/// Ordered model *family roots* per provider, best-first and **specific →
/// general** so each model is claimed by its first matching root. Roots are
/// deliberately version-free (`gpt-5`, not `gpt-5.1`): a new `gpt-5.6` matches
/// the `gpt-5` root automatically — no release needed. A release is only
/// warranted to bless a genuinely new family (a future `gpt-6`/`o5`). The
/// concrete id shown per family is chosen by `rank_shortlist`.
fn family_roots(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::OpenAI => &["gpt-5", "gpt-4.1", "gpt-4o", "o4", "o3", "o1", "gpt-4"],
        Provider::Anthropic => &["claude-sonnet-4", "claude-opus-4", "claude-haiku-4"],
        Provider::Gemini => &["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash"],
        Provider::OpenRouter => &[
            "openai/gpt-5",
            "anthropic/claude-sonnet-4",
            "google/gemini-2.5-pro",
            "openai/gpt-4o",
        ],
        // Ollama is shown uncurated (see rank_shortlist); bedrock has no list.
        Provider::Ollama | Provider::Bedrock => &[],
    }
}

/// Substrings that mark an entry in a provider's model list as not a chat
/// model (embeddings, audio, images, instruct/search variants, …).
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
    "instruct",
    "search",
];

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Output path for the generated config
    #[arg(long, short = 'o', default_value = "config.toml")]
    pub output: PathBuf,

    /// LLM provider (openai, anthropic, bedrock, gemini, ollama, openrouter)
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,

    /// Model name (verified against the provider's model list when possible)
    #[arg(long)]
    pub model: Option<String>,

    /// Environment variable whose value is used as the API key. Defaults to
    /// the provider's conventional var (e.g. OPENAI_API_KEY); not used for
    /// bedrock/ollama.
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
        provider: Provider,
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
        provider: Provider,
        api_key: Option<&str>,
        base_url: Option<&str>,
    ) -> Result<ModelList, String> {
        let key = api_key.unwrap_or_default();
        let models = match provider {
            Provider::OpenAI => Self::extract(
                &Self::get_json(
                    Self::client()?
                        .get("https://api.openai.com/v1/models")
                        .bearer_auth(key),
                )?,
                "data",
                "id",
            ),
            Provider::Anthropic => Self::extract(
                &Self::get_json(
                    Self::client()?
                        .get("https://api.anthropic.com/v1/models")
                        .header("x-api-key", key)
                        .header("anthropic-version", ANTHROPIC_VERSION),
                )?,
                "data",
                "id",
            ),
            Provider::OpenRouter => Self::extract(
                &Self::get_json(Self::client()?.get("https://openrouter.ai/api/v1/models"))?,
                "data",
                "id",
            ),
            Provider::Gemini => Self::extract(
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
            Provider::Ollama => Self::extract(
                &Self::get_json(Self::client()?.get(format!(
                    "{}/api/tags",
                    base_url.unwrap_or(DEFAULT_OLLAMA_URL).trim_end_matches('/')
                )))?,
                "models",
                "name",
            ),
            // Bedrock needs the AWS SDK (ListFoundationModels); skipped in v1.
            Provider::Bedrock => return Ok(ModelList::Unsupported),
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

/// Providers whose conventional key env var is set, in canonical order.
/// `is_set` is injected so tests don't touch the process environment.
fn sensed_providers(is_set: &dyn Fn(&str) -> bool) -> Vec<Provider> {
    Provider::ALL
        .iter()
        .copied()
        .filter(|&p| default_key_env(p).is_some_and(is_set))
        .collect()
}

/// Provider display order: sensed providers first, then the rest.
fn provider_display_order(sensed: &[Provider]) -> Vec<Provider> {
    let mut order: Vec<Provider> = sensed.to_vec();
    order.extend(
        Provider::ALL
            .iter()
            .copied()
            .filter(|p| !sensed.contains(p)),
    );
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

/// Version key for natural ("human") ordering of model ids: the sequence of
/// digit runs parsed as integers (`gpt-5.6` → `[5, 6]`, `o3` → `[3]`). Compared
/// lexicographically, a higher key means a newer model within a family.
fn natural_key(id: &str) -> Vec<u64> {
    let mut keys = Vec::new();
    let mut run = String::new();
    for ch in id.chars() {
        if ch.is_ascii_digit() {
            run.push(ch);
        } else if !run.is_empty() {
            keys.push(run.parse().unwrap_or(0));
            run.clear();
        }
    }
    if !run.is_empty() {
        keys.push(run.parse().unwrap_or(0));
    }
    keys
}

/// True when an id carries a dated/snapshot segment: any `-`-delimited segment
/// that is all digits and at least 4 long (`2024`, `20250514`, `1106`, `0125`).
/// Plain version ids (`gpt-4o`, `gpt-5.1`, `o3`) are not flagged.
fn is_dated(id: &str) -> bool {
    id.split('-')
        .any(|seg| seg.len() >= 4 && seg.bytes().all(|b| b.is_ascii_digit()))
}

/// Build the curated, best-first shortlist shown after verification. For each
/// family root (in table order) pick one representative from the live list:
/// prefer non-dated ids, then the highest `natural_key` (newest version), tie
/// broken by the shortest id (the base). Models matching no root are omitted
/// from the shortlist but remain typeable. Falls back to the chat-filtered list
/// (newest-first) when no root matches at all.
fn rank_shortlist(provider: Provider, models: &[String]) -> Vec<String> {
    // Ollama's `/api/tags` returns whatever the user installed — non-canonical
    // names with quant suffixes and aliases — so family-root matching and the
    // chat-marker filter are both unreliable (many good local models are tagged
    // `*-instruct`). Show the installed list as-is and let the user choose.
    if provider == Provider::Ollama {
        return models.to_vec();
    }
    let chat = filter_chat_models(models);
    let mut claimed = vec![false; chat.len()];
    let mut shortlist = Vec::new();

    for root in family_roots(provider) {
        let family: Vec<usize> = chat
            .iter()
            .enumerate()
            .filter(|(i, m)| !claimed[*i] && m.starts_with(root))
            .map(|(i, _)| i)
            .collect();
        if family.is_empty() {
            continue;
        }
        for &i in &family {
            claimed[i] = true;
        }
        let non_dated: Vec<usize> = family
            .iter()
            .copied()
            .filter(|&i| !is_dated(&chat[i]))
            .collect();
        let pool = if non_dated.is_empty() {
            family
        } else {
            non_dated
        };
        if let Some(&best) = pool.iter().max_by(|&&a, &&b| {
            natural_key(&chat[a])
                .cmp(&natural_key(&chat[b]))
                .then_with(|| chat[b].len().cmp(&chat[a].len()))
        }) {
            shortlist.push(chat[best].clone());
        }
    }

    if shortlist.is_empty() {
        let mut fallback = chat;
        fallback.sort_by(|a, b| {
            natural_key(b)
                .cmp(&natural_key(a))
                .then_with(|| a.len().cmp(&b.len()))
        });
        fallback.truncate(8);
        return fallback;
    }
    shortlist
}

// ============================================================================
// Interactive prompting
// ============================================================================

/// Interactive prompt helper. All prompts go through here so the resolution
/// logic stays testable without a terminal.
struct Prompter<R: BufRead> {
    interactive: bool,
    /// True when reading from a real terminal. Secret prompts then read with
    /// echo suppressed; in tests (scripted stdin) this is false so `ask_secret`
    /// reads the injected `stdin` instead.
    is_tty: bool,
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
                return Ok(default);
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

    /// Ask a yes/no question. Returns the default in non-interactive mode.
    fn ask_yes_no(&mut self, question: &str, default: bool) -> Result<bool> {
        if !self.interactive {
            return Ok(default);
        }
        let hint = if default { "Y/n" } else { "y/N" };
        print!("{question} [{hint}]: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        self.stdin.read_line(&mut line)?;
        let answer = line.trim().to_lowercase();
        if answer.is_empty() {
            Ok(default)
        } else {
            Ok(answer.starts_with('y'))
        }
    }

    /// Prompt for an API key with masked input. On a real terminal the input
    /// is read with echo suppressed via `rpassword`. In test contexts
    /// (`is_tty = false`), falls back to `read_line` on the injected stdin.
    /// Returns `None` on empty input or EOF.
    fn ask_secret_masked(&mut self, prompt: &str) -> Result<Option<String>> {
        if !self.interactive {
            return Ok(None);
        }
        print!("{prompt}: ");
        std::io::stdout().flush()?;
        let raw = if self.is_tty {
            let secret = rpassword::read_password()?;
            println!();
            secret
        } else {
            let mut line = String::new();
            self.stdin.read_line(&mut line)?;
            line
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed.to_string()))
        }
    }

    /// Pick a model: a number selects from the displayed `shortlist`, an empty
    /// line / EOF accepts the suggested entry, and anything else is taken as a
    /// typed model id verbatim. Out-of-range numbers re-prompt. Returns the
    /// suggested entry in non-interactive mode (`None` if the shortlist is
    /// empty, which the caller turns into the `--model` requirement).
    fn ask_model(
        &mut self,
        shortlist: &[String],
        suggested_index: usize,
    ) -> Result<Option<String>> {
        let suggested = shortlist.get(suggested_index).cloned();
        if !self.interactive {
            return Ok(suggested);
        }
        loop {
            match &suggested {
                Some(d) => print!("Which model should AURA use? [{d}]: "),
                None => print!("Which model should AURA use?: "),
            }
            std::io::stdout().flush()?;
            let mut line = String::new();
            if self.stdin.read_line(&mut line)? == 0 {
                return Ok(suggested);
            }
            let answer = line.trim();
            if answer.is_empty() {
                return Ok(suggested);
            }
            if !shortlist.is_empty()
                && let Ok(n) = answer.parse::<usize>()
            {
                if (1..=shortlist.len()).contains(&n) {
                    return Ok(Some(shortlist[n - 1].clone()));
                }
                eprintln!(
                    "Please enter a number between 1 and {}, or a model id.",
                    shortlist.len()
                );
                continue;
            }
            return Ok(Some(answer.to_string()));
        }
    }
}

// ============================================================================
// Resolution + rendering
// ============================================================================

/// How the API key is sourced — determines what goes into the config and
/// whether a `.env` needs to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ApiKeySource {
    /// The key lives in an existing environment variable; the config
    /// references it directly via `{{ env.<var_name> }}`. No `.env` write.
    EnvVar(String),
    /// The user typed a key at the prompt; it's written to `.env` under the
    /// provider's conventional env var name, and the config references that.
    Provided { env_var: String, value: String },
}

/// Fully resolved inputs (after flags, sensing, verification, prompts).
#[derive(Debug)]
struct ConfigSpec {
    provider: Provider,
    model: String,
    api_key: Option<ApiKeySource>,
    region: Option<String>,
    base_url: Option<String>,
    name: String,
}

impl ConfigSpec {
    fn api_key_env_var(&self) -> Option<&str> {
        match &self.api_key {
            Some(ApiKeySource::EnvVar(var)) => Some(var),
            Some(ApiKeySource::Provided { env_var, .. }) => Some(env_var),
            None => None,
        }
    }
}

fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render the `[agent.llm]` block. Values that come from the environment are
/// referenced via `{{ env.<ACTUAL_VAR> }}` — no intermediate `LLM_*` rename.
fn render_config(spec: &ConfigSpec) -> String {
    let mut llm = format!("provider = \"{}\"\n", spec.provider.as_str());
    if let Some(var) = spec.api_key_env_var() {
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
         \n\
         [agent]\n\
         name = \"{name}\"\n\
         system_prompt = \"You are a helpful general-purpose assistant.\"\n\
         \n\
         [agent.llm]\n\
         {llm}",
        name = toml_escape(&spec.name),
    )
}

/// Render a `.env` file for a user-provided API key. Only called when the
/// user typed a key that isn't already in their environment.
fn render_env(env_var: &str, value: &str) -> String {
    format!(
        "# Generated by `aura-cli init`. Contains a secret — do not commit it \
         (add this file to your .gitignore).\n\
         {env_var}={value}\n"
    )
}

/// Merge a new key into an existing `.env`: replace the line if the key
/// already exists, otherwise append it.
fn merge_env(existing: &str, env_var: &str, value: &str) -> String {
    let mut found = false;
    let mut out = String::new();
    for line in existing.lines() {
        let trimmed = line.trim_start();
        if trimmed
            .strip_prefix(env_var)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
        {
            out.push_str(&format!("{env_var}={value}\n"));
            found = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !found {
        out.push_str(&format!("{env_var}={value}\n"));
    }
    out
}

/// Resolve all inputs. Network access only through `lister`; environment
/// reads only through `key_is_set` / `key_value`.
fn resolve_spec<R: BufRead>(
    args: &InitArgs,
    prompter: &mut Prompter<R>,
    lister: &dyn ModelLister,
    key_is_set: &dyn Fn(&str) -> bool,
    key_value: &dyn Fn(&str) -> Option<String>,
) -> Result<ConfigSpec> {
    // ---- provider ---- (clap already validated --provider into a Provider)
    let sensed = sensed_providers(key_is_set);
    let provider = match args.provider {
        Some(p) => p,
        None => {
            let order = provider_display_order(&sensed);
            let default_index = if sensed.len() == 1 {
                order.iter().position(|&p| p == sensed[0])
            } else {
                None
            };
            if prompter.interactive {
                println!("\nWhich LLM provider should AURA use?\n");
                for (i, p) in order.iter().enumerate() {
                    let marker = if sensed.contains(p) {
                        "  (key detected)"
                    } else {
                        ""
                    };
                    println!("  {}. {p}{marker}", i + 1);
                }
                println!();
            }
            match prompter.ask_choice("Provider", order.len(), default_index)? {
                Some(i) => order[i],
                None => bail!("--provider is required in non-interactive mode"),
            }
        }
    };

    // ---- provider-specific connection details ----
    let api_key_env_var = args
        .api_key_env
        .clone()
        .or_else(|| default_key_env(provider).map(String::from));

    let region = if provider == Provider::Bedrock {
        Some(match &args.region {
            Some(r) => r.clone(),
            None => prompter.require("AWS region (e.g. us-east-1)", "--region")?,
        })
    } else {
        None
    };
    let base_url = if provider == Provider::Ollama {
        Some(match &args.base_url {
            Some(u) => u.clone(),
            None => prompter
                .ask("Ollama base URL", Some(DEFAULT_OLLAMA_URL))?
                .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string()),
        })
    } else {
        None
    };

    // ---- API key ----
    let api_key: Option<ApiKeySource> = if let Some(env_var) = &api_key_env_var {
        let detected_value = (key_value)(env_var);
        if let Some(_value) = &detected_value {
            if prompter.ask_yes_no(
                &format!("Found {env_var} in your environment. Use this environment variable?"),
                true,
            )? {
                Some(ApiKeySource::EnvVar(env_var.clone()))
            } else {
                match prompter.ask_secret_masked("Enter your API key")? {
                    Some(v) => Some(ApiKeySource::Provided {
                        env_var: env_var.clone(),
                        value: v,
                    }),
                    None => bail!("an API key is required for {provider}"),
                }
            }
        } else {
            if prompter.interactive {
                println!("\nNo {env_var} found in your environment.");
            }
            match prompter.ask_secret_masked("Enter your API key")? {
                Some(v) => Some(ApiKeySource::Provided {
                    env_var: env_var.clone(),
                    value: v,
                }),
                None => {
                    if prompter.interactive {
                        eprintln!("warning: no API key provided — set {env_var} before starting");
                    }
                    Some(ApiKeySource::EnvVar(env_var.clone()))
                }
            }
        }
    } else {
        None
    };

    // ---- verify key + fetch models ----
    let detected_for_verify: Option<String> = match &api_key {
        Some(ApiKeySource::EnvVar(var)) => (key_value)(var),
        _ => None,
    };
    let verify_key: Option<&str> = match &api_key {
        Some(ApiKeySource::Provided { value, .. }) => Some(value.as_str()),
        Some(ApiKeySource::EnvVar(_)) => detected_for_verify.as_deref(),
        None => None,
    };

    let live_models: Option<Vec<String>> = if args.offline {
        None
    } else {
        match lister.list(provider, verify_key, base_url.as_deref()) {
            Ok(ModelList::Verified(models)) => {
                if list_verifies_key(provider) {
                    println!(
                        "Verified: {provider} answered with {} model(s).",
                        models.len()
                    );
                } else if default_key_env(provider).is_some() {
                    // Provider uses a key, but its list endpoint doesn't check
                    // it — be explicit that nothing was validated.
                    println!(
                        "{provider} lists {} model(s) (this did not validate \
                         your key — that happens on first use).",
                        models.len()
                    );
                } else {
                    println!("{provider} lists {} model(s).", models.len());
                }
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

    // ---- model ----
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
            let shortlist = match &live_models {
                Some(models) => rank_shortlist(provider, models),
                None => Vec::new(),
            };
            if prompter.interactive && !shortlist.is_empty() {
                // Ollama's list is the user's installed models, not a curated
                // recommendation, so don't mark a "suggested" pick for it.
                let is_ollama = provider == Provider::Ollama;
                println!("\nAvailable models (enter a number, or type any model id):\n");
                for (i, m) in shortlist.iter().enumerate() {
                    let marker = if i == 0 && !is_ollama {
                        "  (suggested)"
                    } else {
                        ""
                    };
                    println!("  {}. {m}{marker}", i + 1);
                }
                if let Some(models) = &live_models
                    && models.len() > shortlist.len()
                {
                    println!(
                        "  … and {} more — type any id",
                        models.len() - shortlist.len()
                    );
                }
                if is_ollama {
                    println!(
                        "\n  note: these are the models installed on this host. \
                         Local-model quality varies — recent instruct-tuned \
                         models work best; see the docs for ones we've tested."
                    );
                }
                println!();
            }
            let model = match prompter.ask_model(&shortlist, 0)? {
                Some(m) => m,
                None => bail!("--model is required in non-interactive mode"),
            };
            if let Some(models) = &live_models
                && !models.iter().any(|x| x == &model)
            {
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
        api_key,
        region,
        base_url,
        name: args.name.clone(),
    })
}

/// Validate the generated config through the real parser.
#[cfg(feature = "standalone-cli")]
fn validate_rendered(spec: &ConfigSpec, rendered: &str) -> Result<()> {
    let mut literal = rendered.to_string();
    // Substitute env var references with literal values for validation
    literal = literal.replace(&format!("{{{{ env.{} }}}}", ""), "");
    // Replace specific known references
    if let Some(var) = spec.api_key_env_var() {
        literal = literal.replace(&format!("{{{{ env.{var} }}}}"), "test-key");
    }
    // Provider and model are written literally, not as env refs
    aura_config::load_config_from_str(&literal)
        .map_err(|e| anyhow::anyhow!("generated config failed validation (bug): {e}"))?;
    Ok(())
}

/// Human-readable next-steps shown after writing the files.
fn next_steps(config_path: &Path, wrote_env: bool) -> String {
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
    let env_note = if wrote_env {
        "\nThe API key was written to .env (gitignored — do not commit it).\n\
         The server reads it automatically; a shell export of the same \
         variable takes precedence."
    } else {
        ""
    };
    format!(
        "\nNext steps:\n  \
           1. {run_prefix}CONFIG_PATH={config_for_run} aura-web-server\n  \
           2. aura-cli --api-url http://localhost:8080\n\
         {env_note}",
    )
}

pub fn run_init(args: &InitArgs) -> Result<()> {
    dotenvy::dotenv().ok();
    let is_tty = std::io::stdin().is_terminal();
    let interactive = !args.non_interactive && is_tty;
    let mut prompter = Prompter {
        interactive,
        is_tty,
        stdin: std::io::stdin().lock(),
    };
    if prompter.interactive {
        println!(
            "Welcome to AURA. This init process will generate a starter config \
             you can run right away. I'll ask a couple of questions, then write \
             your config."
        );
    }

    // Resolve an existing config before asking anything: prompt to overwrite
    // (interactive) or fail fast with --force guidance (non-interactive).
    if args.output.exists() && !args.force {
        if prompter.interactive {
            let overwrite = prompter.ask_yes_no(
                &format!("\n{} already exists. Overwrite?", args.output.display()),
                false,
            )?;
            if !overwrite {
                println!("Exiting — {} left unchanged.", args.output.display());
                return Ok(());
            }
        } else {
            bail!(
                "{} already exists — pass --force to overwrite",
                args.output.display()
            );
        }
    }

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

    toml::from_str::<toml::Value>(&rendered).context("generated config is not valid TOML (bug)")?;
    #[cfg(feature = "standalone-cli")]
    validate_rendered(&spec, &rendered)?;

    // Only write .env when the user provided a new key
    let mut wrote_env = false;
    if let Some(ApiKeySource::Provided { env_var, value }) = &spec.api_key {
        let env_path = args
            .output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from(".env"), |dir| dir.join(".env"));
        let env_contents = if env_path.exists() {
            let existing = std::fs::read_to_string(&env_path)
                .with_context(|| format!("failed to read {}", env_path.display()))?;
            merge_env(&existing, env_var, value)
        } else {
            render_env(env_var, value)
        };
        std::fs::write(&env_path, &env_contents)
            .with_context(|| format!("failed to write {}", env_path.display()))?;
        wrote_env = true;
        println!("Wrote {}", env_path.display());
    }

    std::fs::write(&args.output, &rendered)
        .with_context(|| format!("failed to write {}", args.output.display()))?;
    println!("Wrote {}", args.output.display());

    println!("{}", next_steps(&args.output, wrote_env));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> InitArgs {
        InitArgs {
            output: PathBuf::from("config.toml"),
            provider: Some(Provider::OpenAI),
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
            is_tty: false,
            stdin: std::io::empty(),
        }
    }

    fn scripted(input: &'static str) -> Prompter<&'static [u8]> {
        // is_tty = false so ask_secret reads the scripted stdin (no real tty).
        Prompter {
            interactive: true,
            is_tty: false,
            stdin: input.as_bytes(),
        }
    }

    struct FailingLister;
    impl ModelLister for FailingLister {
        fn list(&self, _: Provider, _: Option<&str>, _: Option<&str>) -> Result<ModelList, String> {
            Err("connection refused".to_string())
        }
    }

    struct FixedLister(Vec<&'static str>);
    impl ModelLister for FixedLister {
        fn list(&self, _: Provider, _: Option<&str>, _: Option<&str>) -> Result<ModelList, String> {
            Ok(ModelList::Verified(
                self.0.iter().map(|s| s.to_string()).collect(),
            ))
        }
    }

    struct RecordingLister {
        seen_key: std::cell::RefCell<Option<String>>,
        models: Vec<&'static str>,
    }
    impl ModelLister for RecordingLister {
        fn list(
            &self,
            _: Provider,
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
        assert_eq!(sensed, vec![Provider::Gemini]);
        assert_eq!(
            provider_display_order(&sensed),
            vec![
                Provider::Gemini,
                Provider::OpenAI,
                Provider::Anthropic,
                Provider::Bedrock,
                Provider::Ollama,
                Provider::OpenRouter,
            ]
        );
    }

    #[test]
    fn sensing_none_keeps_default_order() {
        let sensed = sensed_providers(&no_keys);
        assert!(sensed.is_empty());
        assert_eq!(provider_display_order(&sensed), Provider::ALL.to_vec());
    }

    #[test]
    fn sensing_multiple_keys_preserves_provider_order() {
        let two = |var: &str| var == "OPENROUTER_API_KEY" || var == "ANTHROPIC_API_KEY";
        assert_eq!(
            sensed_providers(&two),
            vec![Provider::Anthropic, Provider::OpenRouter]
        );
    }

    // ------------------------------------------------------------------
    // model suggestion
    // ------------------------------------------------------------------

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

    #[test]
    fn natural_key_orders_versions() {
        assert!(natural_key("gpt-5.6") > natural_key("gpt-5.1"));
        assert!(natural_key("gpt-5.1") > natural_key("gpt-5"));
        assert!(natural_key("o4-mini") > natural_key("o3"));
        assert_eq!(natural_key("gpt-4o"), vec![4]);
        assert_eq!(natural_key("gpt-4o-2024-08-06"), vec![4, 2024, 8, 6]);
    }

    #[test]
    fn is_dated_flags_snapshots_only() {
        assert!(is_dated("gpt-4o-2024-05-13"));
        assert!(is_dated("gpt-3.5-turbo-1106"));
        assert!(is_dated("gpt-3.5-turbo-0125"));
        assert!(is_dated("claude-sonnet-4-20250514"));
        assert!(!is_dated("gpt-4o"));
        assert!(!is_dated("gpt-5.1"));
        assert!(!is_dated("o3"));
    }

    #[test]
    fn rank_shortlist_curates_openai_from_screenshot_list() {
        let models: Vec<String> = [
            "gpt-3.5-turbo",
            "gpt-3.5-turbo-16k",
            "gpt-3.5-turbo-instruct",
            "gpt-3.5-turbo-1106",
            "gpt-3.5-turbo-0125",
            "gpt-4o",
            "gpt-4o-2024-05-13",
            "gpt-4o-mini",
            "gpt-4o-mini-2024-07-18",
            "gpt-4o-2024-08-06",
            "o1-2024-12-17",
            "o1",
            "o3-mini",
            "o3-mini-2025-01-31",
            "o3",
            "o4-mini",
            "o4-mini-2025-04-16",
            "gpt-4o-mini-search-preview",
            "gpt-4o-search-preview",
            "o3-2025-04-16",
            "gpt-4.1",
            "gpt-5",
            "gpt-5.1",
            "gpt-5.6",
            "text-embedding-3-small",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let shortlist = rank_shortlist(Provider::OpenAI, &models);

        assert_eq!(
            shortlist,
            vec![
                "gpt-5.6".to_string(),
                "gpt-4.1".to_string(),
                "gpt-4o".to_string(),
                "o4-mini".to_string(),
                "o3".to_string(),
                "o1".to_string(),
            ]
        );
        for m in &shortlist {
            assert!(!m.contains("3.5"), "{m}");
            assert!(!m.contains("instruct"), "{m}");
            assert!(!m.contains("search"), "{m}");
            assert!(!is_dated(m), "{m}");
        }
    }

    #[test]
    fn rank_shortlist_ollama_shows_installed_unfiltered() {
        // Ollama: show whatever /api/tags returns, unchanged — including
        // `*-instruct` variants the generic chat-filter would drop, and in the
        // installed order (no curation).
        let models: Vec<String> = ["qwen3:30b-a3b", "qwen2.5-coder:7b-instruct", "gpt-oss:20b"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(rank_shortlist(Provider::Ollama, &models), models);
    }

    #[test]
    fn rank_shortlist_picks_newest_dated_when_family_has_only_snapshots() {
        let models: Vec<String> = [
            "claude-sonnet-4-20241022",
            "claude-sonnet-4-20250514",
            "claude-opus-4-20250101",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let shortlist = rank_shortlist(Provider::Anthropic, &models);
        assert_eq!(
            shortlist,
            vec![
                "claude-sonnet-4-20250514".to_string(),
                "claude-opus-4-20250101".to_string(),
            ]
        );
    }

    #[test]
    fn rank_shortlist_falls_back_when_no_family_matches() {
        let models = vec!["weird-model".to_string(), "another-thing".to_string()];
        let shortlist = rank_shortlist(Provider::OpenAI, &models);
        assert!(shortlist.contains(&"weird-model".to_string()));
        assert!(shortlist.contains(&"another-thing".to_string()));
    }

    fn sample_shortlist() -> Vec<String> {
        vec![
            "gpt-5.6".to_string(),
            "gpt-4.1".to_string(),
            "gpt-4o".to_string(),
        ]
    }

    #[test]
    fn ask_model_empty_uses_suggested() {
        let mut p = scripted("\n");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("gpt-5.6".to_string())
        );
    }

    #[test]
    fn ask_model_eof_uses_suggested() {
        let mut p = scripted("");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("gpt-5.6".to_string())
        );
    }

    #[test]
    fn ask_model_number_selects_from_shortlist() {
        let mut p = scripted("2\n");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("gpt-4.1".to_string())
        );
    }

    #[test]
    fn ask_model_typed_id_is_used_verbatim() {
        let mut p = scripted("my-finetune\n");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("my-finetune".to_string())
        );
    }

    #[test]
    fn ask_model_out_of_range_number_reprompts_then_typed() {
        let mut p = scripted("9\nmy-ft\n");
        assert_eq!(
            p.ask_model(&sample_shortlist(), 0).unwrap(),
            Some("my-ft".to_string())
        );
    }

    #[test]
    fn ask_model_non_interactive_returns_suggested() {
        assert_eq!(
            non_interactive().ask_model(&sample_shortlist(), 0).unwrap(),
            Some("gpt-5.6".to_string())
        );
        assert_eq!(non_interactive().ask_model(&[], 0).unwrap(), None);
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
    fn openai_config_references_native_env_var() {
        let spec = resolve(&args()).unwrap();
        let rendered = render_config(&spec);
        assert!(rendered.contains("provider = \"openai\""));
        assert!(rendered.contains("api_key = \"{{ env.OPENAI_API_KEY }}\""));
        assert!(rendered.contains("model = \"gpt-5.1\""));
        assert!(rendered.contains("name = \"assistant\""));
        toml::from_str::<toml::Value>(&rendered).unwrap();
    }

    #[test]
    fn bedrock_gets_region_and_no_key() {
        let mut a = args();
        a.provider = Some(Provider::Bedrock);
        a.region = Some("us-east-1".to_string());
        let spec = resolve(&a).unwrap();
        let rendered = render_config(&spec);
        assert!(rendered.contains("region = \"us-east-1\""));
        assert!(!rendered.contains("api_key"));
    }

    #[test]
    fn ollama_gets_base_url_and_no_key() {
        let mut a = args();
        a.provider = Some(Provider::Ollama);
        let spec = resolve(&a).unwrap();
        let rendered = render_config(&spec);
        assert!(rendered.contains(&format!("base_url = \"{DEFAULT_OLLAMA_URL}\"")));
        assert!(!rendered.contains("api_key"));
    }

    #[test]
    fn env_var_detected_skips_prompt_non_interactive() {
        let only_openai = |v: &str| v == "OPENAI_API_KEY";
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let spec = resolve_spec(
            &args(),
            &mut non_interactive(),
            &FailingLister,
            &only_openai,
            &detected,
        )
        .unwrap();
        assert_eq!(
            spec.api_key,
            Some(ApiKeySource::EnvVar("OPENAI_API_KEY".to_string()))
        );
    }

    #[test]
    fn env_var_detected_user_accepts() {
        // "y\n" accepts the detected key
        let only_openai = |v: &str| v == "OPENAI_API_KEY";
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let spec = resolve_spec(
            &args(),
            &mut scripted("y\n"),
            &FailingLister,
            &only_openai,
            &detected,
        )
        .unwrap();
        assert_eq!(
            spec.api_key,
            Some(ApiKeySource::EnvVar("OPENAI_API_KEY".to_string()))
        );
        let rendered = render_config(&spec);
        assert!(rendered.contains("api_key = \"{{ env.OPENAI_API_KEY }}\""));
    }

    #[test]
    fn env_var_detected_user_declines_and_provides_key() {
        // "n\n" declines, then provides a key
        let only_openai = |v: &str| v == "OPENAI_API_KEY";
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let spec = resolve_spec(
            &args(),
            &mut scripted("n\nsk-new\n"),
            &FailingLister,
            &only_openai,
            &detected,
        )
        .unwrap();
        assert_eq!(
            spec.api_key,
            Some(ApiKeySource::Provided {
                env_var: "OPENAI_API_KEY".to_string(),
                value: "sk-new".to_string(),
            })
        );
    }

    #[test]
    fn no_env_var_user_provides_key() {
        let spec = resolve_spec(
            &args(),
            &mut scripted("sk-manual\n"),
            &FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
        assert_eq!(
            spec.api_key,
            Some(ApiKeySource::Provided {
                env_var: "OPENAI_API_KEY".to_string(),
                value: "sk-manual".to_string(),
            })
        );
    }

    #[test]
    fn provided_key_is_used_for_verification() {
        let mut a = args();
        a.offline = false;
        let lister = RecordingLister {
            seen_key: std::cell::RefCell::new(None),
            models: vec!["gpt-5.1"],
        };
        // "n\n" declines detected key, "sk-override\n" provides new one
        let only_openai = |v: &str| v == "OPENAI_API_KEY";
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let spec = resolve_spec(
            &a,
            &mut scripted("n\nsk-override\n"),
            &lister,
            &only_openai,
            &detected,
        )
        .unwrap();
        assert_eq!(
            spec.api_key,
            Some(ApiKeySource::Provided {
                env_var: "OPENAI_API_KEY".to_string(),
                value: "sk-override".to_string(),
            })
        );
        assert_eq!(lister.seen_key.borrow().as_deref(), Some("sk-override"));
    }

    #[test]
    fn detected_key_is_used_for_verification() {
        let mut a = args();
        a.offline = false;
        let lister = RecordingLister {
            seen_key: std::cell::RefCell::new(None),
            models: vec!["gpt-5.1"],
        };
        let only_openai = |v: &str| v == "OPENAI_API_KEY";
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        // "y\n" accepts detected key
        let spec =
            resolve_spec(&a, &mut scripted("y\n"), &lister, &only_openai, &detected).unwrap();
        assert_eq!(
            spec.api_key,
            Some(ApiKeySource::EnvVar("OPENAI_API_KEY".to_string()))
        );
        assert_eq!(lister.seen_key.borrow().as_deref(), Some("sk-detected"));
    }

    #[test]
    fn ollama_has_no_api_key() {
        let mut a = args();
        a.provider = Some(Provider::Ollama);
        let spec = resolve_spec(
            &a,
            &mut non_interactive(),
            &FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.api_key, None);
    }

    #[test]
    fn bedrock_has_no_api_key() {
        let mut a = args();
        a.provider = Some(Provider::Bedrock);
        a.region = Some("us-east-1".to_string());
        let spec = resolve_spec(
            &a,
            &mut non_interactive(),
            &FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.api_key, None);
    }

    // ------------------------------------------------------------------
    // .env rendering
    // ------------------------------------------------------------------

    #[test]
    fn render_env_single_key() {
        let env = render_env("OPENAI_API_KEY", "sk-xyz");
        assert!(env.contains("OPENAI_API_KEY=sk-xyz"));
        assert!(!env.contains("LLM_"));
    }

    #[test]
    fn merge_env_preserves_unrelated() {
        let existing = "GITHUB_TOKEN=ghp_abc\nOPENAI_API_KEY=old\n";
        let env = merge_env(existing, "OPENAI_API_KEY", "sk-new");
        assert!(env.contains("GITHUB_TOKEN=ghp_abc"));
        assert!(env.contains("OPENAI_API_KEY=sk-new"));
        assert!(!env.contains("OPENAI_API_KEY=old"));
    }

    #[test]
    fn merge_env_appends_when_absent() {
        let existing = "GITHUB_TOKEN=ghp_abc\n";
        let env = merge_env(existing, "OPENAI_API_KEY", "sk-new");
        assert!(env.contains("GITHUB_TOKEN=ghp_abc"));
        assert!(env.contains("OPENAI_API_KEY=sk-new"));
    }

    #[test]
    fn merge_env_idempotent() {
        let once = merge_env("", "OPENAI_API_KEY", "sk-test");
        assert_eq!(merge_env(&once, "OPENAI_API_KEY", "sk-test"), once);
    }

    // ------------------------------------------------------------------
    // interactive flow
    // ------------------------------------------------------------------

    #[test]
    fn interactive_model_numbered_pick_through_resolve() {
        let mut a = args();
        a.provider = None;
        a.model = None;
        a.offline = false;
        let only_openai = |v: &str| v == "OPENAI_API_KEY";
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk".to_string());
        let lister = FixedLister(vec!["gpt-4o", "gpt-5.1", "gpt-4.1"]);
        // provider(enter) -> accept key(y) -> model(2)
        let spec = resolve_spec(
            &a,
            &mut scripted("\ny\n2\n"),
            &lister,
            &only_openai,
            &detected,
        )
        .unwrap();
        assert_eq!(spec.model, "gpt-4.1");
    }

    #[test]
    fn interactive_model_typed_off_list_is_kept() {
        let mut a = args();
        a.provider = None;
        a.model = None;
        a.offline = false;
        let only_openai = |v: &str| v == "OPENAI_API_KEY";
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk".to_string());
        let lister = FixedLister(vec!["gpt-4o", "gpt-5.1"]);
        // provider(enter) -> accept key(y) -> model(my-finetune)
        let spec = resolve_spec(
            &a,
            &mut scripted("\ny\nmy-finetune\n"),
            &lister,
            &only_openai,
            &detected,
        )
        .unwrap();
        assert_eq!(spec.model, "my-finetune");
    }

    #[test]
    fn interactive_model_defaults_to_suggestion() {
        let mut a = args();
        a.provider = None;
        a.model = None;
        a.offline = false;
        let only_openai = |var: &str| var == "OPENAI_API_KEY";
        let detected = |var: &str| (var == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let lister = FixedLister(vec!["gpt-4o", "gpt-5.1", "text-embedding-3-small"]);
        // provider(enter) -> accept key(enter=yes) -> model(enter=suggested)
        let spec = resolve_spec(
            &a,
            &mut scripted("\n\n\n"),
            &lister,
            &only_openai,
            &detected,
        )
        .unwrap();
        assert_eq!(spec.provider, Provider::OpenAI);
        assert_eq!(spec.model, "gpt-5.1");
        assert_eq!(
            spec.api_key,
            Some(ApiKeySource::EnvVar("OPENAI_API_KEY".to_string()))
        );
    }

    #[test]
    fn interactive_provider_by_number_uses_display_order() {
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
        assert_eq!(spec.provider, Provider::Gemini);
    }

    // ------------------------------------------------------------------
    // provider menu (numeric-only, re-prompts on bad input)
    // ------------------------------------------------------------------

    #[test]
    fn ask_choice_rejects_until_valid_number() {
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
        assert_eq!(spec.provider, Provider::Anthropic);
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
        assert_eq!(spec.provider, Provider::OpenAI);
    }

    #[test]
    fn provider_empty_no_default_reprompts() {
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
        assert_eq!(spec.provider, Provider::Anthropic);
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

    // (Invalid `--provider` values are now rejected by clap's ValueEnum at
    // parse time, so there's nothing for resolve_spec to validate.)

    #[test]
    fn lister_failure_warns_and_continues() {
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
    fn explicit_model_not_in_list_is_kept_with_warning() {
        let mut a = args();
        a.offline = false;
        a.model = Some("my-finetune".to_string());
        let lister = FixedLister(vec!["gpt-5.1"]);
        let spec = resolve_spec(&a, &mut non_interactive(), &lister, &no_keys, &no_values).unwrap();
        assert_eq!(spec.model, "my-finetune");
    }

    #[test]
    fn next_steps_mentions_config() {
        let s = next_steps(Path::new("config.toml"), false);
        assert!(s.contains("config.toml"), "got: {s}");
    }

    #[test]
    fn next_steps_mentions_env_when_written() {
        let s = next_steps(Path::new("config.toml"), true);
        assert!(s.contains(".env"), "got: {s}");
    }

    #[test]
    fn next_steps_cds_into_config_dir() {
        let s = next_steps(Path::new("proj/config.toml"), false);
        assert!(s.contains("cd proj"), "got: {s}");
        assert!(s.contains("CONFIG_PATH=config.toml"), "got: {s}");
    }

    #[cfg(feature = "standalone-cli")]
    #[test]
    fn validate_rendered_accepts_generated() {
        let spec = resolve(&args()).unwrap();
        let rendered = render_config(&spec);
        validate_rendered(&spec, &rendered).unwrap();
    }

    #[test]
    fn non_interactive_api_key_from_flag() {
        let mut a = args();
        a.api_key_env = Some("MY_KEY".to_string());
        let from_flag = |v: &str| (v == "MY_KEY").then(|| "sk-flag".to_string());
        let key_set = |v: &str| v == "MY_KEY";
        let spec = resolve_spec(
            &a,
            &mut non_interactive(),
            &FailingLister,
            &key_set,
            &from_flag,
        )
        .unwrap();
        assert_eq!(
            spec.api_key,
            Some(ApiKeySource::EnvVar("MY_KEY".to_string()))
        );
    }

    #[test]
    fn ask_yes_no_defaults() {
        let mut p = scripted("\n");
        assert!(p.ask_yes_no("test?", true).unwrap());
        let mut p = scripted("\n");
        assert!(!p.ask_yes_no("test?", false).unwrap());
    }

    #[test]
    fn ask_yes_no_explicit() {
        let mut p = scripted("y\n");
        assert!(p.ask_yes_no("test?", false).unwrap());
        let mut p = scripted("n\n");
        assert!(!p.ask_yes_no("test?", true).unwrap());
    }
}
