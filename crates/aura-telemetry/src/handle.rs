//! The user-facing `TelemetryHandle` plus the background task that
//! drains captured events and POSTs them to PostHog `/batch`.
//!
//! Design invariants:
//!
//! - `capture` is fire-and-forget. It does no I/O and never blocks the
//!   caller. The only synchronous side effect is appending one line to
//!   the local inspection log; if that fails, we swallow the error
//!   because the inspection log is a best-effort audit trail.
//! - When telemetry is disabled, the background task is not spawned at
//!   all; `capture` still writes to the inspection log with
//!   `sent: false` so a curious user can verify the kill switch is
//!   honored.
//! - Network failures are *silent* at `tracing::debug!` level — they
//!   must never alter Aura's core behaviour.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::disable::DisableReason;
use crate::inspection_log::{disable_reason_label, InspectedEvent, InspectionLog};
use crate::properties::{DeploymentMethod, OsFamily, Source};
use crate::sink::{build_event_json, post_batch, Envelope};
use crate::{Event, EventPayload};

/// Inputs to [`init`]. Most fields have spec-derived defaults; the
/// concrete defaults live in [`TelemetryConfig::default`] for tests and
/// in the call sites in `aura-web-server` / `aura-cli` for production.
pub struct TelemetryConfig {
    pub endpoint: String,
    pub api_key: String,
    pub install_id: Uuid,
    /// Where the install UUID is persisted. Surfaced by
    /// [`TelemetryHandle::install_id_path`] for the `/telemetry status`
    /// slash command and the docs/telemetry.md "reset" instructions.
    /// `None` is only used in tests that synthesise a handle without a
    /// real filesystem location.
    pub install_id_path: Option<PathBuf>,
    pub session_id: Uuid,
    pub source: Source,
    pub os_family: OsFamily,
    pub deployment_method: DeploymentMethod,
    pub aura_version: &'static str,
    pub inspection_log_path: Option<PathBuf>,
    /// Pre-computed disable reason (from `disable::decide_disabled`
    /// plus the caller's own config-disable check).
    pub disable_reason: Option<DisableReason>,
    /// Buffer between `capture` and the background task. Defaults to
    /// 256; full → drop (incremented on the dropped counter).
    pub channel_capacity: usize,
    /// Flush when this many events are queued, regardless of timer.
    pub batch_size: usize,
    /// Flush timer (max time an event sits unsent).
    pub flush_interval: Duration,
    /// Optional pre-built reqwest client; tests inject one with a low
    /// connect timeout.
    pub http_client: Option<reqwest::Client>,
}

impl TelemetryConfig {
    pub fn default_for(
        source: Source,
        install_id: Uuid,
        endpoint: String,
        api_key: String,
        inspection_log_path: Option<PathBuf>,
    ) -> Self {
        Self {
            endpoint,
            api_key,
            install_id,
            install_id_path: None,
            session_id: Uuid::new_v4(),
            source,
            os_family: OsFamily::current(),
            deployment_method: DeploymentMethod::Local,
            aura_version: env!("CARGO_PKG_VERSION"),
            inspection_log_path,
            disable_reason: None,
            channel_capacity: 256,
            batch_size: 25,
            flush_interval: Duration::from_secs(5),
            http_client: None,
        }
    }
}

/// File-driven telemetry settings as they appear under a `[telemetry]`
/// block in the main server config (`config.toml`) or the per-user
/// `cli.toml`. Every field is optional so partial configs are valid;
/// the bootstrap layer applies env > file > built-in defaults.
///
/// This struct is also where the `enabled = false` user-facing kill
/// switch documented in `docs/telemetry.md` is wired in. When a caller
/// passes a file config with `enabled = Some(false)` and no env-level
/// disable fired first, the bootstrap layer records the disable as
/// `DisableReason::ConfigDisabled`.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileTelemetryConfig {
    /// `Some(false)` → ConfigDisabled (lowest-precedence kill switch).
    /// `Some(true)` and `None` are no-ops (env-level decisions still
    /// apply, and the built-in default is on).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Override the PostHog endpoint. Env `AURA_TELEMETRY_ENDPOINT`
    /// still wins.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Override the PostHog API key. Env `AURA_TELEMETRY_API_KEY` still
    /// wins.
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Cheap clone; the inner state lives behind an `Arc`.
#[derive(Clone)]
pub struct TelemetryHandle {
    inner: Arc<Inner>,
}

