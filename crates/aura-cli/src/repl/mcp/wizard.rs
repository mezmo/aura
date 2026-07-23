//! Guided `/mcp add` flow: pick a catalog server (or define a custom one),
//! collect credentials with masked input, and write the result to the
//! standalone config via `aura_config::writer` plus `.env` for secrets.
//!
//! Standalone mode only — HTTP mode's config lives on the remote server.
//! The flow is deliberately deterministic: no LLM ever sees a credential.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use aura_config::config::McpServerConfig;

use super::catalog::{CATALOG, CatalogEntry, Template};
use crate::backend::Backend;
use crate::repl::registry::CommandContext;
use crate::theme::{AuraStyle, Themed};

/// How the wizard ended.
enum WizardEnd {
    Written {
        name: String,
        starter_prompt: Option<&'static str>,
        verified: bool,
    },
    Aborted,
    Failed(String),
}

pub(super) fn run(ctx: &mut CommandContext) {
    // The REPL loop hides the cursor before dispatching a command and only
    // re-shows it on the next prompt; this handler reads input, so bring
    // the cursor back for its prompts.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::cursor::Show);
    let Backend::Direct(direct) = ctx.backend else {
        println!(
            "/mcp add edits the local agent config, so it is only available in \
             standalone mode (run without --api-url)."
        );
        return;
    };
    let config_path = direct.config_path().to_path_buf();
    if !config_path.is_file() {
        println!(
            "The agent config was loaded from `{}`, which is not a single file. \
             /mcp add can only edit a single-file config; add the server to one \
             of the TOML files in that directory manually.",
            config_path.display()
        );
        return;
    }

    match drive(ctx, &config_path) {
        WizardEnd::Written {
            name,
            starter_prompt,
            verified,
        } => {
            if verified {
                println!(
                    "\n{} `{name}` added to {} and verified.",
                    "✓".themed(AuraStyle::Success),
                    config_path.display()
                );
                println!("Restart aura to activate the new server.");
                if let Some(prompt) = starter_prompt {
                    println!(
                        "\nTry this once it's live:\n  {}",
                        prompt.themed(AuraStyle::Emphasis)
                    );
                }
            } else {
                println!(
                    "\n`{name}` added to {} (connection not verified).",
                    config_path.display()
                );
                println!("Restart aura to activate the new server once it's reachable.");
            }
        }
        WizardEnd::Aborted => println!("\n/mcp add aborted — nothing was written."),
        WizardEnd::Failed(reason) => {
            println!("\n{} {reason}", "error:".themed(AuraStyle::Error));
        }
    }
}

