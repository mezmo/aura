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

use crate::disable::{DisableReason, TelemetryState};
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

impl FileTelemetryConfig {
    /// Merge `over` on top of `self`, returning the combined config.
    ///
    /// The kill switch AND-merges: `enabled = Some(false)` from
    /// **either** layer wins, so an opt-out recorded in one file can
    /// never be silently reversed by another (a project `cli.toml`
    /// shipping `enabled = true`, or a second server config loaded from
    /// a `CONFIG_PATH` directory). Only when no layer asserts `false`
    /// do the usual "overlay wins" semantics apply. Non-kill-switch
    /// fields (`endpoint`, `api_key`) take the overlay's value when
    /// set.
    ///
    /// This is the single definition of cross-file telemetry layering;
    /// both the CLI (global + project `cli.toml`) and the web server
    /// (multi-TOML `CONFIG_PATH`) fold their configs through it.
    pub fn merged_over(self, over: FileTelemetryConfig) -> FileTelemetryConfig {
        let enabled = if self.enabled == Some(false) || over.enabled == Some(false) {
            Some(false)
        } else {
            over.enabled.or(self.enabled)
        };
        FileTelemetryConfig {
            enabled,
            endpoint: over.endpoint.or(self.endpoint),
            api_key: over.api_key.or(self.api_key),
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

/// Everything `init` stashes so [`TelemetryHandle::enable`] can spawn the
/// background sink later, when an `Unknown` handle transitions to
/// `Enabled` mid-process. Present only when the handle is not `Disabled`
/// at init (a kill switch can never be resurrected) **and** a tokio
/// runtime was current at init time.
struct SinkParams {
    /// Captured at `init` (which runs under the runtime). `enable()` runs
    /// from the REPL loop body **outside** `rt.enter()`, so a bare
    /// `tokio::spawn` there would panic; we spawn via this handle.
    runtime: tokio::runtime::Handle,
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
    envelope: Envelope,
    batch_size: usize,
    flush_interval: Duration,
    post_timeout: Duration,
    channel_capacity: usize,
    inspection: Option<InspectionLog>,
}

struct Inner {
    envelope: Envelope,
    inspection: Option<InspectionLog>,
    /// Runtime-mutable telemetry state. Read by `capture_payload` (sync)
    /// and flipped by `enable`/`set_disabled` (sync).
    state: std::sync::Mutex<TelemetryState>,
    /// `Some` only when the handle may ever send (init state ≠ Disabled
    /// and a runtime was available). Consumed by `enable()`.
    sink_params: Option<SinkParams>,
    /// `Mutex` so [`TelemetryHandle::shutdown`] can `take()` the
    /// sender and let the background task observe channel close.
    sender: std::sync::Mutex<Option<mpsc::Sender<EventPayload>>>,
    dropped: AtomicUsize,
    /// `std::sync::Mutex` (not tokio) so `enable()` can populate it from
    /// a sync context; `shutdown` `take()`s the handle before awaiting,
    /// so the lock is never held across `.await`.
    bg: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    // Audit-surface fields: pure read-only echo of the resolved
    // settings so `/telemetry status` and `GET /telemetry/recent` can
    // tell users where their data is going without re-deriving paths
    // from env vars at the slash-command level.
    endpoint: String,
    install_id_path: Option<PathBuf>,
    inspection_log_path: Option<PathBuf>,
}

impl TelemetryHandle {
    /// Current telemetry state (`Unknown` / `Enabled` / `Disabled`).
    /// Cloned out so callers don't hold the lock.
    pub fn state(&self) -> TelemetryState {
        self.inner
            .state
            .lock()
            .expect("state mutex poisoned")
            .clone()
    }

    /// Transition to `Enabled` at runtime and start (or resume) the sink.
    ///
    /// Enables from `Unknown` (the first-input consent gate) **or** from a
    /// runtime `/telemetry disable` (so `/telemetry enable` is a symmetric,
    /// immediate undo). Both are possible only when a sink was captured at
    /// `init` — i.e. init was not a hard `Disabled`. A startup kill switch
    /// or config `enabled = false` leaves `sink_params` empty and can
    /// **never** be resurrected in-process; that returns
    /// [`EnableOutcome::HeldUntilRestart`] and leaves the state `Disabled`.
    ///
    /// Idempotent: a second call once the sink exists just flips the state
    /// flag. Events captured while `Unknown`/`Disabled` were never queued,
    /// so enabling does **not** backfill them. Spawns via the runtime
    /// handle captured at `init`.
    pub fn enable(&self) -> EnableOutcome {
        {
            let st = self.inner.state.lock().expect("state mutex poisoned");
            if matches!(*st, TelemetryState::Enabled) {
                return EnableOutcome::AlreadyEnabled;
            }
            // Unknown, or a runtime `set_disabled`. Either can transition,
            // but only if a sink was captured at init. No sink ⇒ init was
            // a hard Disabled (kill switch / config), which we never
            // resurrect: hold until the next launch.
            if self.inner.sink_params.is_none() {
                return EnableOutcome::HeldUntilRestart;
            }
        }
        // Install the sender BEFORE publishing `Enabled` (see
        // `ensure_sink`): if the state flipped first, a concurrent capture
        // could observe `Enabled` with no sender and drop the event.
        self.ensure_sink();
        *self.inner.state.lock().expect("state mutex poisoned") = TelemetryState::Enabled;
        EnableOutcome::Enabled
    }

    /// Spawn the background sink if it is not already running. No-op when
    /// no `sink_params` were captured at init (a hard `Disabled`, or no
    /// tokio runtime was available) or when a sink already exists.
    ///
    /// **Does not change the telemetry state.** Callers that want to
    /// publish `Enabled` must do so *after* this returns, so a concurrent
    /// capture never sees `Enabled` with no sender. `capture_consented`
    /// relies on this to send a request-scoped event while leaving the
    /// global state untouched.
    fn ensure_sink(&self) {
        let Some(params) = self.inner.sink_params.as_ref() else {
            return;
        };
        let mut tx_guard = self.inner.sender.lock().expect("sender mutex poisoned");
        if tx_guard.is_none() {
            let (tx, bg) = spawn_sink(params);
            *tx_guard = Some(tx);
            *self.inner.bg.lock().expect("bg mutex poisoned") = Some(bg);
        }
    }

    /// Transition to `Disabled` at runtime (the first-input opt-out).
    /// Future captures are held; any already-spawned sink simply stops
    /// receiving (no new events are queued). Persisting the preference is
    /// the caller's responsibility.
    pub fn set_disabled(&self, reason: DisableReason) {
        *self.inner.state.lock().expect("state mutex poisoned") = TelemetryState::Disabled(reason);
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

    /// Capture an event that carries **per-request consent** — e.g. an
    /// Enabled CLI's `X-Aura-Telemetry-Consent` header on the request
    /// being handled. Consent rides with the request, so this sends even
    /// when the server's own global state is `Unknown`, **without**
    /// changing that state and without any operator-level opt-in.
    ///
    /// An operator opt-out still wins: when the global state is
    /// `Disabled` the event is held (inspection-log only), exactly like
    /// [`capture`](Self::capture). This is what makes header-driven
    /// propagation safe by default — a forged header can only emit
    /// anonymous telemetry about the forger's *own* request; it cannot
    /// flip a global switch or affect any other user. Server-side
    /// per-request events route through here, gated on the request's
    /// consent header.
    pub fn capture_consented<E: Event>(&self, event: E) {
        let payload = event.into_payload();
        // Operator opt-out always wins, even with request consent.
        if let TelemetryState::Disabled(reason) = self.state() {
            self.append_local(&payload, false, Some(disable_reason_label(&reason)));
            return;
        }
        // Unknown or Enabled: send. Bring the sink up without touching the
        // global state, so the server stays Unknown and its own
        // non-consented captures remain held.
        self.ensure_sink();
        self.enqueue_or_record(payload);
    }

    /// Lower-level capture for callers that already have an
    /// `EventPayload` (e.g. the synthetic `telemetry_opt_out` first
    /// record). Tests also use this.
    pub fn capture_payload(&self, payload: EventPayload) {
        // Held paths (Disabled / Unknown): nothing goes on the wire;
        // write the inspection-log entry here with a stable label so the
        // user can inspect what was *held* (and, for Unknown, what
        // *would* be sent). No channel send, no background-task
        // involvement, and Unknown events are never backfilled because
        // they are never queued.
        match self.state() {
            TelemetryState::Disabled(reason) => {
                self.append_local(&payload, false, Some(disable_reason_label(&reason)));
                return;
            }
            TelemetryState::Unknown => {
                self.append_local(&payload, false, Some("Unknown".into()));
                return;
            }
            TelemetryState::Enabled => {}
        }
        // Active path: hand the payload to the background task.
        self.enqueue_or_record(payload);
    }

    /// Queue a payload to the background sink, or record it locally as
    /// not-sent when the channel is full (`ChannelFull`) or no sink
    /// exists (`NoSink`). The background task is the only writer that
    /// knows the POST outcome, so it owns the inspection-log append for
    /// successfully-queued events — nothing is recorded here on the
    /// happy path. Shared by the `Enabled` global path
    /// ([`capture_payload`](Self::capture_payload)) and the request-scoped
    /// path ([`capture_consented`](Self::capture_consented)).
    fn enqueue_or_record(&self, payload: EventPayload) {
        let tx_guard = self.inner.sender.lock().expect("sender mutex poisoned");
        if let Some(tx) = tx_guard.as_ref() {
            // `try_send` is non-blocking; a full channel means burst
            // pressure. Drop, increment the counter, AND surface the
            // drop in the inspection log so a user inspecting "what
            // happened to my event" sees the truth rather than silence.
            if tx.try_send(payload.clone()).is_err() {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                drop(tx_guard);
                self.append_local(&payload, false, Some("ChannelFull".into()));
            }
        } else {
            // No background task: `ensure_sink`/`enable` could not spawn
            // one (no `sink_params` — a hard Disabled at init, or no
            // runtime). Record locally so the user still sees the event.
            drop(tx_guard);
            self.append_local(&payload, false, Some("NoSink".into()));
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
    /// gives a max budget; we never block forever.
    pub async fn shutdown(self, budget: Duration) {
        // The drain budget must exceed the per-POST timeout, otherwise an
        // in-flight POST is cancelled mid-write and its inspection-log row
        // is lost (the `post_timeout` invariant — see `TelemetryConfig`).
        // `init` cannot enforce it (the budget is only known here), so
        // surface a violation rather than letting it fail silently.
        if let Some(params) = self.inner.sink_params.as_ref() {
            if drain_budget_too_short(budget, params.post_timeout) {
                tracing::warn!(
                    budget_ms = budget.as_millis() as u64,
                    post_timeout_ms = params.post_timeout.as_millis() as u64,
                    "telemetry shutdown budget <= per-POST timeout; an \
                     in-flight POST may be cancelled mid-write and its \
                     inspection-log row dropped. Set post_timeout below \
                     the shutdown budget."
                );
            }
        }
        // Drop the sender so the background task observes channel
        // close after it has drained any pending events.
        {
            let mut tx_guard = self.inner.sender.lock().expect("sender mutex poisoned");
            tx_guard.take();
        }
        // `take()` the JoinHandle out of the std mutex BEFORE awaiting,
        // so the lock is never held across `.await`.
        let bg = self.inner.bg.lock().expect("bg mutex poisoned").take();
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

    // Stash everything needed to spawn the sink later, but only when the
    // handle could ever send (not Disabled) AND a tokio runtime is
    // current (it always is in production — init runs under rt.enter();
    // a missing runtime only happens in non-async test fixtures, which
    // then degrade to local-only inspection writes).
    let sink_params = if matches!(state, TelemetryState::Disabled(_)) {
        None
    } else {
        tokio::runtime::Handle::try_current()
            .ok()
            .map(|runtime| SinkParams {
                runtime,
                client: config.http_client.clone().unwrap_or_default(),
                endpoint: config.endpoint.clone(),
                api_key: config.api_key.clone(),
                envelope: envelope.clone(),
                batch_size: config.batch_size,
                flush_interval: config.flush_interval,
                post_timeout: config.post_timeout,
                channel_capacity: config.channel_capacity,
                inspection: inspection.clone(),
            })
    };

    // Spawn the sink eagerly only when Enabled at init. Unknown defers
    // to `enable()`; Disabled never spawns.
    let (sender, bg_handle) = match (&state, sink_params.as_ref()) {
        (TelemetryState::Enabled, Some(p)) => {
            let (tx, bg) = spawn_sink(p);
            (Some(tx), Some(bg))
        }
        _ => (None, None),
    };

    let handle = TelemetryHandle {
        inner: Arc::new(Inner {
            envelope,
            inspection,
            state: std::sync::Mutex::new(state.clone()),
            sink_params,
            sender: std::sync::Mutex::new(sender),
            dropped: AtomicUsize::new(0),
            bg: std::sync::Mutex::new(bg_handle),
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
/// runtime handle. Shared by `init` (Enabled-at-start) and
/// [`TelemetryHandle::enable`] (Unknown→Enabled at runtime).
fn spawn_sink(p: &SinkParams) -> (mpsc::Sender<EventPayload>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<EventPayload>(p.channel_capacity);
    let bg = p.runtime.spawn(run_background(BackgroundCtx {
        rx,
        client: p.client.clone(),
        endpoint: p.endpoint.clone(),
        api_key: p.api_key.clone(),
        envelope: p.envelope.clone(),
        batch_size: p.batch_size,
        flush_interval: p.flush_interval,
        post_timeout: p.post_timeout,
        inspection: p.inspection.clone(),
    }));
    (tx, bg)
}

struct BackgroundCtx {
    rx: mpsc::Receiver<EventPayload>,
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

/// One buffered event awaiting flush. We carry the wire JSON and the
/// `InspectedEvent` skeleton side by side so the inspection-log row
/// can be finalised with the actual POST outcome once `flush`
/// completes — no second build pass, no timestamp drift between the
/// wire and the local audit trail.
struct Pending {
    wire: Value,
    inspected: InspectedEvent,
}

async fn run_background(mut ctx: BackgroundCtx) {
    let mut buf: Vec<Pending> = Vec::with_capacity(ctx.batch_size);
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
                        buf.push(build_pending(&ctx.envelope, payload));
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

/// Whether a shutdown drain `budget` is too short to let an in-flight
/// POST (up to `post_timeout`) finish and write its inspection-log row.
/// The budget must be strictly greater than the per-POST timeout.
fn drain_budget_too_short(budget: Duration, post_timeout: Duration) -> bool {
    budget <= post_timeout
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

async fn flush(ctx: &BackgroundCtx, buf: &mut Vec<Pending>) {
    if buf.is_empty() {
        return;
    }
    let wires: Vec<Value> = buf.iter().map(|p| p.wire.clone()).collect();
    let result = post_batch(
        &ctx.client,
        &ctx.endpoint,
        &ctx.api_key,
        &wires,
        ctx.post_timeout,
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
    if let Some(log) = ctx.inspection.as_ref() {
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

#[cfg(test)]
mod file_config_merge_tests {
    use super::FileTelemetryConfig;

    fn cfg(enabled: Option<bool>, endpoint: Option<&str>) -> FileTelemetryConfig {
        FileTelemetryConfig {
            enabled,
            endpoint: endpoint.map(String::from),
            api_key: None,
        }
    }

    #[test]
    fn enabled_false_from_base_survives_overlay_true() {
        let merged = cfg(Some(false), None).merged_over(cfg(Some(true), None));
        assert_eq!(merged.enabled, Some(false), "kill switch must AND-merge");
    }

    #[test]
    fn enabled_false_from_overlay_wins() {
        let merged = cfg(Some(true), None).merged_over(cfg(Some(false), None));
        assert_eq!(merged.enabled, Some(false));
    }

    #[test]
    fn overlay_wins_for_non_kill_switch_fields() {
        let merged = cfg(None, Some("https://base/")).merged_over(cfg(Some(true), Some("https://over/")));
        assert_eq!(merged.enabled, Some(true), "overlay sets enabled when base is silent");
        assert_eq!(merged.endpoint.as_deref(), Some("https://over/"));
    }

    #[test]
    fn base_fields_survive_silent_overlay() {
        let merged = cfg(Some(true), Some("https://base/")).merged_over(cfg(None, None));
        assert_eq!(merged.enabled, Some(true));
        assert_eq!(merged.endpoint.as_deref(), Some("https://base/"));
    }
}

#[cfg(test)]
mod drain_budget_tests {
    use super::drain_budget_too_short;
    use std::time::Duration;

    #[test]
    fn budget_must_exceed_post_timeout() {
        let pt = Duration::from_millis(1500);
        // Too short: equal or less than the per-POST timeout.
        assert!(drain_budget_too_short(Duration::from_millis(1500), pt));
        assert!(drain_budget_too_short(Duration::from_millis(1000), pt));
        // Adequate: strictly greater (the documented 1.5s/2s pairing).
        assert!(!drain_budget_too_short(Duration::from_millis(2000), pt));
    }
}
