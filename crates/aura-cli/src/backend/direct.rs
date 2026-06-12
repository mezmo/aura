//! Direct backend — calls the same handler internals as the web server.
//!
//! Instead of reimplementing agent building and stream consumption, this backend
//! constructs an `AppState`, calls `prepare_request` / `execute_completion` from
//! `aura_web_server::handlers`, and parses the resulting SSE chunks through the
//! same `process_sse_events` parser used by the HTTP backend. This guarantees
//! identical behavior whether the CLI connects via HTTP or runs standalone.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::{Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use aura_web_server::handlers::{self, CollectedResult, DeliveryMode};
use aura_web_server::streaming::ToolResultMode;
use aura_web_server::types::{
    ActiveRequestTracker, AppState, ChatCompletionRequest, ChatMessage, ChatMessageFunctionCall,
    ChatMessageToolCall, ClientFunctionDefinition, ClientToolDefinition, Role,
};

use crate::api::stream::{StreamHandler, StreamResult, process_sse_events};
use crate::api::types::{Message, ToolCallInfo, ToolDefinition};
use crate::ui::prompt::get_selected_model;

/// Factory for `additional_tools` registered on every standalone agent.
///
/// Returns no tools. Standalone mode uses the same client-side tools path as
/// HTTP mode: tool defs are passed in the request, registered as passthrough
/// tools on the agent, and execution comes back to the REPL via SSE so the
/// REPL's permission system runs. Registering tools directly here would
/// bypass that loop and execute tools server-side without prompting.
///
/// `crate::tools::rig_tools::cli_tools_as_rig_tools` exists for library
/// consumers who do want the agent to execute CLI tools itself; it is not
/// wired into the CLI binary.
fn additional_tools_factory() -> Arc<dyn Fn() -> Vec<Box<dyn aura::ToolDyn>> + Send + Sync> {
    Arc::new(Vec::new)
}

/// Direct backend — holds AppState with loaded configs and CLI tool factory.
pub struct DirectBackend {
    app_state: Arc<AppState>,
    extra_headers: HashMap<String, String>,
}

/// Hot-reload hook for standalone mode: re-load the config path from disk,
/// swap the roster, and refresh the REPL's `/model` cache so the new agents
/// are immediately listable.
fn make_reload_hook(
    registry: Arc<aura_web_server::types::ConfigRegistry>,
    config_path: String,
) -> aura::bootstrap::ReloadHook {
    Arc::new(move || {
        let configs = aura_config::load_config(&config_path).map_err(|e| e.to_string())?;
        let mut names: Vec<String> = configs
            .iter()
            .map(|c| {
                c.agent
                    .alias
                    .as_deref()
                    .unwrap_or(&c.agent.name)
                    .to_string()
            })
            .collect();
        let count = names.len();
        registry.replace(configs);
        names.push(aura::bootstrap::BOOTSTRAP_AGENT_NAME.to_string());
        let summary = format!(
            "Hot reload applied; {count} agent(s) now live: {}",
            names[..count].join(", ")
        );
        crate::ui::prompt::seed_model_cache(names);
        Ok(summary)
    })
}

