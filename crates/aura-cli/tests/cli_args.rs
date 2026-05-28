use std::process::Command;

/// Spawn the CLI binary with telemetry kill-switched off.
///
/// `CARGO_BIN_EXE_aura-cli` starts a fresh subprocess that does not
/// reliably inherit the `CARGO_TARGET_TMPDIR` / `RUST_TEST_THREADS`
/// markers `decide_disabled` checks, so without this guard a black-box
/// test run would create `~/.aura/install-id`, append rows to the
/// developer's real `~/.aura/telemetry/events.jsonl`, and attempt
/// delivery to whatever endpoint defaults to (the OSS build's empty
/// API key still triggers a 401 attempt). The explicit
/// `AURA_TELEMETRY_DISABLED=1` is the canonical kill switch and
/// guarantees the spawned binary stays out of the user's filesystem
/// and off the network.
fn aura_cli() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aura-cli"));
    cmd.env("AURA_TELEMETRY_DISABLED", "1");
    cmd
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

#[test]
fn oneshot_error_leaves_stdout_empty() {
    // The one-shot output contract is: stdout is *only* the assistant
    // response, never errors, prompts, or markers. When the request
    // fails (here: connection refused), the error must land on stderr
    // and stdout must be empty so a downstream pipe doesn't ingest
    // garbage. A non-empty stdout here would also catch regressions
    // where someone re-adds the old `● Error` decoration.
    let output = aura_cli()
        .arg("--api-url")
        .arg("http://127.0.0.1:19999")
        .arg("--query")
        .arg("hello")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.is_empty(),
        "stdout must be empty on one-shot error; got: {stdout:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("error:"),
        "stderr should carry the error message; got: {stderr:?}"
    );
    // Specifically guard against re-introducing the legacy `●` bullet
    // marker on the error path — it used to be on stdout, which broke
    // pipes; even on stderr it would signal the formatting regressed.
    assert!(
        !stdout.contains('●') && !stderr.contains('●'),
        "no bullet markers should appear in one-shot output; \
         stdout={stdout:?} stderr={stderr:?}",
    );
}

#[test]
fn help_includes_log_file_flag() {
    let output = aura_cli().arg("--help").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--log-file"),
        "help should mention --log-file"
    );
    assert!(
        stdout.contains("AURA_LOG_FILE"),
        "help should advertise the AURA_LOG_FILE env binding"
    );
    assert!(
        stdout.contains("rotation"),
        "help should warn that log rotation is the user's responsibility"
    );
}

#[test]
fn log_file_creates_and_writes_file() {
    // Drive the CLI against an unreachable API so it exits quickly; what we
    // care about is that `--log-file <path>` materialized into a file on
    // disk and that the global subscriber actually wrote to it. We elevate
    // `RUST_LOG=trace` so hyper/reqwest emit enough events to populate the
    // file even on a connection-refused error path.
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("cli.log");

    let output = aura_cli()
        .env("RUST_LOG", "trace")
        .arg("--log-file")
        .arg(&log_path)
        .arg("--api-url")
        .arg("http://127.0.0.1:19999")
        .arg("--query")
        .arg("hello")
        .output()
        .unwrap();

    assert!(
        log_path.exists(),
        "expected --log-file path to be created; stderr was: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let contents = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        !contents.is_empty(),
        "expected log file to receive tracing events when --log-file is set + RUST_LOG=trace"
    );
}

#[test]
fn log_file_appends_across_invocations() {
    // The CLI documents `--log-file` as append-only; confirm two invocations
    // grow the file rather than truncating it. Without this guarantee, users
    // who pipe `cli.log` to a real log forwarder could silently lose
    // history on the next run.
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("cli.log");

    for _ in 0..2 {
        let _ = aura_cli()
            .env("RUST_LOG", "trace")
            .arg("--log-file")
            .arg(&log_path)
            .arg("--api-url")
            .arg("http://127.0.0.1:19999")
            .arg("--query")
            .arg("hello")
            .output()
            .unwrap();
    }

    let bytes_after_two = std::fs::metadata(&log_path).unwrap().len();
    let one_run_lower_bound = 50; // smallest plausible single-run trace size
    assert!(
        bytes_after_two > 2 * one_run_lower_bound,
        "log file should grow across invocations (append mode); size={bytes_after_two}",
    );
}

#[test]
fn aura_log_file_env_is_picked_up() {
    // The clap arg declares `env = "AURA_LOG_FILE"`, so the env var should
    // be equivalent to passing `--log-file`. We can't reach into the live
    // subscriber to verify this directly, but we can confirm the binary
    // wrote the same file it would have via the flag.
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("cli.log");

    let _ = aura_cli()
        .env("RUST_LOG", "trace")
        .env("AURA_LOG_FILE", &log_path)
        .arg("--api-url")
        .arg("http://127.0.0.1:19999")
        .arg("--query")
        .arg("hello")
        .output()
        .unwrap();

    assert!(
        log_path.exists() && std::fs::metadata(&log_path).unwrap().len() > 0,
        "AURA_LOG_FILE env var should drive log emission like --log-file"
    );
}

#[cfg(feature = "standalone-cli")]
#[test]
fn standalone_otel_init_does_not_panic_without_collector() {
    // Regression test: before the runtime-hoisting refactor, calling
    // `init_otel_provider` from `main` panicked with
    // "there is no reactor running, must be called from the context of a
    // Tokio 1.x runtime" because the OTLP gRPC exporter's `with_tonic()`
    // build path calls `Handle::current()` during construction. The fix
    // hoists the tokio runtime up to `main` and enters its context
    // before calling `logging::init`.
    //
    // We can't easily assert "OTel is wired up correctly" from a black-
    // box test (no collector), but we *can* assert that pointing
    // `OTEL_EXPORTER_OTLP_ENDPOINT` at a refused port no longer trips
    // the panic — the CLI must proceed past init and surface only the
    // expected config-load error.
    let output = aura_cli()
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:65000")
        .arg("--standalone")
        .arg("--config")
        .arg("/tmp/aura-cli-does-not-exist-on-disk.toml")
        .arg("--query")
        .arg("hi")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "expected non-zero exit because the config path is bogus"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("there is no reactor running"),
        "OTel init must not panic with 'no reactor running' anymore; stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "no panic should escape during OTel init; stderr was:\n{stderr}"
    );
    // The actual failure should be the missing config — confirms we
    // got *past* OTel setup before hitting the expected error.
    assert!(
        stderr.contains("Failed to load agent config")
            || stderr.contains("No such file or directory"),
        "expected config-load failure on stderr; got:\n{stderr}"
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
