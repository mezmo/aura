//! CLI tracing setup.
//!
//! Two independent layers compose the CLI's subscriber stack:
//!
//! 1. **File fmt layer** — attached only when [`AppConfig::log_file`] is set
//!    (via `--log-file`, `AURA_LOG_FILE`, or `cli.toml`). Diagnostic events
//!    are written to that path in both REPL and one-shot mode, and the file
//!    is opened in append mode. **Rotation, truncation, and pruning are the
//!    user's responsibility** — the CLI never truncates the file.
//!
//! 2. **OpenTelemetry layer** — attached only in `standalone-cli` builds
//!    when `OTEL_EXPORTER_OTLP_ENDPOINT` is set. The HTTP-mode CLI is just
//!    an SSE consumer, so OTel init lives on the server side; in standalone
//!    mode the agent runs in-process and needs its own provider.
//!
//! Both layers can be absent — the default is a no-op subscriber. When the
//! file layer is absent, no console output is produced either; the user
//! opts in by setting a log path.
//!
//! ## Runtime context
//!
//! `init` must be called from **within** a tokio runtime context (e.g.
//! `let _enter = rt.enter();` around the call, or from inside
//! `rt.block_on(...)`). The OTLP gRPC exporter's `with_tonic()` build path
//! calls `Handle::current()`, so without an entered runtime hyper-util
//! panics with "no reactor running". `main` builds the CLI's runtime
//! before calling `init` for exactly this reason.
//!
//! ## Shutdown
//!
//! Call [`aura::logging::shutdown_tracer`] from inside the same runtime
//! before it drops, to flush the `BatchSpanProcessor`'s buffered spans.
//! `main` does this with `rt.block_on(...)` after `run_oneshot` /
//! `run_repl` returns. In non-`standalone-cli` builds there's nothing to
//! flush — the call is gated behind the feature.
//!
//! [`AppConfig::log_file`]: crate::config::AppConfig::log_file

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing_subscriber::{EnvFilter, Layer, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize the global tracing subscriber for the CLI.
///
/// `log_file` — when `Some`, fmt events are written to that path in
/// append mode (created if missing). When `None`, no fmt layer is
/// installed and nothing is written to stdout/stderr/disk.
///
/// `is_standalone` — when `true` (and the `standalone-cli` feature is
/// enabled), an OpenTelemetry layer is attached if
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set. HTTP mode never needs the OTel
/// layer because traces are emitted from the server process.
///
/// **Must be called inside a tokio runtime context.** The OTLP exporter
/// build path uses `Handle::current()` and will panic otherwise. Callers
/// typically wrap with `let _enter = rt.enter();` immediately before
/// invocation. The runtime that's current at call time must outlive the
/// process (or at least live until the `shutdown_tracer` call) because
/// the `BatchSpanProcessor` worker is spawned onto it.
pub fn init(log_file: Option<&str>, is_standalone: bool) -> Result<()> {
    let fmt_layer = match log_file {
        Some(path) => Some(build_file_layer(path)?),
        None => None,
    };

    let registry = tracing_subscriber::registry().with(fmt_layer);

    // Standalone OTel wiring — gated at compile time because the `aura`
    // crate is only present with `standalone-cli` and at runtime because
    // HTTP-mode CLIs don't need their own provider. Caller is responsible
    // for entering a runtime context before calling `init`.
    #[cfg(feature = "standalone-cli")]
    {
        let otel_layer = if is_standalone {
            aura::logging::init_content_config();
            aura::logging::init_otel_provider().map(|provider| {
                aura::logging::otel_layer(provider)
                    .with_filter(aura::logging::otel_filter("aura_cli"))
            })
        } else {
            None
        };
        registry.with(otel_layer).init();
    }
    #[cfg(not(feature = "standalone-cli"))]
    {
        let _ = is_standalone;
        registry.init();
    }

    Ok(())
}

/// Open `path` in append mode, creating the file if missing. Extracted from
/// [`build_file_layer`] so the file-handling contract — append-only, no
/// truncation, fail loudly on bad paths — is unit-testable without having
/// to install a global tracing subscriber from a test.
fn open_append(path: &str) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(Path::new(path))
        .with_context(|| format!("failed to open log file {path}"))
}

/// Open `path` in append mode and build a fmt layer that writes there.
///
/// The default filter is moderately verbose — info-level for aura crates
/// and rig request handling — and can be overridden via `RUST_LOG`. ANSI
/// colours are disabled because the destination is typically a plain
/// log file.
fn build_file_layer<S>(path: &str) -> Result<Box<dyn Layer<S> + Send + Sync + 'static>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let file: File = open_append(path)?;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "warn,aura_config=info,aura=info,aura_cli=info,rig::agent::prompt_request=info".into()
    });

    let layer = fmt::layer()
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .with_filter(filter);

    Ok(Box::new(layer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn open_append_creates_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("new.log");
        assert!(
            !path.exists(),
            "precondition: log file should not exist yet"
        );

        let mut f = open_append(path.to_str().unwrap()).unwrap();
        writeln!(f, "hello").unwrap();
        drop(f);

        assert!(path.exists(), "open_append should create the file");
        assert!(std::fs::read_to_string(&path).unwrap().contains("hello"));
    }

    #[test]
    fn open_append_preserves_existing_content() {
        // The whole "user manages rotation" promise hinges on append mode —
        // a regression to truncating mode would silently wipe prior runs.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("existing.log");
        std::fs::write(&path, "previous run\n").unwrap();

        let mut f = open_append(path.to_str().unwrap()).unwrap();
        writeln!(f, "new run").unwrap();
        drop(f);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("previous run"),
            "previous content must survive"
        );
        assert!(contents.contains("new run"), "new content must be appended");
    }

    #[test]
    fn open_append_errors_when_parent_missing() {
        // We intentionally don't `create_dir_all` the parent — pointing the
        // CLI at a typo'd directory should surface a real error, not
        // silently swallow logs.
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("does/not/exist/cli.log");
        let err = open_append(bogus.to_str().unwrap()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("failed to open log file"),
            "error should mention the log file path; got: {msg}"
        );
    }
}