impl DirectBackend {
    /// Load configs and construct AppState, mirroring the web server's startup.
    ///
    /// When exactly one loaded config sets `[bootstrap] enabled = true`, the
    /// aura-bootstrap agent is served alongside the roster — same shared
    /// `prepare_request` routing as the web server. The token gate is an
    /// HTTP-layer boundary, so standalone generates a private token and
    /// presents it to itself on every request: the local operator already
    /// owns the config file.
    pub async fn from_toml(
        config_path: &str,
        extra_headers: Vec<(String, String)>,
    ) -> Result<Self> {
        let config_pairs = aura_config::load_config_with_paths(config_path)
            .context("Failed to load agent config")?;
        if config_pairs.is_empty() {
            anyhow::bail!("No agent config found in {}", config_path);
        }

        if let Some((_, config)) = config_pairs.iter().find(|(_, c)| {
            [Some(c.agent.name.as_str()), c.agent.alias.as_deref()]
                .into_iter()
                .flatten()
                .any(|id| id.eq_ignore_ascii_case(aura::bootstrap::BOOTSTRAP_AGENT_NAME))
        }) {
            anyhow::bail!(
                "agent '{}' uses the reserved name '{}'",
                config.agent.name,
                aura::bootstrap::BOOTSTRAP_AGENT_NAME
            );
        }

        let enablers: Vec<&(std::path::PathBuf, aura_config::Config)> = config_pairs
            .iter()
            .filter(|(_, c)| c.bootstrap.as_ref().is_some_and(|b| b.enabled))
            .collect();
        if enablers.len() > 1 {
            anyhow::bail!(
                "[bootstrap] is enabled in more than one config file — enable it in \
                 exactly one so the bootstrap agent's LLM and write target are unambiguous"
            );
        }
        let bootstrap_declaration = enablers
            .first()
            .map(|(path, config)| (path.clone(), config.clone()));

        let configs: Vec<aura_config::Config> =
            config_pairs.into_iter().map(|(_, config)| config).collect();
        let roster_names: Vec<String> = configs
            .iter()
            .map(|c| {
                c.agent
                    .alias
                    .as_deref()
                    .unwrap_or(&c.agent.name)
                    .to_string()
            })
            .collect();
        let registry = Arc::new(aura_web_server::types::ConfigRegistry::new(configs));

        let bootstrap = bootstrap_declaration.map(|(declaring_path, declaring)| {
            let target = aura::bootstrap::ConfigTarget {
                config_path: std::path::PathBuf::from(config_path),
                target: declaring_path,
            };
            let reload = make_reload_hook(registry.clone(), config_path.to_string());
            aura_web_server::types::BootstrapState {
                agent_config: aura::bootstrap::bootstrap_agent_config(
                    &declaring,
                    &target,
                    &roster_names,
                ),
                token: uuid::Uuid::new_v4().to_string(),
                tools: aura::bootstrap::bootstrap_tools_factory(target, reload),
            }
        });

        // Self-present the private token on every request so the shared
        // routing in `prepare_request` admits bootstrap traffic.
        let mut headers_map: HashMap<String, String> = extra_headers.into_iter().collect();
        if let Some(bs) = &bootstrap {
            headers_map.insert("x-aura-bootstrap-token".to_string(), bs.token.clone());
        }

        let app_state = Arc::new(AppState {
            configs: registry,
            bootstrap,
            tool_result_mode: ToolResultMode::Aura,
            tool_result_max_length: 0,
            streaming_buffer_size: 400,
            aura_custom_events: true,
            aura_emit_reasoning: true,
            streaming_timeout_secs: 900,
            first_chunk_timeout_secs: 30,
            shutdown_token: CancellationToken::new(),
            stream_shutdown_token: CancellationToken::new(),
            active_requests: Arc::new(ActiveRequestTracker::new()),
            default_agent: None,
            additional_tools: additional_tools_factory(),
        });

        Ok(Self {
            app_state,
            extra_headers: headers_map,
        })
    }

    /// Return `true` if any loaded config enables client-side tools.
    ///
    /// A config qualifies when it is single-agent (orchestration disabled)
    /// **and** has `[agent].enable_client_tools = true`. Used to surface a
    /// startup warning when the user passed `--enable-client-tools` but
    /// no loaded config opted in — the request would otherwise silently
    /// produce a chat-only experience.
    pub fn any_agent_enables_client_tools(&self) -> bool {
        self.app_state
            .configs
            .snapshot()
            .iter()
            .any(|c| !c.orchestration_enabled() && c.agent.enable_client_tools)
    }