/// Interactive body. Every early `return` before the config write means no
/// file was touched; `.env` is only written after the config upsert succeeds.
fn drive(ctx: &mut CommandContext, config_path: &Path) -> WizardEnd {
    println!("{}", "Add an MCP server".themed(AuraStyle::Heading));
    for (i, entry) in CATALOG.iter().enumerate() {
        println!(
            "  {} {} {} {}",
            format!("{}.", i + 1).themed(AuraStyle::Muted),
            entry.key.themed(AuraStyle::Heading),
            "—".themed(AuraStyle::Muted),
            entry.description.themed(AuraStyle::Muted),
        );
    }
    println!(
        "  {} {} {} {}",
        format!("{}.", CATALOG.len() + 1).themed(AuraStyle::Muted),
        "custom".themed(AuraStyle::Heading),
        "—".themed(AuraStyle::Muted),
        "define transport, URL/command, and auth yourself".themed(AuraStyle::Muted),
    );

    let entry = loop {
        let Some(answer) = ask(ctx, &format!("Server to add [1-{}]: ", CATALOG.len() + 1)) else {
            return WizardEnd::Aborted;
        };
        match answer.parse::<usize>() {
            Ok(n) if (1..=CATALOG.len()).contains(&n) => break Some(&CATALOG[n - 1]),
            Ok(n) if n == CATALOG.len() + 1 => break None,
            _ => println!("Enter a number between 1 and {}.", CATALOG.len() + 1),
        }
    };

    let collected = match entry {
        Some(entry) => collect_catalog(ctx, entry, config_path),
        None => collect_custom(ctx, config_path),
    };
    let Some(collected) = collected else {
        return WizardEnd::Aborted;
    };

    let new_secrets: Vec<(String, String)> = collected
        .secrets
        .iter()
        .filter_map(|source| match source {
            CredentialSource::New { env_var, value } => Some((env_var.clone(), value.clone())),
            CredentialSource::Existing { .. } => None,
        })
        .collect();
    let mut existing_vars: Vec<&str> = collected
        .secrets
        .iter()
        .filter_map(|source| match source {
            CredentialSource::Existing { env_var, .. } => Some(env_var.as_str()),
            CredentialSource::New { .. } => None,
        })
        .collect();
    existing_vars.dedup();
    let mut unresolved: Vec<&str> = collected
        .secrets
        .iter()
        .filter(|source| source.known_value().is_none())
        .map(CredentialSource::env_var)
        .collect();
    unresolved.dedup();
    let known_values: Vec<(String, String)> = collected
        .secrets
        .iter()
        .filter_map(|source| {
            source
                .known_value()
                .map(|value| (source.env_var().to_string(), value.to_string()))
        })
        .collect();

    // Verify BEFORE asking to write, so consent to the config change is
    // informed by a live connection result and the discovered tools — and
    // a failing credential never lands on disk unless explicitly chosen.
    // The check runs against an in-memory copy; nothing is written yet.
    let mut verified = false;
    let mut tool_names: Vec<String> = Vec::new();
    if !unresolved.is_empty() {
        println!(
            "\nSkipping the connection check — {} not set in this session.",
            unresolved.join(", ")
        );
    } else {
        println!(
            "\nVerifying connection to `{}` (up to {}s)...",
            collected.name,
            VERIFY_TIMEOUT.as_secs()
        );
        match verify_server(
            ctx.rt,
            &collected.name,
            inline_secrets(&collected.server, &known_values),
        ) {
            Ok((aura::mcp::ConnectionStatus::Connected, names)) => {
                println!(
                    "{} Connected — {} tool(s) discovered.",
                    "✓".themed(AuraStyle::Success),
                    names.len()
                );
                if !names.is_empty() {
                    println!("  {}", entries_summary(&names).themed(AuraStyle::Muted));
                }
                verified = true;
                tool_names = names;
            }
            Ok((aura::mcp::ConnectionStatus::Failed(reason), _)) => println!(
                "{} Connection failed: {reason}",
                "✗".themed(AuraStyle::Error)
            ),
            Ok((aura::mcp::ConnectionStatus::NotAttempted, _)) => println!(
                "{} The connection was not attempted.",
                "!".themed(AuraStyle::Warning)
            ),
            Err(reason) => println!(
                "{} Could not verify the connection: {reason}",
                "!".themed(AuraStyle::Warning)
            ),
        }
    }

    // Preview exactly what will be appended before touching the file.
    match aura_config::writer::upsert_mcp_server_in_str("", &collected.name, &collected.server) {
        Ok(preview) => println!("\nThis will be added to the config:\n\n{preview}"),
        Err(e) => {
            return WizardEnd::Failed(format!(
                "could not render the server config: {e}\nNothing was written."
            ));
        }
    }
    if !new_secrets.is_empty() {
        let vars: Vec<&str> = new_secrets.iter().map(|(var, _)| var.as_str()).collect();
        println!(
            "Secrets are stored in `.env` next to the config ({}), not in the TOML.",
            vars.join(", ")
        );
    }
    if !existing_vars.is_empty() {
        println!(
            "References environment variable(s) you already manage ({}) — nothing is written for them.",
            existing_vars.join(", ")
        );
    }
    // A verified (or knowingly unverifiable) server defaults to yes; a
    // server that just failed its check defaults to no.
    let write_prompt = if verified || !unresolved.is_empty() {
        format!("Write to {}? [Y/n] ", config_path.display())
    } else {
        format!(
            "The connection could not be verified — write to {} anyway? [y/N] ",
            config_path.display()
        )
    };
    if !confirm(ctx, &write_prompt) {
        return WizardEnd::Aborted;
    }

    if let Err(e) =
        aura_config::writer::upsert_mcp_server(config_path, &collected.name, &collected.server)
    {
        return WizardEnd::Failed(format!(
            "failed to update the config: {e}\nThe config file was left unchanged."
        ));
    }
    if !new_secrets.is_empty() {
        let env_path = config_path.parent().unwrap_or(Path::new(".")).join(".env");
        if let Err(e) = write_env_secrets(&env_path, &new_secrets) {
            return WizardEnd::Failed(format!(
                "the server was written to the config, but saving secrets to {} failed: {e}\n\
                 Add the variable(s) to that file manually.",
                env_path.display()
            ));
        }
        println!("Saved secrets to {}.", env_path.display());
    }

    if verified {
        offer_worker_access(ctx, config_path, &collected.name, &tool_names);
    } else {
        print_allowlist_hint(config_path, &collected.name);
    }

    WizardEnd::Written {
        name: collected.name,
        starter_prompt: collected.starter_prompt,
        verified,
    }
}

/// Replace each `{{ env.VAR }}` placeholder with its collected secret value.
///
/// The connectivity check needs real credentials, but the freshly written
/// `.env` is only read at process startup (dotenvy), so the placeholders
/// can't resolve through the normal loader this session. The inlined copy
/// exists only in memory for the duration of the check.
fn inline_secrets(server: &McpServerConfig, secrets: &[(String, String)]) -> McpServerConfig {
    let mut server = server.clone();
    let inline = |values: &mut HashMap<String, String>| {
        for value in values.values_mut() {
            for (var, secret) in secrets {
                *value = value.replace(&format!("{{{{ env.{var} }}}}"), secret);
            }
        }
    };
    match &mut server {
        McpServerConfig::HttpStreamable { headers, .. } | McpServerConfig::Sse { headers, .. } => {
            inline(headers);
        }
        McpServerConfig::Stdio { env, .. } => inline(env),
    }
    server
}

/// Throwaway connection + tool-discovery check against just the new server.
///
/// Builds a single-server `McpManager` (entirely separate from the running
/// agent — the new server only joins the real roster on restart), reads the
/// resulting status and tool count, and closes every client — including any
/// spawned stdio child process — before returning.
/// Bounds the whole connect + tool-discovery attempt: the underlying HTTP
/// client has no timeout of its own, so a blackholed endpoint (or npx
/// downloading a stdio server on first run) would otherwise hang the REPL
/// with no Ctrl-C escape.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

