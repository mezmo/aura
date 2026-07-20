//! Resolution: fold flags, environment sensing, key verification, and prompts
//! into a fully-resolved `ConfigSpec`. Network access is confined to the
//! injected `ModelLister`; environment reads to the injected closures — so the
//! whole flow is driven from scripted input in tests.

use std::io::BufRead;

use anyhow::{Result, bail};

use super::InitArgs;
use super::model_list::{ModelList, ModelLister};
use super::prompt::Prompter;
use super::provider::{DEFAULT_OLLAMA_URL, Provider, default_key_env, list_verifies_key};
use super::ranking::{provider_display_order, rank_shortlist, sensed_providers};

/// How the API key is sourced — determines what goes into the config and
/// whether a `.env` needs to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApiKeySource {
    /// The key lives in an existing environment variable; the config
    /// references it directly via `{{ env.<var_name> }}`. No `.env` write.
    EnvVar(String),
    /// The user typed a key at the prompt; it's written to `.env` under the
    /// provider's conventional env var name, and the config references that.
    Provided { env_var: String, value: String },
}

/// Fully resolved inputs (after flags, sensing, verification, prompts).
#[derive(Debug)]
pub(crate) struct ConfigSpec {
    pub(crate) provider: Provider,
    pub(crate) model: String,
    pub(crate) api_key: Option<ApiKeySource>,
    pub(crate) region: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) name: String,
    pub(crate) bootstrap: bool,
}

impl ConfigSpec {
    pub(crate) fn api_key_env_var(&self) -> Option<&str> {
        match &self.api_key {
            Some(ApiKeySource::EnvVar(var)) => Some(var),
            Some(ApiKeySource::Provided { env_var, .. }) => Some(env_var),
            None => None,
        }
    }
}

/// Resolve all inputs. Network access only through `lister`; environment
/// reads only through `key_is_set` / `key_value`.
pub(crate) fn resolve_spec<R: BufRead>(
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
        if (key_value)(env_var).is_some() {
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
        Some(m) => m.clone(),
        None => {
            let shortlist = match &live_models {
                Some(models) => rank_shortlist(provider, models),
                None => Vec::new(),
            };
            if prompter.interactive && shortlist.is_empty() {
                // No curated shortlist (e.g. OpenRouter's huge catalog) — the
                // operator types the id they want.
                if let Some(models) = &live_models {
                    println!(
                        "\n{provider} lists {} model(s); enter the model id you want.\n",
                        models.len()
                    );
                }
            }
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
            match prompter.ask_model(&shortlist, 0)? {
                Some(m) => m,
                None => bail!("--model is required in non-interactive mode"),
            }
        }
    };

    // Warn once if the chosen model isn't in the provider's live list — typed
    // ids and offline runs may legitimately miss it.
    if let Some(models) = &live_models
        && !models.iter().any(|x| x == &model)
    {
        eprintln!(
            "warning: '{model}' is not in {provider}'s model list — \
             continuing anyway"
        );
    }

    // ---- bootstrap agent ----
    // The flag wins outright; otherwise offer it interactively (default no —
    // it is a standing admin surface, so enabling stays a deliberate choice).
    let bootstrap = args.bootstrap
        || prompter.ask_yes_no(
            "\nEnable the aura-bootstrap agent? It lets you edit this config \
             by chatting with it (token-gated; the token is printed at server \
             startup).",
            false,
        )?;

    Ok(ConfigSpec {
        provider,
        model,
        api_key,
        region,
        base_url,
        name: args.name.clone(),
        bootstrap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::render::render_config;
    use crate::init::test_support::{
        FixedLister, RecordingLister, args, no_keys, no_values, non_interactive, resolve, scripted,
    };

    #[test]
    fn env_var_detected_skips_prompt_non_interactive() {
        let only_openai = |v: &str| v == "OPENAI_API_KEY";
        let detected = |v: &str| (v == "OPENAI_API_KEY").then(|| "sk-detected".to_string());
        let spec = resolve_spec(
            &args(),
            &mut non_interactive(),
            &crate::init::test_support::FailingLister,
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
            &crate::init::test_support::FailingLister,
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
            &crate::init::test_support::FailingLister,
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
            &crate::init::test_support::FailingLister,
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
            &crate::init::test_support::FailingLister,
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
            &crate::init::test_support::FailingLister,
            &no_keys,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.api_key, None);
    }

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
        let lister = FixedLister(vec!["gpt-5.4", "gpt-5.5", "text-embedding-3-small"]);
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
        assert_eq!(spec.model, "gpt-5.5");
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
            &crate::init::test_support::FailingLister,
            &gemini_only,
            &no_values,
        )
        .unwrap();
        assert_eq!(spec.provider, Provider::Gemini);
    }

    #[test]
    fn provider_reprompt_then_selects() {
        let mut a = args();
        a.provider = None;
        a.offline = true;
        let spec = resolve_spec(
            &a,
            &mut scripted("9\n0\nfoo\n2\n"),
            &crate::init::test_support::FailingLister,
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
            &crate::init::test_support::FailingLister,
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
            &crate::init::test_support::FailingLister,
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
            &crate::init::test_support::FailingLister,
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
    fn non_interactive_api_key_from_flag() {
        let mut a = args();
        a.api_key_env = Some("MY_KEY".to_string());
        let from_flag = |v: &str| (v == "MY_KEY").then(|| "sk-flag".to_string());
        let key_set = |v: &str| v == "MY_KEY";
        let spec = resolve_spec(
            &a,
            &mut non_interactive(),
            &crate::init::test_support::FailingLister,
            &key_set,
            &from_flag,
        )
        .unwrap();
        assert_eq!(
            spec.api_key,
            Some(ApiKeySource::EnvVar("MY_KEY".to_string()))
        );
    }
}