    /// Return the effective model ID for each loaded config, plus the
    /// bootstrap agent when it is enabled.
    pub(crate) fn model_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .app_state
            .configs
            .snapshot()
            .iter()
            .map(|c| {
                c.agent
                    .alias
                    .as_deref()
                    .unwrap_or(&c.agent.name)
                    .to_string()
            })
            .collect();
        if self.app_state.bootstrap.is_some() {
            ids.push(aura::bootstrap::BOOTSTRAP_AGENT_NAME.to_string());
        }
        ids
    }

    /// Whether more than one agent config is loaded.
    pub fn has_multiple_configs(&self) -> bool {
        self.app_state.configs.snapshot().len() > 1
    }

    /// Check if `model_name` matches any loaded config's agent.name or agent.alias.
    /// Case-insensitive comparison against both name and alias independently.
    /// Returns the canonical effective ID (alias or name) on match.
    pub fn find_matching_model(&self, model_name: &str) -> Option<String> {
        let lower = model_name.to_lowercase();
        if self.app_state.bootstrap.is_some() && lower == aura::bootstrap::BOOTSTRAP_AGENT_NAME {
            return Some(aura::bootstrap::BOOTSTRAP_AGENT_NAME.to_string());
        }
        for config in self.app_state.configs.snapshot().iter() {
            let effective_id = config
                .agent
                .alias
                .as_deref()
                .unwrap_or(&config.agent.name)
                .to_string();

            // Match against effective ID, name, or alias independently
            if effective_id.to_lowercase() == lower
                || config.agent.name.to_lowercase() == lower
                || config
                    .agent
                    .alias
                    .as_ref()
                    .is_some_and(|a| a.to_lowercase() == lower)
            {
                return Some(effective_id);
            }
        }
        None
    }

    /// Get the system prompt from the config matching `model` (or first config if None).
    pub fn get_config_system_prompt(&self, model: Option<&str>) -> Option<String> {
        let configs = self.app_state.configs.snapshot();
        let config = if let Some(model_name) = model {
            let lower = model_name.to_lowercase();
            configs.iter().find(|c| {
                let effective = c
                    .agent
                    .alias
                    .as_deref()
                    .unwrap_or(&c.agent.name)
                    .to_lowercase();
                effective == lower
            })
        } else {
            configs.first()
        };
        config.map(|c| c.agent.system_prompt.clone())
    }

    /// Replace the system prompt in the config matching `model` (or first config if None).
    pub fn override_system_prompt(&mut self, model: Option<&str>, new_prompt: String) {
        let mut configs: Vec<_> = (*self.app_state.configs.snapshot()).clone();
        let target = if let Some(model_name) = model {
            let lower = model_name.to_lowercase();
            configs.iter_mut().find(|c| {
                let effective = c
                    .agent
                    .alias
                    .as_deref()
                    .unwrap_or(&c.agent.name)
                    .to_lowercase();
                effective == lower
            })
        } else {
            configs.first_mut()
        };
        if let Some(config) = target {
            config.agent.system_prompt = new_prompt;
        }
        self.app_state.configs.replace(configs);
    }

    /// Convert CLI messages to web server ChatMessage format, preserving
    /// `tool_calls` on assistant messages and the full set of fields needed
    /// for `role: "tool"` follow-ups so the server's client-side tools path
    /// can reconstruct conversation history correctly.
    fn build_chat_request(
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        model: Option<String>,
    ) -> ChatCompletionRequest {
        let web_messages: Vec<ChatMessage> = messages.iter().map(convert_cli_message).collect();
        let web_tools: Option<Vec<ClientToolDefinition>> = tools
            .filter(|t| !t.is_empty())
            .map(|t| t.iter().map(convert_cli_tool_def).collect());

        ChatCompletionRequest {
            model,
            messages: web_messages,
            max_tokens: None,
            stream: Some(true),
            metadata: None,
            tools: web_tools,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn stream_chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
        session_id: &str,
        cancel: Arc<AtomicBool>,
        handler: &mut impl StreamHandler,
    ) -> Result<StreamResult> {
        let selected = get_selected_model();
        let mut req = Self::build_chat_request(messages, tools, selected);

        let setup =
            handlers::prepare_request(&self.app_state, &mut req, session_id, &self.extra_headers)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

        let config = handlers::build_completion_config(&self.app_state, &setup, None, true, true);

        let (chunk_tx, chunk_rx) =
            mpsc::channel::<Result<bytes::Bytes, String>>(self.app_state.streaming_buffer_size);
        let heartbeat_interval = Duration::from_secs(15);

        tokio::spawn(
            handlers::execute_completion(
                setup,
                config,
                DeliveryMode::Sse {
                    chunk_tx,
                    heartbeat_interval,
                },
            )
            .instrument(tracing::info_span!(parent: None, "agent.stream")),
        );

        // Convert the mpsc receiver into an eventsource stream.
        // Each chunk from execute_completion is a complete SSE-formatted message
        // (e.g., "data: {json}\n\n" or "event: aura.tool_requested\ndata: {json}\n\n").
        // Use filter + map (synchronous) instead of filter_map (async) to stay Unpin.
        let byte_stream = ReceiverStream::new(chunk_rx)
            .filter(|r| std::future::ready(r.is_ok()))
            .map(|r| r.map_err(std::io::Error::other));
        let sse_stream = byte_stream.eventsource();

        // Parse SSE events through the same parser used by the HTTP backend
        process_sse_events(sse_stream, cancel, handler).await
    }

    pub async fn summarize(
        &self,
        text: &str,
        session_id: &str,
    ) -> Result<(String, Option<(u64, u64)>)> {
        let selected = get_selected_model();

        let prompt = format!(
            "You are a title generator. Given an assistant response, produce a single short \
             plain-text title (max 72 chars) that summarizes it. No markdown, no quotes, no \
             punctuation at the end. Just the title.\n\n{}",
            text
        );

        let mut req = ChatCompletionRequest {
            model: selected,
            messages: vec![ChatMessage {
                role: Role::User,
                content: Some(prompt),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            max_tokens: None,
            stream: Some(false),
            metadata: None,
            tools: None,
        };

        let setup =
            handlers::prepare_request(&self.app_state, &mut req, session_id, &self.extra_headers)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

        let config = handlers::build_completion_config(&self.app_state, &setup, None, false, false);

        let (result_tx, result_rx) = tokio::sync::oneshot::channel::<CollectedResult>();

        tokio::spawn(
            handlers::execute_completion(setup, config, DeliveryMode::Collect { result_tx })
                .instrument(tracing::info_span!(parent: None, "agent.stream.summarize")),
        );

        match result_rx.await {
            Ok(collected) => {
                let usage = collected
                    .outcome
                    .usage
                    .map(|u| (u.prompt_tokens, u.completion_tokens));
                Ok((collected.outcome.content.trim().to_string(), usage))
            }
            Err(_) => Ok(("Response".to_string(), None)),
        }
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        Ok(self.model_ids())
    }
}

/// Map the CLI-side `Message` (OpenAI-shaped, with role as a string) to the
/// web-server `ChatMessage` (typed `Role` enum, separate tool fields).
fn convert_cli_message(m: &Message) -> ChatMessage {
    let role = match m.role.as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "system" => Role::System,
        "tool" => Role::Tool,
        _ => Role::Unknown,
    };
    let tool_calls = m
        .tool_calls
        .as_ref()
        .map(|calls| calls.iter().map(convert_cli_tool_call).collect());
    ChatMessage {
        role,
        content: m.content.clone(),
        tool_calls,
        tool_call_id: m.tool_call_id.clone(),
        name: m.name.clone(),
    }
}

/// Map a CLI-side `ToolCallInfo` to a web-server `ChatMessageToolCall`.
fn convert_cli_tool_call(tc: &ToolCallInfo) -> ChatMessageToolCall {
    ChatMessageToolCall {
        id: tc.id.clone(),
        call_type: tc.call_type.clone(),
        function: ChatMessageFunctionCall {
            name: tc.function.name.clone(),
            arguments: tc.function.arguments.clone(),
        },
    }
}

/// Map a CLI-side `ToolDefinition` to a web-server `ClientToolDefinition`,
/// preserving the OpenAI `{ type, function: { name, description, parameters } }` shape.
fn convert_cli_tool_def(def: &ToolDefinition) -> ClientToolDefinition {
    ClientToolDefinition {
        tool_type: def.tool_type.clone(),
        function: ClientFunctionDefinition {
            name: def.function.name.clone(),
            description: Some(def.function.description.clone()),
            parameters: Some(def.function.parameters.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::Message;

    /// Construct a DirectBackend with test configs (no real TOML loading).
    fn make_backend(configs: Vec<aura_config::Config>) -> DirectBackend {
        let app_state = Arc::new(AppState {
            configs: Arc::new(aura_web_server::types::ConfigRegistry::new(configs)),
            bootstrap: None,
            tool_result_mode: ToolResultMode::Aura,
            tool_result_max_length: 0,
            streaming_buffer_size: 400,
            aura_custom_events: true,
            aura_emit_reasoning: true,
            streaming_timeout_secs: 900,
            first_chunk_timeout_secs: 30,
            shutdown_token: CancellationToken::new(),
            stream_shutdown_token: CancellationToken::new(),
            active_requests: Arc::new(ActiveRequestTracker::new()),
            default_agent: None,
            additional_tools: additional_tools_factory(),
        });
        DirectBackend {
            app_state,
            extra_headers: HashMap::new(),
        }
    }

    /// Create a Config with the given agent name, alias, and system prompt.
    fn make_config(name: &str, alias: Option<&str>, system_prompt: &str) -> aura_config::Config {
        let mut config = aura_config::Config::default();
        config.agent.name = name.to_string();
        config.agent.alias = alias.map(|a| a.to_string());
        config.agent.system_prompt = system_prompt.to_string();
        config
    }

    // -----------------------------------------------------------------------
    // standalone bootstrap host
    // -----------------------------------------------------------------------

    /// Complete config (ollama, no env deps) with the bootstrap agent enabled.
    const BOOTSTRAP_ENABLED: &str = r#"
[agent]
name = "assistant"
system_prompt = "You are helpful."

[agent.llm]
provider = "ollama"
model = "qwen3:8b"

[bootstrap]
enabled = true
"#;

    #[tokio::test]
    async fn from_toml_serves_bootstrap_agent_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, BOOTSTRAP_ENABLED).unwrap();

        let backend = DirectBackend::from_toml(path.to_str().unwrap(), vec![])
            .await
            .unwrap();

        assert_eq!(
            backend.model_ids(),
            vec!["assistant".to_string(), "aura-bootstrap".to_string()]
        );
        assert_eq!(
            backend.find_matching_model("aura-bootstrap"),
            Some("aura-bootstrap".to_string())
        );
        // The private token is self-presented so shared routing admits it.
        assert!(backend.extra_headers.contains_key("x-aura-bootstrap-token"));
    }

    #[tokio::test]
    async fn from_toml_without_bootstrap_serves_roster_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            BOOTSTRAP_ENABLED.replace("enabled = true", "enabled = false"),
        )
        .unwrap();

        let backend = DirectBackend::from_toml(path.to_str().unwrap(), vec![])
            .await
            .unwrap();

        assert_eq!(backend.model_ids(), vec!["assistant".to_string()]);
        assert_eq!(backend.find_matching_model("aura-bootstrap"), None);
        assert!(!backend.extra_headers.contains_key("x-aura-bootstrap-token"));
    }

    #[tokio::test]
    async fn from_toml_rejects_reserved_agent_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            BOOTSTRAP_ENABLED.replace("name = \"assistant\"", "name = \"aura-bootstrap\""),
        )
        .unwrap();

        let err = DirectBackend::from_toml(path.to_str().unwrap(), vec![])
            .await
            .err()
            .expect("expected reserved-name error");
        assert!(err.to_string().contains("reserved"), "got: {err}");
    }

    #[tokio::test]
    async fn reload_hook_swaps_roster() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, BOOTSTRAP_ENABLED).unwrap();

        let registry = Arc::new(aura_web_server::types::ConfigRegistry::new(vec![
            make_config("assistant", None, "old"),
        ]));
        let hook = make_reload_hook(registry.clone(), path.to_str().unwrap().to_string());

        std::fs::write(
            &path,
            BOOTSTRAP_ENABLED.replace("name = \"assistant\"", "name = \"renamed\""),
        )
        .unwrap();

        let summary = hook().expect("reload should succeed");
        assert!(summary.contains("renamed"), "got: {summary}");
        assert_eq!(registry.snapshot()[0].agent.name, "renamed");

        // A broken on-disk config reports an error and leaves the roster.
        std::fs::write(&path, "not [valid").unwrap();
        assert!(hook().is_err());
        assert_eq!(registry.snapshot()[0].agent.name, "renamed");
    }

    // -----------------------------------------------------------------------
    // find_matching_model
    // -----------------------------------------------------------------------

    #[test]
    fn find_matching_model_by_name() {
        let backend = make_backend(vec![make_config("Math Agent", None, "You do math.")]);
        assert_eq!(
            backend.find_matching_model("Math Agent"),
            Some("Math Agent".to_string())
        );
    }

    #[test]
    fn find_matching_model_by_alias() {
        let backend = make_backend(vec![make_config("Math Agent", Some("math"), "prompt")]);
        assert_eq!(
            backend.find_matching_model("math"),
            Some("math".to_string())
        );
    }

    #[test]
    fn find_matching_model_by_name_when_alias_exists() {
        let backend = make_backend(vec![make_config("Math Agent", Some("math"), "prompt")]);
        assert_eq!(
            backend.find_matching_model("Math Agent"),
            Some("math".to_string())
        );
    }

    #[test]
    fn find_matching_model_case_insensitive() {
        let backend = make_backend(vec![make_config("Math Agent", None, "prompt")]);
        assert_eq!(
            backend.find_matching_model("math agent"),
            Some("Math Agent".to_string())
        );
    }

    #[test]
    fn find_matching_model_no_match() {
        let backend = make_backend(vec![make_config("Math Agent", None, "prompt")]);
        assert_eq!(backend.find_matching_model("Code Agent"), None);
    }

    #[test]
    fn find_matching_model_multiple_configs() {
        let backend = make_backend(vec![
            make_config("Math Agent", Some("math"), "prompt"),
            make_config("Code Agent", Some("code"), "prompt"),
        ]);
        assert_eq!(
            backend.find_matching_model("code"),
            Some("code".to_string())
        );
        assert_eq!(
            backend.find_matching_model("Math Agent"),
            Some("math".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // has_multiple_configs
    // -----------------------------------------------------------------------

    #[test]
    fn has_multiple_configs_true() {
        let backend = make_backend(vec![make_config("A", None, ""), make_config("B", None, "")]);
        assert!(backend.has_multiple_configs());
    }

    #[test]
    fn has_multiple_configs_false() {
        let backend = make_backend(vec![make_config("A", None, "")]);
        assert!(!backend.has_multiple_configs());
    }

    // -----------------------------------------------------------------------
    // model_ids
    // -----------------------------------------------------------------------

    #[test]
    fn model_ids_uses_alias_when_present() {
        let backend = make_backend(vec![
            make_config("Math Agent", Some("math"), ""),
            make_config("Code Agent", None, ""),
        ]);
        assert_eq!(backend.model_ids(), vec!["math", "Code Agent"]);
    }

    // -----------------------------------------------------------------------
    // get_config_system_prompt
    // -----------------------------------------------------------------------

    #[test]
    fn get_config_system_prompt_first_when_none() {
        let backend = make_backend(vec![
            make_config("A", None, "first prompt"),
            make_config("B", None, "second prompt"),
        ]);
        assert_eq!(
            backend.get_config_system_prompt(None),
            Some("first prompt".to_string())
        );
    }

    #[test]
    fn get_config_system_prompt_by_model() {
        let backend = make_backend(vec![
            make_config("Math", Some("math"), "math prompt"),
            make_config("Code", Some("code"), "code prompt"),
        ]);
        assert_eq!(
            backend.get_config_system_prompt(Some("code")),
            Some("code prompt".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // override_system_prompt
    // -----------------------------------------------------------------------

    #[test]
    fn override_system_prompt_first_when_none() {
        let mut backend = make_backend(vec![make_config("A", None, "original")]);
        backend.override_system_prompt(None, "overridden".to_string());
        assert_eq!(
            backend.get_config_system_prompt(None),
            Some("overridden".to_string())
        );
    }

    #[test]
    fn override_system_prompt_by_model() {
        let mut backend = make_backend(vec![
            make_config("Math", Some("math"), "math original"),
            make_config("Code", Some("code"), "code original"),
        ]);
        backend.override_system_prompt(Some("code"), "code overridden".to_string());
        assert_eq!(
            backend.get_config_system_prompt(Some("code")),
            Some("code overridden".to_string())
        );
        // Other config unchanged
        assert_eq!(
            backend.get_config_system_prompt(Some("math")),
            Some("math original".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // build_chat_request
    // -----------------------------------------------------------------------

    #[test]
    fn user_role_mapping() {
        let msgs = vec![Message::user("hello")];
        let req = DirectBackend::build_chat_request(&msgs, None, None);
        assert_eq!(req.messages[0].role, Role::User);
    }

    #[test]
    fn assistant_role_mapping() {
        let msgs = vec![Message::assistant("hi")];
        let req = DirectBackend::build_chat_request(&msgs, None, None);
        assert_eq!(req.messages[0].role, Role::Assistant);
    }

    #[test]
    fn system_role_mapping() {
        let msgs = vec![Message::system("you are helpful")];
        let req = DirectBackend::build_chat_request(&msgs, None, None);
        assert_eq!(req.messages[0].role, Role::System);
    }

    #[test]
    fn tool_role_mapping_carries_call_id() {
        // Tool follow-ups must arrive as `Role::Tool` with the originating
        // tool_call_id so the server's client-side tools path can correlate
        // them with the assistant's prior tool_call.
        let msgs = vec![Message::tool_result("call_1", "Read", "content")];
        let req = DirectBackend::build_chat_request(&msgs, None, None);
        assert_eq!(req.messages[0].role, Role::Tool);
        assert_eq!(req.messages[0].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(req.messages[0].content.as_deref(), Some("content"));
    }

    #[test]
    fn assistant_with_tool_calls_round_trips() {
        // Empty content + tool_calls must serialize to the wire as `content: null`
        // (Option<String> = None), with the tool_calls preserved.
        let msgs = vec![Message::assistant_with_tool_calls(None, vec![])];
        let req = DirectBackend::build_chat_request(&msgs, None, None);
        assert!(req.messages[0].content.is_none());
        assert!(req.messages[0].tool_calls.is_some());
    }

    #[test]
    fn model_passed_through() {
        let msgs = vec![Message::user("hi")];
        let req = DirectBackend::build_chat_request(&msgs, None, Some("gpt-4".to_string()));
        assert_eq!(req.model, Some("gpt-4".to_string()));
    }

    #[test]
    fn model_none_passed_through() {
        let msgs = vec![Message::user("hi")];
        let req = DirectBackend::build_chat_request(&msgs, None, None);
        assert!(req.model.is_none());
    }

    #[test]
    fn stream_always_true() {
        let msgs = vec![Message::user("hi")];
        let req = DirectBackend::build_chat_request(&msgs, None, None);
        assert_eq!(req.stream, Some(true));
    }

    #[test]
    fn empty_tools_omits_field() {
        let msgs = vec![Message::user("hi")];
        let req = DirectBackend::build_chat_request(&msgs, Some(&[]), None);
        assert!(req.tools.is_none());
    }

    #[test]
    fn tools_round_trip() {
        use crate::api::types::{FunctionDefinition, ToolDefinition};
        let tools = vec![ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "Shell".to_string(),
                description: "run a shell command".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            },
        }];
        let msgs = vec![Message::user("hi")];
        let req = DirectBackend::build_chat_request(&msgs, Some(&tools), None);
        let tools = req.tools.expect("tools should be present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "Shell");
    }
}