fn verify_server(
    rt: &tokio::runtime::Runtime,
    name: &str,
    server: McpServerConfig,
) -> Result<(aura::mcp::ConnectionStatus, Vec<String>), String> {
    let mcp_config = aura_config::config::McpConfig {
        servers: HashMap::from([(name.to_string(), server)]),
        sanitize_schemas: true,
    };
    rt.block_on(async {
        let manager = match tokio::time::timeout(
            VERIFY_TIMEOUT,
            aura::mcp::McpManager::initialize_from_config(&mcp_config),
        )
        .await
        {
            Err(_) => {
                return Err(format!(
                    "timed out after {}s; check the server after restarting",
                    VERIFY_TIMEOUT.as_secs()
                ));
            }
            Ok(Err(e)) => return Err(e.to_string()),
            Ok(Ok(manager)) => manager,
        };
        let result = manager
            .server_info
            .get(name)
            .map(|info| {
                let tools: Vec<String> = manager
                    .streamable_tools
                    .get(name)
                    .into_iter()
                    .chain(manager.sse_tools.get(name))
                    .chain(manager.stdio_tools.get(name))
                    .flatten()
                    .map(|tool| tool.name.to_string())
                    .collect();
                (info.status.clone(), tools)
            })
            .ok_or_else(|| format!("`{name}` missing from the connection status snapshot"));
        manager
            .cancel_and_close_all("mcp-add-verify", "verification complete")
            .await;
        result
    })
}

/// Offer to expose (or scope) a freshly verified server's tools per
/// orchestration worker.
///
/// Two cases, both via the format-preserving `append_worker_mcp_filter`
/// writer:
/// - Workers *without* an `mcp_filter` already see the new server (an
///   omitted filter = every MCP tool; see `resolve_worker_tools` in the
///   orchestrator). They're offered a lockdown choice: scope to this
///   server's tools, assign no MCP tools (`mcp_filter = []`), or keep
///   all-tools access (the default).
/// - Workers with a filter (including the explicit no-tools `[]`) don't
///   see the new server; they're offered a grant appending to their
///   filter.
fn offer_worker_access(
    ctx: &mut CommandContext,
    config_path: &Path,
    server: &str,
    tool_names: &[String],
) {
    let workers = orchestrated_workers(config_path);
    if workers.is_empty() || tool_names.is_empty() {
        return;
    }
    let entries = filter_entries(tool_names);
    let (open, filtered): (Vec<_>, Vec<_>) = workers
        .into_iter()
        .partition(|(_, filter)| filter.is_none());

    if !open.is_empty() {
        let names: Vec<&str> = open.iter().map(|(name, _)| name.as_str()).collect();
        println!(
            "\nWorkers without an mcp_filter receive every MCP tool, so `{server}` \
             is already visible to: {}.",
            names.join(", ")
        );
        println!(
            "You can lock each one down now — a written mcp_filter limits the \
             worker to exactly the listed tools:"
        );
        for (worker, _) in open {
            println!("  `{worker}`:");
            println!(
                "    1. Scope to `{server}`'s tools ({})",
                entries_summary(&entries)
            );
            println!("    2. No MCP tools at all (mcp_filter = [])");
            println!("    3. Keep access to every tool");
            loop {
                let Some(answer) = ask(ctx, "    Choice [1-3, default 3]: ") else {
                    return;
                };
                match answer.as_str() {
                    "1" => append_filter_and_report(config_path, &worker, &entries),
                    "2" => append_filter_and_report(config_path, &worker, &[]),
                    "3" | "" => {}
                    _ => {
                        println!("    Enter 1, 2, or 3.");
                        continue;
                    }
                }
                break;
            }
        }
    }

    if !filtered.is_empty() {
        println!(
            "\nWorkers with an mcp_filter don't see `{server}` yet. \
             Grant access per worker (adds {} to their mcp_filter):",
            entries_summary(&entries)
        );
        for (worker, _) in filtered {
            if confirm(ctx, &format!("  Grant `{worker}` access? [y/N] ")) {
                append_filter_and_report(config_path, &worker, &entries);
            }
        }
    }
}

/// Compact display form of filter entries for prompts; the full list is
/// always what gets written.
fn entries_summary(entries: &[String]) -> String {
    const SHOW: usize = 5;
    if entries.len() <= SHOW {
        entries.join(", ")
    } else {
        format!(
            "{}, ... ({} tools)",
            entries[..SHOW].join(", "),
            entries.len()
        )
    }
}

fn append_filter_and_report(config_path: &Path, worker: &str, entries: &[String]) {
    match aura_config::writer::append_worker_mcp_filter(config_path, worker, entries) {
        Ok(()) => println!("  {} updated `{worker}`", "✓".themed(AuraStyle::Success)),
        Err(e) => println!(
            "  {} failed to update `{worker}`: {e}",
            "error:".themed(AuraStyle::Error)
        ),
    }
}

