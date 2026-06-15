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
//!
//! The runtime-mutable core is a single [`ConsentSink`] behind one
//! `std::sync::Mutex`: it pairs the consent state with the background
//! sink's lifecycle so the two can never disagree. `route` is the one
//! place that decides, for a captured event, whether it is held locally
//! or queued to the sink.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::disable::{DisableReason, TelemetryState};
use crate::inspection_log::{InspectedEvent, InspectionLog};
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
    /// The resolved telemetry state (from `disable::decide_state`).
    /// `Unknown` holds events locally without sending; `Enabled` spawns
    /// the sink at init; `Disabled` never sends. A handle that inits
    /// `Unknown` can later transition to `Enabled` at runtime via
    /// [`TelemetryHandle::enable`].
    pub state: TelemetryState,
    /// Buffer between `capture` and the background task. Defaults to
    /// 256; full → drop (incremented on the dropped counter).
    pub channel_capacity: usize,
    /// Flush when this many events are queued, regardless of timer.
    pub batch_size: usize,
    /// Flush timer (max time an event sits unsent).
    pub flush_interval: Duration,
    /// Per-request POST budget. Must be **shorter than the shutdown
    /// budget** the caller passes to [`TelemetryHandle::shutdown`],
    /// otherwise an in-flight POST during shutdown will outlive the
    /// budget and the background task will be cancelled before
    /// writing its post-flush inspection-log rows — leaving the
    /// captured event neither delivered nor recorded. Default 1.5s
    /// pairs with the 2s shutdown budget aura-cli and aura-web-server
    /// use in production.
    pub post_timeout: Duration,
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
            state: TelemetryState::Enabled,
            channel_capacity: 256,
            batch_size: 25,
            flush_interval: Duration::from_secs(5),
            post_timeout: Duration::from_millis(1500),
            http_client: None,
        }
    }
}

/// Outcome of [`TelemetryHandle::enable`], so callers can report
/// honestly instead of always claiming success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnableOutcome {
    /// State is now `Enabled` and the sink is running for this session
    /// (either a fresh `Unknown → Enabled`, or resuming a runtime
    /// `/telemetry disable`).
    Enabled,
    /// Was already `Enabled`; nothing changed.
    AlreadyEnabled,
    /// Could not enable for this session: telemetry was hard-disabled at
    /// init (a kill switch such as `DO_NOT_TRACK`, or config
    /// `enabled = false`), so there is no captured sink to start. A
    /// persisted preference still applies on the next launch — provided
    /// the kill switch is no longer in force.
    HeldUntilRestart,
}

/// Cheap clone; the inner state lives behind an `Arc`.
#[derive(Clone)]
pub struct TelemetryHandle {
    inner: Arc<Inner>,
}

/// Everything the background flush task needs, cloned once per spawn.
/// Shared verbatim by `init` (Enabled-at-start) and
/// [`TelemetryHandle::enable`] (Unknown→Enabled at runtime).
#[derive(Clone)]
struct SinkIo {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    envelope: Envelope,
    batch_size: usize,
    flush_interval: Duration,
    post_timeout: Duration,
    /// Inspection-log handle for post-flush writes. `None` when the
    /// user disabled the log via `AURA_TELEMETRY_LOG_EVENTS=0`.
    inspection: Option<InspectionLog>,
}

/// Everything `init` stashes so the sink can be spawned later, when an
/// `Unknown` handle transitions to `Enabled` mid-process. Held inside
/// [`SinkState::Dormant`] only when a tokio runtime was current at init.
struct SinkParams {
    /// Captured at `init` (which runs under the runtime). `enable()` runs
    /// from the REPL loop body **outside** `rt.enter()`, so a bare
    /// `tokio::spawn` there would panic; we spawn via this handle.
    runtime: tokio::runtime::Handle,
    channel_capacity: usize,
    io: SinkIo,
}

/// The background sink's lifecycle, paired with consent in [`ConsentSink`].
///
/// The two states a naive `Option`-soup would also let us build —
/// "Enabled but no sink that can ever run" and "Unknown with a sink
/// already running" — are unrepresentable here: a sink only reaches
/// `Running` through [`ConsentSink::enable`], which flips consent to
/// `Enabled` in the same step.
enum SinkState {
    /// Hard-disabled at init, or no tokio runtime was available. Cannot
    /// ever run a sink this process. Permanent.
    NeverSpawnable,
    /// Captured at init, spawnable later via [`ConsentSink::enable`].
    Dormant(SinkParams),
    /// Channel + background task are live.
    Running {
        tx: mpsc::Sender<EventPayload>,
        bg: tokio::task::JoinHandle<()>,
    },
}

