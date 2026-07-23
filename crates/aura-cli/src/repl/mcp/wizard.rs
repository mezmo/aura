//! Guided `/mcp add` flow: pick a catalog server (or define a custom one),
//! collect credentials with masked input, and write the result to the
//! standalone config via `aura_config::writer` plus `.env` for secrets.
//!
//! Standalone mode only — HTTP mode's config lives on the remote server.
//! The flow is deliberately deterministic: no LLM ever sees a credential.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use aura_config::config::McpServerConfig;

use super::catalog::{CATALOG, CatalogEntry, Template};
use crate::backend::Backend;
use crate::repl::registry::CommandContext;
use crate::theme::{AuraStyle, Themed};

/// The wizard's terminal states, folded into a user-facing message by [`run`].
enum WizardEnd {
    Written {
        name: String,
        starter_prompt: Option<&'static str>,
    },
    Aborted,
    Failed(String),
}

pub(super) fn run(ctx: &mut CommandContext) {
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
        } => {
            println!(
                "\n{} `{name}` added to {}.",
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
        }
        WizardEnd::Aborted => println!("\n/mcp add aborted — nothing was written."),
        WizardEnd::Failed(reason) => {
            println!(
                "\n{} {reason}\nNothing was partially applied to the config.",
                "error:".themed(AuraStyle::Error)
            );
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

    // Preview exactly what will be appended before touching the file.
    match aura_config::writer::upsert_mcp_server_in_str("", &collected.name, &collected.server) {
        Ok(preview) => println!("\nThis will be added to the config:\n\n{preview}"),
        Err(e) => return WizardEnd::Failed(format!("could not render the server config: {e}")),
    }
    if !collected.secrets.is_empty() {
        let vars: Vec<&str> = collected
            .secrets
            .iter()
            .map(|(var, _)| var.as_str())
            .collect();
        println!(
            "Secrets are stored in `.env` next to the config ({}), not in the TOML.",
            vars.join(", ")
        );
    }
    if !confirm(ctx, &format!("Write to {}? [Y/n] ", config_path.display())) {
        return WizardEnd::Aborted;
    }

    if let Err(e) =
        aura_config::writer::upsert_mcp_server(config_path, &collected.name, &collected.server)
    {
        return WizardEnd::Failed(format!("failed to update the config: {e}"));
    }
    if !collected.secrets.is_empty() {
        let env_path = config_path.parent().unwrap_or(Path::new(".")).join(".env");
        if let Err(e) = write_env_secrets(&env_path, &collected.secrets) {
            return WizardEnd::Failed(format!(
                "the server was written to the config, but saving secrets to {} failed: {e}\n\
                 Add the variable(s) to that file manually.",
                env_path.display()
            ));
        }
        println!("Saved secrets to {}.", env_path.display());
    }

    WizardEnd::Written {
        name: collected.name,
        starter_prompt: collected.starter_prompt,
    }
}

/// Everything the interactive steps produce for the write phase.
struct CollectedServer {
    name: String,
    server: McpServerConfig,
    /// `(env var, secret value)` pairs destined for `.env`.
    secrets: Vec<(String, String)>,
    starter_prompt: Option<&'static str>,
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
                let value = ask_secret(&format!("{}: ", template.secret_prompt))?;
                header_map.insert(
                    template.header.to_string(),
                    template.value_template.to_string(),
                );
                secrets.push((template.env_var.to_string(), value));
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
            let env_var = derive_env_var(&name, &header);
            let value = ask_secret(&format!(
                "Value for `{header}` (full value, e.g. `Bearer <token>`): "
            ))?;
            headers.insert(header, format!("{{{{ env.{env_var} }}}}"));
            secrets.push((env_var, value));
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
/// existing keys are replaced; unrelated lines are preserved.
fn write_env_secrets(env_path: &Path, secrets: &[(String, String)]) -> std::io::Result<()> {
    let mut content = match fs::read_to_string(env_path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let creating = content.is_empty();
    for (var, value) in secrets {
        if content.is_empty() {
            content = crate::init::render_env(var, value);
        } else {
            content = crate::init::merge_env(&content, var, value);
        }
    }
    fs::write(env_path, content)?;
    #[cfg(unix)]
    if creating {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(env_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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
/// rustyline history). `None` on error or empty input — treat as abort.
fn ask_secret(prompt: &str) -> Option<String> {
    let value = rpassword::prompt_password(prompt).ok()?;
    let value = value.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
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