/// When the new server couldn't be verified (so its tool names are
/// unknown), workers with an mcp_filter still won't see it — say so
/// instead of leaving them silently blind to the new server.
fn print_allowlist_hint(config_path: &Path, server: &str) {
    let filtered: Vec<String> = orchestrated_workers(config_path)
        .into_iter()
        .filter(|(_, filter)| filter.is_some())
        .map(|(name, _)| name)
        .collect();
    if !filtered.is_empty() {
        println!(
            "Workers with an mcp_filter ({}) won't see `{server}`'s tools \
             until matching patterns are added to their filters.",
            filtered.join(", ")
        );
    }
}

/// Read `(worker name, mcp_filter)` pairs from the config file when
/// orchestration is enabled; empty when it isn't (or on any parse problem —
/// this is an optional convenience step, not a gate). `None` = the worker
/// has no `mcp_filter` key (all tools); `Some` mirrors the written array,
/// where empty is the explicit no-tools assignment.
fn orchestrated_workers(config_path: &Path) -> Vec<(String, Option<Vec<String>>)> {
    let Ok(content) = fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    let Some(orchestration) = doc.get("orchestration") else {
        return Vec::new();
    };
    if !orchestration
        .get("enabled")
        .and_then(toml_edit::Item::as_bool)
        .unwrap_or(false)
    {
        return Vec::new();
    }
    let Some(workers) = orchestration
        .get("worker")
        .and_then(toml_edit::Item::as_table_like)
    else {
        return Vec::new();
    };
    let mut result: Vec<(String, Option<Vec<String>>)> = workers
        .iter()
        .map(|(name, worker)| {
            let filter = worker
                .get("mcp_filter")
                .and_then(toml_edit::Item::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                });
            (name.to_string(), filter)
        })
        .collect();
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// Tool-name stems too generic to anchor a glob.
const GENERIC_STEMS: [&str; 10] = [
    "get", "list", "read", "set", "create", "delete", "update", "query", "fetch", "describe",
];

/// Filter entries granting the given tools: a `{prefix}*` glob when every
/// tool shares a namespacing prefix, else the exact names.
///
/// Globs are matched against the tool names of *every* configured server,
/// so a glob is only used when the shared prefix looks like a namespace
/// rather than a verb: it must end at a `_`/`-` separator, be at least 5
/// chars, and its stem must not be a [`GENERIC_STEMS`] verb — `mezmo_*`
/// qualifies, but `get_issue`/`get_pr` fall back to exact names because
/// `get_*` would also grant `get_logs` from an unrelated server.
fn filter_entries(tool_names: &[String]) -> Vec<String> {
    if tool_names.len() > 1 {
        let first = &tool_names[0];
        let mut len = first.len();
        for name in &tool_names[1..] {
            let common = first
                .bytes()
                .zip(name.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            len = len.min(common);
        }
        while len > 0 && !first.is_char_boundary(len) {
            len -= 1;
        }
        if let Some(sep) = first[..len].rfind(['_', '-']) {
            let prefix = &first[..=sep];
            let stem = first[..sep].to_ascii_lowercase();
            if prefix.len() >= 5 && !GENERIC_STEMS.contains(&stem.as_str()) {
                return vec![format!("{prefix}*")];
            }
        }
    }
    tool_names.to_vec()
}

/// Everything the interactive steps produce for the write phase.
struct CollectedServer {
    name: String,
    server: McpServerConfig,
    secrets: Vec<CredentialSource>,
    starter_prompt: Option<&'static str>,
}

/// Where a credential's value comes from.
enum CredentialSource {
    /// Entered during the wizard.
    New { env_var: String, value: String },
    /// An env var the user already exports.
    Existing {
        env_var: String,
        value: Option<String>,
    },
}

impl CredentialSource {
    fn env_var(&self) -> &str {
        match self {
            CredentialSource::New { env_var, .. } | CredentialSource::Existing { env_var, .. } => {
                env_var
            }
        }
    }

    /// The value as known this session (always known for `New`; for
    /// `Existing` only when the var is actually set).
    fn known_value(&self) -> Option<&str> {
        match self {
            CredentialSource::New { value, .. } => Some(value),
            CredentialSource::Existing { value, .. } => value.as_deref(),
        }
    }
}

/// Collect one credential: prefer an env var the user already exports
/// (auto-offered when the conventional var is set) over entering the value
/// now, which is the only path that writes to `.env`.
fn collect_credential(
    ctx: &mut CommandContext,
    default_env_var: &str,
    secret_prompt: &str,
) -> Option<CredentialSource> {
    if let Some(value) = set_env_value(default_env_var)
        && confirm(
            ctx,
            &format!("{default_env_var} is already set in your environment — use it? [Y/n] "),
        )
    {
        return Some(CredentialSource::Existing {
            env_var: default_env_var.to_string(),
            value: Some(value),
        });
    }
    println!("How do you want to provide the {secret_prompt}?");
    println!("  1. Enter it now (saved to .env next to the config)");
    println!("  2. Reference an environment variable you already export");
    loop {
        let answer = ask(ctx, "Choice [1-2]: ")?;
        match answer.as_str() {
            "1" => {
                let value = ask_secret(&format!("{secret_prompt}: "))?;
                // At startup the shell's export wins over .env (dotenvy
                // never overwrites existing env), so a value entered here
                // is dead weight while the export exists.
                if set_env_value(default_env_var).is_some() {
                    println!(
                        "note: {default_env_var} is exported in your shell, and exported \
                         values override .env at startup — unset or update the export \
                         for this value to take effect."
                    );
                }
                return Some(CredentialSource::New {
                    env_var: default_env_var.to_string(),
                    value,
                });
            }
            "2" => {
                let env_var = loop {
                    let name = ask_with_default(ctx, "Environment variable", default_env_var)?;
                    if is_valid_env_var(&name) {
                        break name;
                    }
                    println!(
                        "Variable names contain letters, digits, and `_`, and don't \
                         start with a digit."
                    );
                };
                let value = set_env_value(&env_var);
                if value.is_none() {
                    println!(
                        "note: {env_var} is not set in this session, so the connection \
                         check will be skipped."
                    );
                }
                return Some(CredentialSource::Existing { env_var, value });
            }
            _ => println!("Enter 1 or 2."),
        }
    }
}

fn set_env_value(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.trim().is_empty())
}

