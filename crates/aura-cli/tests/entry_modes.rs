//! Both modes of the `aura` binary, driven through the real executable.

#![cfg(feature = "webserver")]

use std::process::{Command, Output};

/// Loader and process basics only; everything AURA reads stays cleared.
fn passthrough_env() -> Vec<(String, String)> {
    [
        "PATH",
        "HOME",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "TMPDIR",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
    .collect()
}

fn aura(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aura"))
        .args(args)
        .env_clear()
        .envs(passthrough_env())
        .output()
        .expect("failed to run the aura binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn cli_help_lists_webserver_as_a_command() {
    let output = aura(&["--help"]);
    assert!(output.status.success());
    let help = stdout(&output);
    let commands = help
        .split("Commands:")
        .nth(1)
        .unwrap_or_default()
        .split("Options:")
        .next()
        .unwrap_or_default();
    assert!(
        commands.contains("webserver"),
        "webserver missing from the Commands section:\n{help}"
    );
}

#[test]
fn webserver_help_lists_server_options() {
    let output = aura(&["webserver", "--help"]);
    assert!(output.status.success());
    let help = stdout(&output);
    for flag in [
        "--host",
        "--port",
        "--tool-result-mode",
        "--shutdown-timeout-secs",
    ] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
}

#[test]
fn webserver_help_shows_the_mode_invocation() {
    let help = stdout(&aura(&["webserver", "--help"]));
    assert!(
        help.contains("Usage: aura webserver"),
        "unexpected usage line:\n{help}"
    );
}

#[test]
fn both_modes_report_the_same_version() {
    let cli = aura(&["--version"]);
    let server = aura(&["webserver", "--version"]);
    assert!(cli.status.success() && server.status.success());
    assert_eq!(stdout(&cli), stdout(&server));
    assert_eq!(
        stdout(&cli).trim(),
        format!("aura {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn webserver_mode_rejects_cli_only_options() {
    let output = aura(&["webserver", "--api-url", "http://localhost:8080"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--api-url"),
        "expected an argument error naming --api-url:\n{stderr}"
    );
}

/// Proves the mode reaches server startup, not just argument parsing.
#[test]
fn webserver_mode_loads_the_requested_config() {
    let output = aura(&["webserver", "--config", "definitely-missing.toml"]);
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        stdout(&output),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("definitely-missing.toml"),
        "expected a config error naming the file:\n{combined}"
    );
}
