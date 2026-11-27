//! The `aura-web-server` compatibility shim.

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

fn shim(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aura-web-server"))
        .args(args)
        .env_clear()
        .envs(passthrough_env())
        .output()
        .expect("failed to run the aura-web-server binary")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn warns_and_names_the_replacement_command() {
    let output = shim(&["--version"]);
    assert!(output.status.success());
    let warning = stderr(&output);
    assert!(warning.contains("deprecated"), "no warning:\n{warning}");
    assert!(
        warning.contains("aura webserver"),
        "warning omits the replacement command:\n{warning}"
    );
}

#[test]
fn warning_does_not_pollute_stdout() {
    let output = shim(&["--version"]);
    let version = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        version.trim(),
        format!("aura-web-server {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn accepts_the_server_options() {
    let output = shim(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    for flag in ["--host", "--port", "--tool-result-mode"] {
        assert!(help.contains(flag), "{flag} missing from:\n{help}");
    }
}

#[test]
fn delegates_to_the_shared_startup_path() {
    let output = shim(&["--config", "definitely-missing.toml"]);
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr(&output)
    );
    assert!(
        combined.contains("definitely-missing.toml"),
        "expected a config error naming the file:\n{combined}"
    );
}