/// The single runtime-mutable core: consent plus the sink lifecycle,
/// guarded by one mutex on [`Inner`].
struct ConsentSink {
    consent: TelemetryState,
    sink: SinkState,
}

/// What [`ConsentSink::route`] decides for one captured event.
enum CaptureRoute {
    /// Hold locally: append to the inspection log with this
    /// `not_sent_reason` label; nothing goes on the wire.
    Held(String),
    /// Queue to the running sink via this (cloned) sender. The caller
    /// sends after dropping the lock.
    Queue(mpsc::Sender<EventPayload>),
}

impl ConsentSink {
    /// Decide how a captured event is handled. Consent is checked first:
    /// a `Disabled` or `Unknown` handle holds the event locally and never
    /// inspects the sink. Only `Enabled` with a `Running` sink queues;
    /// `Enabled` without one (test fixtures that bypass `init`) records a
    /// `NoSink` row so the event is still inspectable.
    fn route(&self) -> CaptureRoute {
        match &self.consent {
            TelemetryState::Disabled(reason) => CaptureRoute::Held(reason.to_string()),
            TelemetryState::Unknown => CaptureRoute::Held("Unknown".to_string()),
            TelemetryState::Enabled => match &self.sink {
                SinkState::Running { tx, .. } => CaptureRoute::Queue(tx.clone()),
                SinkState::NeverSpawnable | SinkState::Dormant(_) => {
                    CaptureRoute::Held("NoSink".to_string())
                }
            },
        }
    }

    /// Transition to `Enabled` and start (or resume) the sink. Enabling
    /// from `Unknown` (first-input consent) or from a runtime
    /// `/telemetry disable` (symmetric undo) only works when a sink was
    /// captured at init; a `NeverSpawnable` sink (startup kill switch, or
    /// no runtime) yields [`EnableOutcome::HeldUntilRestart`] and leaves
    /// consent untouched. Spawning is synchronous, so this is safe to run
    /// while holding the state mutex.
    fn enable(&mut self) -> EnableOutcome {
        if matches!(self.consent, TelemetryState::Enabled) {
            return EnableOutcome::AlreadyEnabled;
        }
        match std::mem::replace(&mut self.sink, SinkState::NeverSpawnable) {
            // No sink could ever run this process — never resurrect it.
            // The swapped-in `NeverSpawnable` is a no-op; consent stays
            // `Unknown`/`Disabled`.
            SinkState::NeverSpawnable => EnableOutcome::HeldUntilRestart,
            SinkState::Dormant(params) => {
                let (tx, bg) = spawn_sink(&params);
                self.sink = SinkState::Running { tx, bg };
                self.consent = TelemetryState::Enabled;
                EnableOutcome::Enabled
            }
            // Already running (runtime disable → re-enable): keep the
            // task, just flip consent back on.
            SinkState::Running { tx, bg } => {
                self.sink = SinkState::Running { tx, bg };
                self.consent = TelemetryState::Enabled;
                EnableOutcome::Enabled
            }
        }
    }

    /// Transition to `Disabled` at runtime (the first-input opt-out). A
    /// running sink is left alone — it simply stops receiving, because
    /// `route` now holds every new event. Persisting the preference is
    /// the caller's responsibility.
    fn disable(&mut self, reason: DisableReason) {
        self.consent = TelemetryState::Disabled(reason);
    }

    /// Tear down the sink for shutdown: drop the sender (so the task
    /// drains pending events then observes channel close) and hand back
    /// the `JoinHandle` to await. Idempotent — the sink is swapped to
    /// `NeverSpawnable`, so a second call (a cloned handle shutting down
    /// twice) returns `None`.
    fn begin_shutdown(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        match std::mem::replace(&mut self.sink, SinkState::NeverSpawnable) {
            SinkState::Running { tx, bg } => {
                drop(tx);
                Some(bg)
            }
            SinkState::Dormant(_) | SinkState::NeverSpawnable => None,
        }
    }
}