fn collect_catalog(
    ctx: &mut CommandContext,
    entry: &'static CatalogEntry,
    config_path: &Path,
) -> Option<CollectedServer> {
    println!("\n{}", entry.prerequisites.themed(AuraStyle::Muted));
    if !confirm(ctx, "Continue? [Y/n] ") {
        return None;
    }

    let name = ask_server_name(ctx, Some(entry.key), config_path)?;

    let (server, secrets) = match &entry.template {
        Template::Http { url, headers } => {
            let mut header_map = HashMap::new();
            let mut secrets = Vec::new();
            for template in *headers {
                let source = collect_credential(ctx, template.env_var, template.secret_prompt)?;
                // The template names the conventional var; rewrite its
                // placeholder when the user picked a different one.
                let value = template.value_template.replace(
                    &format!("{{{{ env.{} }}}}", template.env_var),
                    &format!("{{{{ env.{} }}}}", source.env_var()),
                );
                header_map.insert(template.header.to_string(), value);
                secrets.push(source);
            }
            (
                http_streamable(url.to_string(), header_map, entry.description),
                secrets,
            )
        }
        Template::Stdio { cmd, args } => (
            McpServerConfig::Stdio {
                cmd: cmd.iter().map(|s| s.to_string()).collect(),
                args: args.iter().map(|s| s.to_string()).collect(),
                env: HashMap::new(),
                description: Some(entry.description.to_string()),
                scratchpad: HashMap::new(),
            },
            Vec::new(),
        ),
    };

    Some(CollectedServer {
        name,
        server,
        secrets,
        starter_prompt: Some(entry.starter_prompt),
    })
}

fn collect_custom(ctx: &mut CommandContext, config_path: &Path) -> Option<CollectedServer> {
    let name = ask_server_name(ctx, None, config_path)?;

    println!("Transport:");
    println!("  1. http_streamable (HTTP endpoint)");
    println!("  2. sse (Server-Sent Events endpoint)");
    println!("  3. stdio (local command)");
    let transport = loop {
        let answer = ask(ctx, "Transport [1-3]: ")?;
        match answer.as_str() {
            "1" | "2" | "3" => break answer,
            _ => println!("Enter 1, 2, or 3."),
        }
    };

    let (server, secrets) = if transport == "3" {
        let (cmd, args) = loop {
            let line = ask(ctx, "Command to run (e.g. npx -y some-mcp-server): ")?;
            match parse_command_line(&line) {
                Some(parsed) => break parsed,
                None => println!("Enter a non-empty command."),
            }
        };
        (
            McpServerConfig::Stdio {
                cmd,
                args,
                env: HashMap::new(),
                description: None,
                scratchpad: HashMap::new(),
            },
            Vec::new(),
        )
    } else {
        let url = loop {
            let url = ask(ctx, "Server URL: ")?;
            if url.starts_with("http://") || url.starts_with("https://") {
                break url;
            }
            println!("Enter an http:// or https:// URL.");
        };
        let mut headers = HashMap::new();
        let mut secrets = Vec::new();
        if confirm(ctx, "Does the server need an auth header? [y/N] ")
            && let Some(header) = ask_with_default(ctx, "Header name", "Authorization")
        {
            println!("How is the `{header}` value formed?");
            println!("  1. Bearer <secret>");
            println!("  2. Token <secret>");
            println!("  3. <secret> only (no prefix)");
            println!("  4. Custom prefix");
            let prefix = loop {
                let answer = ask(ctx, "Choice [1-4]: ")?;
                match answer.as_str() {
                    "1" => break "Bearer ".to_string(),
                    "2" => break "Token ".to_string(),
                    "3" => break String::new(),
                    "4" => {
                        // `ask` trims, so a separating space can't be typed —
                        // add it for any non-empty prefix.
                        let custom = ask(ctx, "Prefix: ")?;
                        break if custom.is_empty() {
                            custom
                        } else {
                            format!("{custom} ")
                        };
                    }
                    _ => println!("Enter a number between 1 and 4."),
                }
            };
            let secret_prompt = if prefix.is_empty() {
                format!("`{header}` secret")
            } else {
                format!(
                    "`{header}` secret (without the `{}` prefix)",
                    prefix.trim_end()
                )
            };
            let source = collect_credential(ctx, &derive_env_var(&name, &header), &secret_prompt)?;
            headers.insert(
                header,
                format!("{prefix}{{{{ env.{} }}}}", source.env_var()),
            );
            secrets.push(source);
        }
        let server = if transport == "1" {
            http_streamable(url, headers, "")
        } else {
            McpServerConfig::Sse {
                url,
                headers,
                description: None,
                headers_from_request: HashMap::new(),
                scratchpad: HashMap::new(),
            }
        };
        (server, secrets)
    };

    Some(CollectedServer {
        name,
        server,
        secrets,
        starter_prompt: None,
    })
}