struct Inner {
    envelope: Envelope,
    inspection: Option<InspectionLog>,
    disable_reason: Option<DisableReason>,
    /// `Mutex` so [`TelemetryHandle::shutdown`] can `take()` the
    /// sender and let the background task observe channel close.
    sender: std::sync::Mutex<Option<mpsc::Sender<EventPayload>>>,
    dropped: AtomicUsize,
    bg: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    // Audit-surface fields: pure read-only echo of the resolved
    // settings so `/telemetry status` and `GET /telemetry/recent` can
    // tell users where their data is going without re-deriving paths
    // from env vars at the slash-command level.
    endpoint: String,
    install_id_path: Option<PathBuf>,
    inspection_log_path: Option<PathBuf>,
}

impl TelemetryHandle {
    /// Was the kill switch honored at init time?
    pub fn disable_reason(&self) -> Option<&DisableReason> {
        self.inner.disable_reason.as_ref()
    }

    /// Number of events dropped because the channel was full.
    /// Surfaced for the `/telemetry status` slash command and tests;
    /// never sent to PostHog.
    pub fn dropped_count(&self) -> usize {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// PostHog endpoint base URL the sink is targeting. Surfaced by
    /// `/telemetry status` so users can see at a glance whether they
    /// are pointed at Mezmo's project, a self-hosted PostHog, or
    /// something stale from `cli.toml`.
    pub fn endpoint(&self) -> &str {
        &self.inner.endpoint
    }

    /// Where the persisted install-id lives on disk. Surfaced so the
    /// `rm <path>` reset documented in `docs/telemetry.md` is one
    /// glance away from the status output.
    pub fn install_id_path(&self) -> Option<&Path> {
        self.inner.install_id_path.as_deref()
    }

    /// Where the local inspection log is being written. `None` when
    /// the user disabled the log via `AURA_TELEMETRY_LOG_EVENTS=0`.
    pub fn inspection_log_path(&self) -> Option<&Path> {
        self.inner.inspection_log_path.as_deref()
    }

    /// Fire-and-forget event capture. Always writes to the inspection
    /// log (if configured). If telemetry is active, also enqueues for
    /// the background sink.
    pub fn capture<E: Event>(&self, event: E) {
        let payload = event.into_payload();
        self.capture_payload(payload);
    }

    /// Lower-level capture for callers that already have an
    /// `EventPayload` (e.g. the synthetic `telemetry_opt_out` first
    /// record). Tests also use this.
    pub fn capture_payload(&self, payload: EventPayload) {
        let mut sent = false;
        if self.inner.disable_reason.is_none() {
            let tx_guard = self.inner.sender.lock().expect("sender mutex poisoned");
            if let Some(tx) = tx_guard.as_ref() {
                // `try_send` is non-blocking; a full channel means we
                // are under burst pressure. Drop and increment counter.
                match tx.try_send(payload.clone()) {
                    Ok(()) => sent = true,
                    Err(_) => {
                        self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        if let Some(log) = &self.inner.inspection {
            let envelope_props =
                build_event_json(&self.inner.envelope, &payload, &Utc::now().to_rfc3339());
            let inspected = InspectedEvent {
                ts: Utc::now(),
                event: payload.name.to_string(),
                properties: envelope_props
                    .get("properties")
                    .cloned()
                    .unwrap_or(Value::Null),
                sent,
                disable_reason: self
                    .inner
                    .disable_reason
                    .as_ref()
                    .map(disable_reason_label),
            };
            // Inspection-log write failures are surfaced at debug
            // level; we do not want to crash the caller because the
            // user's disk is full.
            if let Err(e) = log.append(&inspected) {
                tracing::debug!(error = %e, "inspection log append failed");
            }
        }
    }

    /// Drain in-flight events and stop the background task. Caller
    /// gives a max budget; we never block forever.
    pub async fn shutdown(self, budget: Duration) {
        // Drop the sender so the background task observes channel
        // close after it has drained any pending events.
        {
            let mut tx_guard = self.inner.sender.lock().expect("sender mutex poisoned");
            tx_guard.take();
        }
        let mut bg = self.inner.bg.lock().await;
        if let Some(handle) = bg.take() {
            let _ = tokio::time::timeout(budget, handle).await;
        }
    }

    /// Borrow the inspection log for read-only access (e.g. the
    /// `/telemetry recent` slash command and the
    /// `GET /telemetry/recent` web endpoint).
    pub fn inspection_log(&self) -> Option<&InspectionLog> {
        self.inner.inspection.as_ref()
    }

    /// The session UUID — exposed for the web server to thread into
    /// `aura.session_info` SSE events. Never linked to a user.
    pub fn session_id(&self) -> Uuid {
        self.inner.envelope.session_id
    }
}

/// Initialise the telemetry layer.
///
/// Always succeeds: when the inspection-log path is set and openable
/// the user-audit guarantee holds; when it cannot be opened we log at
/// debug and continue with a no-op inspection log so a misconfigured
/// install never crashes Aura.
pub fn init(config: TelemetryConfig) -> TelemetryHandle {
    let envelope = Envelope {
        install_id: config.install_id,
        session_id: config.session_id,
        source: config.source,
        os_family: config.os_family,
        deployment_method: config.deployment_method,
        aura_version: config.aura_version,
    };
    let inspection = config.inspection_log_path.as_ref().and_then(|p| {
        match InspectionLog::open(p.clone(), crate::inspection_log::DEFAULT_ROTATION_LINES) {
            Ok(log) => Some(log),
            Err(e) => {
                tracing::debug!(error = %e, path = %p.display(), "failed to open inspection log");
                None
            }
        }
    });

    let (sender, bg_handle) = if config.disable_reason.is_none() {
        let (tx, rx) = mpsc::channel::<EventPayload>(config.channel_capacity);
        let client = config
            .http_client
            .clone()
            .unwrap_or_else(|| reqwest::Client::new());
        let endpoint = config.endpoint.clone();
        let api_key = config.api_key.clone();
        let envelope_for_task = envelope.clone();
        let batch_size = config.batch_size;
        let flush_interval = config.flush_interval;
        let handle = tokio::spawn(run_background(BackgroundCtx {
            rx,
            client,
            endpoint,
            api_key,
            envelope: envelope_for_task,
            batch_size,
            flush_interval,
        }));
        (Some(tx), Some(handle))
    } else {
        (None, None)
    };

    let handle = TelemetryHandle {
        inner: Arc::new(Inner {
            envelope,
            inspection,
            disable_reason: config.disable_reason.clone(),
            sender: std::sync::Mutex::new(sender),
            dropped: AtomicUsize::new(0),
            bg: tokio::sync::Mutex::new(bg_handle),
            endpoint: config.endpoint.clone(),
            install_id_path: config.install_id_path.clone(),
            inspection_log_path: config.inspection_log_path.clone(),
        }),
    };

    // Synthetic first record: when disabled, write a `telemetry_opt_out`
    // line so the user can see in `/telemetry recent` that the kill
    // switch took effect. Never goes on the wire.
    if let Some(reason) = &config.disable_reason {
        if let Some(log) = &handle.inner.inspection {
            let label = disable_reason_label(reason);
            let mut props = serde_json::Map::new();
            props.insert("reason".into(), Value::String(label.clone()));
            props.insert(
                "aura_source".into(),
                Value::String(handle.inner.envelope.source.as_str().into()),
            );
            let inspected = InspectedEvent {
                ts: Utc::now(),
                event: "telemetry_opt_out".into(),
                properties: Value::Object(props),
                sent: false,
                disable_reason: Some(label),
            };
            if let Err(e) = log.append(&inspected) {
                tracing::debug!(error = %e, "could not record telemetry_opt_out");
            }
        }
    }

    handle
}

struct BackgroundCtx {
    rx: mpsc::Receiver<EventPayload>,
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    envelope: Envelope,
    batch_size: usize,
    flush_interval: Duration,
}

async fn run_background(mut ctx: BackgroundCtx) {
    let mut buf: Vec<Value> = Vec::with_capacity(ctx.batch_size);
    let mut ticker = tokio::time::interval(ctx.flush_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick so we don't try to flush an empty
    // buffer right after init.
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            maybe_payload = ctx.rx.recv() => {
                match maybe_payload {
                    Some(payload) => {
                        let ts = Utc::now().to_rfc3339();
                        buf.push(build_event_json(&ctx.envelope, &payload, &ts));
                        if buf.len() >= ctx.batch_size {
                            flush(&ctx, &mut buf).await;
                        }
                    }
                    None => {
                        // Channel closed: final flush then exit.
                        flush(&ctx, &mut buf).await;
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                if !buf.is_empty() {
                    flush(&ctx, &mut buf).await;
                }
            }
        }
    }
}

async fn flush(ctx: &BackgroundCtx, buf: &mut Vec<Value>) {
    if buf.is_empty() {
        return;
    }
    if let Err(e) = post_batch(&ctx.client, &ctx.endpoint, &ctx.api_key, buf).await {
        tracing::debug!(error = %e, "telemetry post failed");
    }
    buf.clear();
}