impl TelemetryHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, ConsentSink> {
        self.inner
            .state
            .lock()
            .expect("telemetry state mutex poisoned")
    }

    /// Current telemetry state (`Unknown` / `Enabled` / `Disabled`).
    /// Cloned out so callers don't hold the lock.
    pub fn state(&self) -> TelemetryState {
        self.lock().consent.clone()
    }

    /// Transition to `Enabled` at runtime and start (or resume) the sink.
    /// See [`ConsentSink::enable`] for the transition rules. Idempotent;
    /// events captured while `Unknown`/`Disabled` were never queued, so
    /// enabling does **not** backfill them.
    #[must_use = "a HeldUntilRestart outcome means telemetry stayed off; ignoring it can falsely imply the session is now sending"]
    pub fn enable(&self) -> EnableOutcome {
        self.lock().enable()
    }

    /// Transition to `Disabled` at runtime (the first-input opt-out).
    /// Future captures are held; any already-spawned sink simply stops
    /// receiving. Persisting the preference is the caller's responsibility.
    pub fn set_disabled(&self, reason: DisableReason) {
        self.lock().disable(reason);
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

    /// Fire-and-forget event capture. When telemetry is active the
    /// inspection-log entry is written by the background task **after**
    /// the POST result is known, so `sent: true` is honest. When
    /// telemetry is disabled or the channel is full the entry is
    /// written here, immediately, with `sent: false` and a stable
    /// `not_sent_reason` (a kill switch name or `ChannelFull`).
    pub fn capture<E: Event>(&self, event: E) {
        let payload = event.into_payload();
        self.capture_payload(payload);
    }

    /// Lower-level capture for callers that already have an
    /// `EventPayload` (e.g. the synthetic `telemetry_opt_out` first
    /// record). Tests also use this.
    pub fn capture_payload(&self, payload: EventPayload) {
        // Decide under the lock, act after releasing it. `route` returns
        // an owned value (a label to hold, or a cloned sender), so the
        // guard is dropped at the end of this statement and the
        // `try_send` below never runs while the lock is held.
        let route = self.lock().route();
        match route {
            CaptureRoute::Held(reason) => {
                self.append_local(&payload, false, Some(reason));
            }
            CaptureRoute::Queue(tx) => {
                // `try_send` is non-blocking; a full channel means burst
                // pressure. Drop, increment the counter, AND surface the
                // drop in the inspection log so a user inspecting "what
                // happened to my event" sees the truth rather than
                // silence.
                if tx.try_send(payload.clone()).is_err() {
                    self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                    self.append_local(&payload, false, Some("ChannelFull".into()));
                }
            }
        }
    }

    /// Append one record to the inspection log with the supplied
    /// `sent` / `not_sent_reason` pair. Errors are downgraded to
    /// `tracing::debug!` because the local log is a best-effort audit
    /// trail and must never crash the caller.
    fn append_local(&self, payload: &EventPayload, sent: bool, reason: Option<String>) {
        let Some(log) = self.inner.inspection.as_ref() else {
            return;
        };
        let now = Utc::now();
        let envelope_props = build_event_json(&self.inner.envelope, payload, &now.to_rfc3339());
        let inspected = InspectedEvent {
            ts: now,
            event: payload.name.to_string(),
            properties: envelope_props
                .get("properties")
                .cloned()
                .unwrap_or(Value::Null),
            sent,
            not_sent_reason: reason,
        };
        if let Err(e) = log.append(&inspected) {
            tracing::debug!(error = %e, "inspection log append failed");
        }
    }

    /// Drain in-flight events and stop the background task. Caller
    /// gives a max budget; we never block forever. The `JoinHandle` is
    /// extracted under the lock, which is released before the `.await`.
    pub async fn shutdown(self, budget: Duration) {
        let bg = self.lock().begin_shutdown();
        if let Some(handle) = bg {
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

struct Inner {
    envelope: Envelope,
    inspection: Option<InspectionLog>,
    /// Consent + sink lifecycle. Read by `capture_payload` (sync) and
    /// mutated by `enable`/`set_disabled`/`shutdown` (sync).
    state: std::sync::Mutex<ConsentSink>,
    dropped: AtomicUsize,
    // Audit-surface fields: pure read-only echo of the resolved
    // settings so `/telemetry status` and `GET /telemetry/recent` can
    // tell users where their data is going without re-deriving paths
    // from env vars at the slash-command level.
    endpoint: String,
    install_id_path: Option<PathBuf>,
    inspection_log_path: Option<PathBuf>,
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

    let state = config.state.clone();

    let io = SinkIo {
        client: config.http_client.clone().unwrap_or_default(),
        endpoint: config.endpoint.clone(),
        api_key: config.api_key.clone(),
        envelope: envelope.clone(),
        batch_size: config.batch_size,
        flush_interval: config.flush_interval,
        post_timeout: config.post_timeout,
        inspection: inspection.clone(),
    };

    // Decide the initial sink state. Disabled never spawns. Otherwise we
    // need a current tokio runtime (always present in production — init
    // runs under rt.enter(); a missing runtime only happens in non-async
    // test fixtures, which degrade to local-only inspection writes). When
    // Enabled at init we spawn eagerly; Unknown stays Dormant until
    // `enable()`.
    let sink = if matches!(state, TelemetryState::Disabled(_)) {
        SinkState::NeverSpawnable
    } else {
        match tokio::runtime::Handle::try_current().ok() {
            Some(runtime) => {
                let params = SinkParams {
                    runtime,
                    channel_capacity: config.channel_capacity,
                    io,
                };
                if matches!(state, TelemetryState::Enabled) {
                    let (tx, bg) = spawn_sink(&params);
                    SinkState::Running { tx, bg }
                } else {
                    SinkState::Dormant(params)
                }
            }
            None => SinkState::NeverSpawnable,
        }
    };

    let handle = TelemetryHandle {
        inner: Arc::new(Inner {
            envelope,
            inspection,
            state: std::sync::Mutex::new(ConsentSink {
                consent: state.clone(),
                sink,
            }),
            dropped: AtomicUsize::new(0),
            endpoint: config.endpoint.clone(),
            install_id_path: config.install_id_path.clone(),
            inspection_log_path: config.inspection_log_path.clone(),
        }),
    };

    // Synthetic first record: when disabled, write a `telemetry_opt_out`
    // line so the user can see in `/telemetry recent` that the kill
    // switch took effect. Never goes on the wire. (Unknown writes no
    // synthetic record — its held events carry the `"Unknown"` label.)
    if let TelemetryState::Disabled(reason) = &state {
        if let Some(log) = &handle.inner.inspection {
            let label = reason.to_string();
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
                not_sent_reason: Some(label),
            };
            if let Err(e) = log.append(&inspected) {
                tracing::debug!(error = %e, "could not record telemetry_opt_out");
            }
        }
    }

    handle
}

/// Create the channel + spawn the background flush task via the stored
/// runtime handle.
fn spawn_sink(p: &SinkParams) -> (mpsc::Sender<EventPayload>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<EventPayload>(p.channel_capacity);
    let bg = p.runtime.spawn(run_background(rx, p.io.clone()));
    (tx, bg)
}

/// One buffered event awaiting flush. We carry the wire JSON and the
/// `InspectedEvent` skeleton side by side so the inspection-log row
/// can be finalised with the actual POST outcome once `flush`
/// completes — no second build pass, no timestamp drift between the
/// wire and the local audit trail.
struct Pending {
    wire: Value,
    inspected: InspectedEvent,
}

async fn run_background(mut rx: mpsc::Receiver<EventPayload>, io: SinkIo) {
    let mut buf: Vec<Pending> = Vec::with_capacity(io.batch_size);
    let mut ticker = tokio::time::interval(io.flush_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick so we don't try to flush an empty
    // buffer right after init.
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            maybe_payload = rx.recv() => {
                match maybe_payload {
                    Some(payload) => {
                        buf.push(build_pending(&io.envelope, payload));
                        if buf.len() >= io.batch_size {
                            flush(&io, &mut buf).await;
                        }
                    }
                    None => {
                        // Channel closed: final flush then exit.
                        flush(&io, &mut buf).await;
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                if !buf.is_empty() {
                    flush(&io, &mut buf).await;
                }
            }
        }
    }
}

fn build_pending(envelope: &Envelope, payload: EventPayload) -> Pending {
    let ts = Utc::now();
    let wire = build_event_json(envelope, &payload, &ts.to_rfc3339());
    let properties = wire.get("properties").cloned().unwrap_or(Value::Null);
    let inspected = InspectedEvent {
        ts,
        event: payload.name.to_string(),
        properties,
        // Finalised post-flush. The buffered placeholder is never
        // observable to a reader because we only call
        // `inspection.append` after the POST returns.
        sent: false,
        not_sent_reason: Some("InFlight".into()),
    };
    Pending { wire, inspected }
}

async fn flush(io: &SinkIo, buf: &mut Vec<Pending>) {
    if buf.is_empty() {
        return;
    }
    let wires: Vec<Value> = buf.iter().map(|p| p.wire.clone()).collect();
    let result = post_batch(
        &io.client,
        &io.endpoint,
        &io.api_key,
        &wires,
        io.post_timeout,
    )
    .await;
    let (sent, reason) = match &result {
        Ok(()) => (true, None),
        Err(e) => (
            false,
            Some(format!(
                "PostFailed({})",
                crate::sink::classify_post_error(e)
            )),
        ),
    };
    if let Err(e) = &result {
        tracing::debug!(error = %e, "telemetry post failed");
    }
    if let Some(log) = io.inspection.as_ref() {
        for pending in buf.drain(..) {
            let mut inspected = pending.inspected;
            inspected.sent = sent;
            inspected.not_sent_reason = reason.clone();
            if let Err(e) = log.append(&inspected) {
                tracing::debug!(error = %e, "inspection log append failed");
            }
        }
    } else {
        buf.clear();
    }
}