fn http_streamable(
    url: String,
    headers: HashMap<String, String>,
    description: &str,
) -> McpServerConfig {
    McpServerConfig::HttpStreamable {
        url,
        headers,
        description: (!description.is_empty()).then(|| description.to_string()),
        headers_from_request: HashMap::new(),
        scratchpad: HashMap::new(),
    }
}

/// Ask for (or default) the server name, rejecting invalid names and
/// confirming before an existing `[mcp.servers.<name>]` is overwritten.
fn ask_server_name(
    ctx: &mut CommandContext,
    default: Option<&str>,
    config_path: &Path,
) -> Option<String> {
    let name = loop {
        let name = match default {
            Some(default) => ask_with_default(ctx, "Server name", default)?,
            None => ask(ctx, "Server name: ")?,
        };
        if is_valid_server_name(&name) {
            break name;
        }
        println!("Server names may contain letters, digits, `-`, and `_` only.");
    };
    if config_has_server(config_path, &name)
        && !confirm(
            ctx,
            &format!("`{name}` is already configured. Overwrite? [y/N] "),
        )
    {
        return None;
    }
    Some(name)
}

/// Best-effort check whether `[mcp.servers.<name>]` already exists; a
/// malformed config reads as "absent" here and fails properly at write time.
fn config_has_server(config_path: &Path, name: &str) -> bool {
    let Ok(content) = fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc.get("mcp")
        .and_then(|mcp| mcp.get("servers"))
        .and_then(|servers| servers.get(name))
        .is_some()
}

/// Merge `(env var, value)` pairs into the `.env` file, creating it (with
/// the do-not-commit header and `0o600` on unix) when absent. Values of
/// existing keys are replaced; unrelated lines are preserved. Values are
/// quoted as needed so dotenvy can parse them back — an unquoted space or
/// `#` doesn't just corrupt that entry, it aborts dotenvy's parse and
/// silently drops every line after it.
fn write_env_secrets(env_path: &Path, secrets: &[(String, String)]) -> std::io::Result<()> {
    let mut content = match fs::read_to_string(env_path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let creating = content.is_empty();
    for (var, value) in secrets {
        let value = quote_env_value(value);
        if content.is_empty() {
            content = crate::init::render_env(var, &value);
        } else {
            content = crate::init::merge_env(&content, var, &value);
        }
    }
    // On unix, apply 0600 at creation rather than after the write so the
    // secrets are never on disk in a umask-default readable file.
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        if creating {
            options.mode(0o600);
        }
        options.open(env_path)?.write_all(content.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        let _ = creating;
        fs::write(env_path, content)?;
    }
    Ok(())
}

/// Quote a dotenv value when it contains characters dotenvy would misparse
/// unquoted (whitespace, `#`, quotes, backslash). Plain token-like values
/// stay unquoted; single quotes are preferred because dotenvy treats their
/// contents as fully literal.
fn quote_env_value(value: &str) -> String {
    let plain = value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        value.to_string()
    } else if !value.contains('\'') {
        format!("'{value}'")
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// `GRAFANA` + `Authorization` → `GRAFANA_AUTHORIZATION`: uppercase, with
/// every non-alphanumeric run collapsed to a single `_`.
fn derive_env_var(server: &str, header: &str) -> String {
    let mut out = String::new();
    for part in [server, header] {
        for c in part.chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c.to_ascii_uppercase());
            } else if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
        }
        if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}

