use std::process::Command;

fn aura_cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aura-cli"))
}

#[test]
fn help_flag_exits_zero() {
    let output = aura_cli().arg("--help").output().unwrap();
    assert!(output.status.success(), "expected exit 0 for --help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("aura-cli"),
        "help should mention the binary name"
    );
    assert!(
        stdout.contains("--api-url"),
        "help should mention --api-url"
    );
    assert!(stdout.contains("--query"), "help should mention --query");
    assert!(stdout.contains("--force"), "help should mention --force");
}

#[test]
fn version_flag_exits_zero() {
    let output = aura_cli().arg("--version").output().unwrap();
    assert!(output.status.success(), "expected exit 0 for --version");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("aura-cli"),
        "version output should mention the binary name"
    );
}

#[test]
fn unknown_flag_fails() {
    let output = aura_cli().arg("--does-not-exist").output().unwrap();
    assert!(!output.status.success(), "unknown flag should fail");
}

#[test]
fn query_without_api_produces_error() {
    // Connect to a port that's almost certainly not running an API
    let output = aura_cli()
        .arg("--api-url")
        .arg("http://127.0.0.1:19999")
        .arg("--query")
        .arg("hello")
        .output()
        .unwrap();
    // Should exit non-zero because it can't connect
    assert!(
        !output.status.success(),
        "query to unreachable API should fail"
    );
}

#[cfg(feature = "standalone-cli")]
#[test]
fn help_includes_standalone_and_config_flags() {
    let output = aura_cli().arg("--help").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--config"),
        "help should mention --config when built with standalone-cli feature"
    );
    assert!(
        stdout.contains("--standalone"),
        "help should mention --standalone when built with standalone-cli feature"
    );
}

#[cfg(feature = "standalone-cli")]
#[test]
fn standalone_without_config_exits_with_error() {
    let output = aura_cli().arg("--standalone").output().unwrap();
    assert!(
        !output.status.success(),
        "--standalone without --config should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--standalone requires --config"),
        "should explain that --config is required"
    );
}

#[cfg(feature = "standalone-cli")]
#[test]
fn config_without_standalone_exits_with_error() {
    let output = aura_cli()
        .arg("--config")
        .arg("some/path.toml")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "--config without --standalone should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--config requires --standalone"),
        "should explain that --standalone is required"
    );
}

#[cfg(not(feature = "standalone-cli"))]
#[test]
fn standalone_flag_without_feature_exits_with_error() {
    // When standalone-cli feature is NOT enabled, --standalone should be caught pre-parse
    let output = aura_cli().arg("--standalone").output().unwrap();
    assert!(
        !output.status.success(),
        "--standalone without feature should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("standalone-cli feature"),
        "should mention the standalone-cli feature"
    );
}

#[cfg(not(feature = "standalone-cli"))]
#[test]
fn config_flag_without_feature_exits_with_error() {
    let output = aura_cli()
        .arg("--config")
        .arg("some/path.toml")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "--config without feature should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("standalone-cli feature"),
        "should mention the standalone-cli feature"
    );
}