fn is_valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_valid_env_var(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whitespace-split a command line into `cmd = [first]` + `args = rest`.
/// No shell quoting — the wizard says so in the prompt example.
fn parse_command_line(line: &str) -> Option<(Vec<String>, Vec<String>)> {
    let mut tokens = line.split_whitespace().map(str::to_string);
    let first = tokens.next()?;
    Some((vec![first], tokens.collect()))
}

/// One line of wizard input via the REPL's rustyline editor (which owns the
/// terminal between dispatches). `None` on Ctrl-C/Ctrl-D — treat as abort.
fn ask(ctx: &mut CommandContext, prompt: &str) -> Option<String> {
    ctx.input_reader
        .readline(prompt)
        .ok()
        .map(|line| line.trim().to_string())
}

fn ask_with_default(ctx: &mut CommandContext, prompt: &str, default: &str) -> Option<String> {
    let answer = ask(ctx, &format!("{prompt} [{default}]: "))?;
    Some(if answer.is_empty() {
        default.to_string()
    } else {
        answer
    })
}

/// Y/n confirmation; the capitalized letter in the caller's prompt is the
/// default taken on plain Enter.
fn confirm(ctx: &mut CommandContext, prompt: &str) -> bool {
    let default_yes = prompt.contains("[Y/n]");
    match ask(ctx, prompt) {
        Some(answer) if answer.is_empty() => default_yes,
        Some(answer) => matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"),
        None => false,
    }
}

/// Masked credential input straight from the tty (never echoed, never in
/// rustyline history). Empty input gets one re-prompt (rpassword runs in
/// cooked mode where Ctrl-C doesn't interrupt, so Enter-on-empty is the
/// documented escape); `None` aborts the wizard.
///
/// Clears [`SIGINT_RECEIVED`](crate::ui::state::SIGINT_RECEIVED) before
/// returning: a Ctrl-C pressed during the masked read only sets the flag,
/// and left set it would count as a phantom first quit-press on the next
/// streaming turn.
fn ask_secret(prompt: &str) -> Option<String> {
    let mut result = None;
    for attempt in 0..2 {
        match rpassword::prompt_password(prompt).ok() {
            None => break,
            Some(value) => {
                let value = value.trim().to_string();
                if !value.is_empty() {
                    result = Some(value);
                    break;
                }
                if attempt == 0 {
                    println!("Empty input — enter the value, or press Enter again to abort.");
                }
            }
        }
    }
    crate::ui::state::SIGINT_RECEIVED.store(false, std::sync::atomic::Ordering::Relaxed);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_env_var_names() {
        assert_eq!(
            derive_env_var("grafana", "Authorization"),
            "GRAFANA_AUTHORIZATION"
        );
        assert_eq!(
            derive_env_var("my-server", "X-Api-Key"),
            "MY_SERVER_X_API_KEY"
        );
    }

    #[test]
    fn validates_env_var_names() {
        assert!(is_valid_env_var("MEZMO_API_KEY"));
        assert!(is_valid_env_var("_PRIVATE"));
        assert!(!is_valid_env_var(""));
        assert!(!is_valid_env_var("1BAD"));
        assert!(!is_valid_env_var("HAS-DASH"));
    }

    #[test]
    fn credential_source_reports_env_var_and_value() {
        let new = CredentialSource::New {
            env_var: "A".to_string(),
            value: "v".to_string(),
        };
        assert_eq!(new.env_var(), "A");
        assert_eq!(new.known_value(), Some("v"));
        let set = CredentialSource::Existing {
            env_var: "B".to_string(),
            value: Some("w".to_string()),
        };
        assert_eq!(set.known_value(), Some("w"));
        let unset = CredentialSource::Existing {
            env_var: "C".to_string(),
            value: None,
        };
        assert_eq!(unset.known_value(), None);
    }

    #[test]
    fn validates_server_names() {
        assert!(is_valid_server_name("mezmo"));
        assert!(is_valid_server_name("my_server-2"));
        assert!(!is_valid_server_name(""));
        assert!(!is_valid_server_name("has space"));
        assert!(!is_valid_server_name("dotted.name"));
    }

    #[test]
    fn splits_command_lines() {
        assert_eq!(
            parse_command_line("npx -y some-mcp"),
            Some((
                vec!["npx".to_string()],
                vec!["-y".to_string(), "some-mcp".to_string()]
            ))
        );
        assert_eq!(parse_command_line("   "), None);
    }

    #[test]
    fn catalog_templates_render_and_reload() {
        for entry in CATALOG {
            let (server, _secrets) = match &entry.template {
                Template::Http { url, headers } => {
                    let header_map = headers
                        .iter()
                        .map(|t| (t.header.to_string(), t.value_template.to_string()))
                        .collect();
                    (
                        http_streamable(url.to_string(), header_map, entry.description),
                        (),
                    )
                }
                Template::Stdio { cmd, args } => (
                    McpServerConfig::Stdio {
                        cmd: cmd.iter().map(|s| s.to_string()).collect(),
                        args: args.iter().map(|s| s.to_string()).collect(),
                        env: HashMap::new(),
                        description: Some(entry.description.to_string()),
                        scratchpad: HashMap::new(),
                    },
                    (),
                ),
            };
            let rendered =
                aura_config::writer::upsert_mcp_server_in_str("", entry.key, &server).unwrap();
            assert!(
                rendered.contains(&format!("[mcp.servers.{}]", entry.key)),
                "{rendered}"
            );
            // Placeholders must land verbatim so secrets stay in .env.
            if let Template::Http { headers, .. } = &entry.template {
                for template in *headers {
                    assert!(
                        rendered.contains(&format!("{{{{ env.{} }}}}", template.env_var)),
                        "missing placeholder for {} in:\n{rendered}",
                        template.env_var
                    );
                }
            }
        }
    }

    #[test]
    fn env_secrets_create_and_merge() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        write_env_secrets(
            &env_path,
            &[("MEZMO_API_KEY".to_string(), "secret1".to_string())],
        )
        .unwrap();
        let first = fs::read_to_string(&env_path).unwrap();
        assert!(first.contains("MEZMO_API_KEY=secret1"), "{first}");

        write_env_secrets(
            &env_path,
            &[
                ("MEZMO_API_KEY".to_string(), "rotated".to_string()),
                ("DD_API_KEY".to_string(), "secret2".to_string()),
            ],
        )
        .unwrap();
        let merged = fs::read_to_string(&env_path).unwrap();
        assert!(merged.contains("MEZMO_API_KEY=rotated"), "{merged}");
        assert!(!merged.contains("secret1"), "{merged}");
        assert!(merged.contains("DD_API_KEY=secret2"), "{merged}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&env_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn env_values_with_spaces_survive_dotenv_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join(".env");
        write_env_secrets(
            &env_path,
            &[
                ("AUTH_HEADER".to_string(), "Bearer abc def#ghi".to_string()),
                ("PLAIN_KEY".to_string(), "sk-plain-123".to_string()),
            ],
        )
        .unwrap();
        // An unquoted space would abort dotenvy's parse here and drop every
        // later line, so parse the whole file back and check both keys.
        let parsed: HashMap<String, String> = dotenvy::from_path_iter(&env_path)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            parsed.get("AUTH_HEADER").map(String::as_str),
            Some("Bearer abc def#ghi")
        );
        assert_eq!(
            parsed.get("PLAIN_KEY").map(String::as_str),
            Some("sk-plain-123")
        );
    }

    #[test]
    fn quotes_env_values_only_when_needed() {
        assert_eq!(quote_env_value("sk-plain_123"), "sk-plain_123");
        assert_eq!(quote_env_value("Bearer abc"), "'Bearer abc'");
        assert_eq!(quote_env_value("with#hash"), "'with#hash'");
        assert_eq!(quote_env_value("it's"), "\"it's\"");
        assert_eq!(quote_env_value("a\"b"), "'a\"b'");
    }

    #[test]
    fn inlines_secrets_for_verification_without_touching_original() {
        let server = http_streamable(
            "https://mcp.mezmo.com/mcp".to_string(),
            HashMap::from([(
                "Authorization".to_string(),
                "Bearer {{ env.MEZMO_API_KEY }}".to_string(),
            )]),
            "",
        );
        let inlined = inline_secrets(
            &server,
            &[("MEZMO_API_KEY".to_string(), "sk-live-123".to_string())],
        );
        let McpServerConfig::HttpStreamable { headers, .. } = &inlined else {
            panic!("transport changed");
        };
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer sk-live-123")
        );
        // The original (destined for the config file) keeps the placeholder.
        let McpServerConfig::HttpStreamable { headers, .. } = &server else {
            panic!("transport changed");
        };
        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer {{ env.MEZMO_API_KEY }}")
        );
    }

    #[test]
    fn filter_entries_prefer_namespace_prefix_glob() {
        let tools = vec![
            "mezmo_logs".to_string(),
            "mezmo_pipelines".to_string(),
            "mezmo_export".to_string(),
        ];
        assert_eq!(filter_entries(&tools), ["mezmo_*"]);

        // A generic verb stem must not become a glob: `get_*` would also
        // match other servers' tools.
        let generic = vec![
            "get_issue".to_string(),
            "get_pr".to_string(),
            "get_file".to_string(),
        ];
        assert_eq!(filter_entries(&generic), generic);

        // Too-short namespace falls back to exact names.
        let short = vec!["dd_metrics".to_string(), "dd_monitors".to_string()];
        assert_eq!(filter_entries(&short), short);

        let mixed = vec!["ListKnowledgeBases".to_string(), "QueryBases".to_string()];
        assert_eq!(filter_entries(&mixed), mixed);

        let single = vec!["only_tool".to_string()];
        assert_eq!(filter_entries(&single), single);
    }

    #[test]
    fn entries_summary_truncates_long_lists() {
        let short: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(entries_summary(&short), "a, b");
        let long: Vec<String> = (0..8).map(|i| format!("tool_{i}")).collect();
        assert_eq!(
            entries_summary(&long),
            "tool_0, tool_1, tool_2, tool_3, tool_4, ... (8 tools)"
        );
    }

    #[test]
    fn reads_orchestrated_workers_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let base = "[agent]\nname = \"a\"\n\n[orchestration]\nenabled = %E%\n\n\
                    [orchestration.worker.ops]\ndescription = \"d\"\npreamble = \"p\"\n\
                    mcp_filter = [\"logs_*\"]\n\n\
                    [orchestration.worker.silent]\ndescription = \"d\"\npreamble = \"p\"\n\
                    mcp_filter = []\n\n\
                    [orchestration.worker.writer]\ndescription = \"d\"\npreamble = \"p\"\n";

        fs::write(&path, base.replace("%E%", "true")).unwrap();
        let workers = orchestrated_workers(&path);
        // `mcp_filter = []` must stay distinct from an omitted filter — the
        // lockdown/grant partition hinges on it.
        assert_eq!(
            workers,
            [
                ("ops".to_string(), Some(vec!["logs_*".to_string()])),
                ("silent".to_string(), Some(vec![])),
                ("writer".to_string(), None),
            ]
        );

        fs::write(&path, base.replace("%E%", "false")).unwrap();
        assert!(orchestrated_workers(&path).is_empty());
    }

    #[test]
    fn detects_existing_server_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[agent]\nname = \"a\"\n\n[mcp.servers.mezmo]\ntransport = \"sse\"\nurl = \"https://x\"\n",
        )
        .unwrap();
        assert!(config_has_server(&path, "mezmo"));
        assert!(!config_has_server(&path, "datadog"));
        assert!(!config_has_server(
            &dir.path().join("missing.toml"),
            "mezmo"
        ));
    }
}
