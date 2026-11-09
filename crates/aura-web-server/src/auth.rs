//! HTTP API-key authentication for the web server.
//!
//! Built from the `[auth]` table of any loaded agent config (see
//! [`aura_config::AuthConfig`]). When no config defines `[auth]` (or every
//! table sets `mode = "off"`), the middleware is a pass-through and the
//! server behaves exactly as before.
//!
//! When enabled, every route except the open set (`/health`,
//! `/.well-known/agent-card.json`, and `/v1/approvals/*`, which carries its
//! own HMAC posture) requires a `Authorization: Bearer <key>` (or
//! `x-api-key`) header whose SHA-256 matches a configured key hash. On
//! success an [`AuthContext`] is attached to the request so handlers can
//! enforce per-key agent allowlists and namespace sessions per principal.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::warn;

use aura_config::{AuthMode, Config, OidcConfig};

use crate::types::AppState;

/// The authenticated principal for one request. Inserted into request
/// extensions by [`require_auth`] when authentication is enabled.
#[derive(Debug, Clone)]
pub struct AuthContext {
    /// The `[[auth.keys]]` name that matched.
    pub name: String,
    /// Agent names/aliases this key may invoke; `"*"` allows all.
    pub allowed_agents: Vec<String>,
}

impl AuthContext {
    /// Whether this principal may invoke the agent identified by `name`
    /// (with its optional alias). Entries are matched against both, so a
    /// config may list an agent either way; `"*"` allows all.
    pub fn allows(&self, name: &str, alias: Option<&str>) -> bool {
        self.allowed_agents
            .iter()
            .any(|a| a == "*" || a == name || alias.is_some_and(|alias| a == alias))
    }
}

tokio::task_local! {
    /// The authenticated principal for the current request task, set by
    /// [`require_auth`]. Read by the A2A request-handler wrapper, which runs
    /// in the request task. NOTE: the a2a framework spawns execution into a
    /// fresh tokio task, so this does NOT reach the executor — the wrapper
    /// forwards the principal through `service_params` instead (see
    /// `a2a::request_handler`).
    static CURRENT_AUTH: AuthContext;
}

/// The authenticated principal for the current request task, when auth is
/// enabled and the request authenticated.
pub fn current_auth() -> Option<AuthContext> {
    CURRENT_AUTH.try_with(|ctx| ctx.clone()).ok()
}

/// Run `fut` with `ctx` installed as the current principal. Used by the
/// middleware and by tests that need to simulate an authenticated scope.
pub async fn with_current_auth<F: std::future::Future>(ctx: AuthContext, fut: F) -> F::Output {
    CURRENT_AUTH.scope(ctx, fut).await
}

/// Namespace a client-supplied session id under the authenticated principal
/// so two callers that share or guess the same client-side id cannot share
/// conversation state, skill logs, or orchestration history.
///
/// The prefix is a hash of the principal (not the raw name): OIDC
/// `principal_claim` values are IdP-controlled and may contain the delimiter,
/// which a name-based prefix could not disambiguate. Idempotent — the
/// namespaced id echoed back to the client (e.g. `X-Chat-Session-Id`) stays
/// stable when the client sends it back on the next turn.
pub fn namespace_session(principal: &str, session_id: &str) -> String {
    let prefix = session_prefix(principal);
    if session_id.starts_with(&prefix) {
        session_id.to_string()
    } else {
        format!("{prefix}{session_id}")
    }
}

fn session_prefix(principal: &str) -> String {
    use std::fmt::Write;
    let digest = sha256(principal.as_bytes());
    let mut out = String::with_capacity(17);
    for byte in &digest[..8] {
        let _ = write!(out, "{byte:02x}");
    }
    out.push(':');
    out
}

/// One keyring entry: a principal and the 32-byte SHA-256 digests it accepts
/// (current plus, during rotation, previous).
#[derive(Debug)]
struct KeyEntry {
    name: String,
    hashes: Vec<[u8; 32]>,
    allowed_agents: Vec<String>,
}

/// The compiled keyring, built once at startup from all loaded configs.
#[derive(Debug)]
pub struct KeyRing {
    entries: Vec<KeyEntry>,
}

/// Routes that stay reachable without a key when auth is enabled:
/// - `/health`: container probes must not need credentials.
/// - `/.well-known/agent-card.json`: A2A discovery convention.
/// - `/.well-known/oauth-protected-resource`: RFC 9728 discovery, the
///   target of the 401 challenge's `resource_metadata` parameter — it is
///   by definition what unauthenticated clients need to find.
/// - `/v1/approvals/*`: approvers are a different principal than chat
///   callers; this route keeps its own HMAC verification in the handler.
fn is_open_path(path: &str) -> bool {
    path == "/health"
        || path == "/.well-known/agent-card.json"
        || path == "/.well-known/oauth-protected-resource"
        || path.starts_with("/v1/approvals/")
}

impl KeyRing {
    /// Build the deployment keyring from every loaded config. Keys from
    /// multiple config files are merged into one ring; duplicate principal
    /// names across files are rejected at startup.
    ///
    /// Returns `Ok(None)` when no config enables auth.
    pub fn from_configs(configs: &[Config]) -> Result<Option<Self>, String> {
        let mut entries = Vec::new();
        let mut enabled = false;
        for config in configs {
            let Some(auth) = &config.auth else {
                continue;
            };
            if auth.mode == AuthMode::Off {
                continue;
            }
            enabled = true;
            auth.validate().map_err(|e| e.to_string())?;
            for key in &auth.keys {
                if entries.iter().any(|e: &KeyEntry| e.name == key.name) {
                    return Err(format!(
                        "duplicate [auth.keys] name '{}' across loaded configs",
                        key.name
                    ));
                }
                entries.push(KeyEntry {
                    name: key.name.clone(),
                    hashes: key_hashes(key),
                    allowed_agents: key.agents.clone(),
                });
            }
        }
        if !enabled {
            return Ok(None);
        }
        Ok(Some(Self { entries }))
    }

    /// Number of principals in the ring (for startup logging).
    pub fn principal_count(&self) -> usize {
        self.entries.len()
    }

    /// Hash a raw candidate key and constant-time compare it against every
    /// stored digest. Returns the matching principal on success.
    pub fn authenticate(&self, raw: &str) -> Option<AuthContext> {
        let candidate = sha256(raw.as_bytes());
        for entry in &self.entries {
            for hash in &entry.hashes {
                if ct_eq(hash, &candidate) {
                    return Some(AuthContext {
                        name: entry.name.clone(),
                        allowed_agents: entry.allowed_agents.clone(),
                    });
                }
            }
        }
        None
    }
}

/// The deployment authenticator: exactly one mechanism, chosen by
/// `[auth].mode`. Both arms produce the same [`AuthContext`], so everything
/// downstream (allowlists, session namespacing, span identity) is shared.
#[derive(Debug)]
pub enum Authenticator {
    ApiKeys(KeyRing),
    Oidc(Box<OidcValidator>),
}

/// The result of one authentication attempt. `Unavailable` distinguishes an
/// infrastructure failure (e.g. the IdP is unreachable) from a bad
/// credential, so the middleware can answer 503 instead of a misleading 401.
#[derive(Debug)]
pub enum AuthOutcome {
    Authenticated(AuthContext),
    Rejected(Rejection),
    Unavailable,
}

/// Why a presented credential was rejected. The split matters beyond
/// cosmetics: RFC 6750 clients treat 401 as "discard the cached token and
/// re-authenticate", so a valid-but-unauthorized token (claim gate) must
/// answer 403 `insufficient_scope` — answering 401 would loop a compliant
/// client through login forever.
#[derive(Debug)]
pub enum Rejection {
    /// The credential is malformed, unverifiable, expired, or unknown.
    /// The payload is a safe-to-echo `error_description` (RFC 6750 §3.1).
    InvalidToken(&'static str),
    /// The credential verified, but the `[auth.oidc]` claim gate denied the
    /// principal: authenticated, not authorized.
    InsufficientScope,
}

impl Authenticator {
    async fn authenticate(&self, raw: &str) -> AuthOutcome {
        match self {
            Self::ApiKeys(ring) => match ring.authenticate(raw) {
                Some(ctx) => AuthOutcome::Authenticated(ctx),
                None => AuthOutcome::Rejected(Rejection::InvalidToken("API key not recognized")),
            },
            Self::Oidc(validator) => validator.authenticate(raw).await,
        }
    }

    /// One-line description for the startup log.
    pub fn describe(&self) -> String {
        match self {
            Self::ApiKeys(ring) => format!(
                "API-key auth enabled ({} principal(s))",
                ring.principal_count()
            ),
            Self::Oidc(validator) => format!(
                "OIDC auth enabled (issuer {}, audience {})",
                validator.cfg.issuer, validator.cfg.audience
            ),
        }
    }
}

/// The JWKS cache: the last fetched key set (shared via `Arc` so per-token
/// validation is a refcount bump), the URL it came from (config or
/// discovery), when it was fetched, and when a fetch last failed.
#[derive(Debug, Default)]
struct JwksCache {
    set: Option<Arc<JwkSet>>,
    resolved_url: Option<String>,
    fetched_at: Option<Instant>,
    last_failure: Option<Instant>,
}

/// Why JWKS-backed validation could not proceed. Only one reason today; the
/// enum keeps the call site honest about unavailable-vs-invalid.
#[derive(Debug)]
enum JwksError {
    Unavailable,
}

/// Validates OIDC bearer JWTs: signature against the issuer's JWKS, plus
/// issuer, audience, and lifetime claims. JWKS is fetched lazily, cached,
/// refreshed on the configured interval, and force-refreshed (rate-limited)
/// when a token presents an unknown `kid` so key rotation is picked up.
#[derive(Debug)]
pub struct OidcValidator {
    cfg: OidcConfig,
    http: reqwest::Client,
    cache: tokio::sync::Mutex<JwksCache>,
    /// Serializes JWKS fetches so the (slow, timeout-bound) network call does
    /// not hold the cache lock — requests whose `kid` is already cached never
    /// wait on an in-flight refresh.
    refresh_lock: tokio::sync::Mutex<()>,
}

/// Floor between forced (unknown-`kid`) JWKS refreshes, so a flood of tokens
/// with random kids cannot turn into a flood of IdP requests.
const FORCED_REFRESH_FLOOR: std::time::Duration = std::time::Duration::from_secs(30);

/// Minimum gap between JWKS fetch attempts after a failure, so a down IdP
/// does not turn every request into a fresh (mutex-serialized, timeout-bound)
/// fetch.
const FETCH_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

impl OidcValidator {
    pub fn new(cfg: OidcConfig) -> Self {
        Self {
            cfg,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("default client builder"),
            cache: tokio::sync::Mutex::new(JwksCache::default()),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Fetch the JWKS once so a broken issuer/URL fails at startup rather
    /// than on the first user token, and leave the set cached so the first
    /// real token does not re-fetch.
    pub async fn warm(&self) -> Result<(), String> {
        let (url, set) = self.fetch_jwks().await?;
        let mut cache = self.cache.lock().await;
        cache.resolved_url = Some(url);
        cache.set = Some(Arc::new(set));
        cache.fetched_at = Some(Instant::now());
        cache.last_failure = None;
        Ok(())
    }

    /// The configured issuer, for RFC 9728 resource metadata.
    pub fn issuer(&self) -> &str {
        &self.cfg.issuer
    }

    async fn authenticate(&self, token: &str) -> AuthOutcome {
        let reject = |reason: &'static str| AuthOutcome::Rejected(Rejection::InvalidToken(reason));
        let Some(header) = jsonwebtoken::decode_header(token).ok() else {
            return reject("token is not a valid JWT");
        };
        let jwks = match self.jwks_for_kid(header.kid.as_deref()).await {
            Ok(jwks) => jwks,
            Err(JwksError::Unavailable) => return AuthOutcome::Unavailable,
        };
        let Some(jwk) = select_jwk(&jwks, header.kid.as_deref()) else {
            return reject("no signing key matches the token key id");
        };
        let Some(algorithm) = jwk_algorithm(jwk) else {
            return reject("signing key uses an unsupported algorithm");
        };
        let Ok(key) = DecodingKey::from_jwk(jwk) else {
            return reject("signing key could not be parsed");
        };

        let mut validation = Validation::new(algorithm);
        validation.set_issuer(&[&self.cfg.issuer]);
        validation.set_audience(&[&self.cfg.audience]);
        validation.leeway = self.cfg.clock_skew_secs;
        // jsonwebtoken defaults `validate_nbf` to false; the config docs
        // promise `exp`/`nbf` enforcement, so opt in explicitly.
        validation.validate_nbf = true;
        // Presence is only enforced through `required_spec_claims`, whose
        // default is `exp` alone: without this, a token omitting `aud` or
        // `iss` entirely skips those checks and authenticates against any
        // deployment trusting that issuer (Keycloak clients without an
        // audience mapper do exactly this). The call replaces the set, so
        // `exp` must stay listed.
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);

        let claims = match jsonwebtoken::decode::<serde_json::Map<String, serde_json::Value>>(
            token,
            &key,
            &validation,
        ) {
            Ok(data) => data.claims,
            Err(e) => {
                // Safe-to-echo distinction for the challenge's
                // error_description: expired tells a compliant client to
                // refresh its token; anything else means re-login.
                use jsonwebtoken::errors::ErrorKind;
                let reason = match e.kind() {
                    ErrorKind::ExpiredSignature => "token has expired",
                    ErrorKind::ImmatureSignature => "token is not yet valid",
                    ErrorKind::MissingRequiredClaim(_) => "token is missing a required claim",
                    ErrorKind::InvalidIssuer | ErrorKind::InvalidAudience => {
                        "token failed issuer or audience validation"
                    }
                    _ => "token failed signature or claims validation",
                };
                return reject(reason);
            }
        };

        if !self.claim_gate_passes(&claims) {
            warn!(
                sub = claims.get("sub").and_then(|v| v.as_str()).unwrap_or("?"),
                "OIDC token rejected by [auth.oidc] claim gate"
            );
            // Authenticated but not authorized: 403, never 401 — a client
            // that re-authenticates on 401 would loop forever.
            return AuthOutcome::Rejected(Rejection::InsufficientScope);
        }

        let Some(name) = claims
            .get(&self.cfg.principal_claim)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            return reject("token has no usable principal claim");
        };
        AuthOutcome::Authenticated(AuthContext {
            name: name.to_string(),
            allowed_agents: self.allowed_agents_for(&claims),
        })
    }

    /// The optional `require_claim` gate: with values configured the claim
    /// must equal one of them (string claim) or contain one (array claim);
    /// without values the claim must merely be present and non-empty.
    fn claim_gate_passes(&self, claims: &serde_json::Map<String, serde_json::Value>) -> bool {
        let Some(claim) = &self.cfg.require_claim else {
            return true;
        };
        let Some(strings) = claim_strings(claims, claim) else {
            return false;
        };
        if self.cfg.require_claim_values.is_empty() {
            // "Present and non-empty": an IdP may emit `""` or `[""]` for
            // missing membership; those must not satisfy the gate.
            return strings.iter().any(|s| !s.is_empty());
        }
        strings
            .iter()
            .any(|s| self.cfg.require_claim_values.iter().any(|w| w == s))
    }

    /// The agents a token may invoke: the union of every matching
    /// `claim_agents` rule (deduplicated, first-seen order), or the global
    /// `allowed_agents` when no rule matches. Matching a rule never bypasses
    /// the claim gate — the gate runs first.
    fn allowed_agents_for(
        &self,
        claims: &serde_json::Map<String, serde_json::Value>,
    ) -> Vec<String> {
        let mut union: Vec<String> = Vec::new();
        let mut matched = false;
        for rule in &self.cfg.claim_agents {
            let Some(strings) = claim_strings(claims, &rule.claim) else {
                continue;
            };
            if strings.iter().any(|s| *s == rule.value) {
                matched = true;
                for agent in &rule.agents {
                    if !union.contains(agent) {
                        union.push(agent.clone());
                    }
                }
            }
        }
        if matched {
            union
        } else {
            self.cfg.allowed_agents.clone()
        }
    }

    /// The cached key set, refreshed when absent, stale, or (rate-limited)
    /// when `kid` is unknown to the current set. A failed refresh never
    /// invalidates a cached set: stale keys beat no keys, and the failure is
    /// recorded so subsequent requests back off instead of serializing every
    /// caller behind a failing fetch. The fetch runs under `refresh_lock`,
    /// not the cache lock, so cached-key requests never queue behind a slow
    /// or timing-out IdP.
    async fn jwks_for_kid(&self, kid: Option<&str>) -> Result<Arc<JwkSet>, JwksError> {
        if self.refresh_needed(kid).await {
            // Serialize refreshes, then re-check: whoever won the race may
            // have completed the refresh while we waited.
            let _refresh = self.refresh_lock.lock().await;
            if self.refresh_needed(kid).await {
                self.refresh_once().await;
            }
        }
        self.cached_set().await.ok_or(JwksError::Unavailable)
    }

    /// Whether the cached set is unusable for `kid` and a fetch should be
    /// attempted, subject to the failure backoff.
    async fn refresh_needed(&self, kid: Option<&str>) -> bool {
        let cache = self.cache.lock().await;
        let refresh_interval = Duration::from_secs(self.cfg.jwks_refresh_secs);
        let stale = cache
            .fetched_at
            .is_none_or(|t| t.elapsed() >= refresh_interval);
        let kid_missing = match (kid, &cache.set) {
            (Some(kid), Some(set)) => set.find(kid).is_none(),
            _ => false,
        };
        let forced_floor = FORCED_REFRESH_FLOOR.min(refresh_interval);
        let forced_floor_ok = cache.fetched_at.is_none_or(|t| t.elapsed() >= forced_floor);
        let backoff_ok = cache
            .last_failure
            .is_none_or(|t| t.elapsed() >= FETCH_BACKOFF);
        backoff_ok && (cache.set.is_none() || stale || (kid_missing && forced_floor_ok))
    }

    /// Fetch the JWKS and update the cache. A failure records the attempt for
    /// the backoff but never clears a cached set; with nothing cached,
    /// callers see [`JwksError::Unavailable`] via [`Self::jwks_for_kid`].
    async fn refresh_once(&self) {
        match self.fetch_jwks().await {
            Ok((url, set)) => {
                let mut cache = self.cache.lock().await;
                cache.resolved_url = Some(url);
                cache.set = Some(Arc::new(set));
                cache.fetched_at = Some(Instant::now());
                cache.last_failure = None;
            }
            Err(e) => {
                let mut cache = self.cache.lock().await;
                cache.last_failure = Some(Instant::now());
                if cache.set.is_none() {
                    warn!("JWKS unavailable and nothing cached yet: {e}");
                } else {
                    warn!("JWKS refresh failed, serving stale key set: {e}");
                }
            }
        }
    }

    async fn cached_set(&self) -> Option<Arc<JwkSet>> {
        self.cache.lock().await.set.clone()
    }

    /// Resolve the JWKS URL (explicit config, else OIDC discovery) and fetch
    /// the key set.
    async fn fetch_jwks(&self) -> Result<(String, JwkSet), String> {
        let url = match &self.cfg.jwks_url {
            Some(url) => url.clone(),
            None => self.discover_jwks_url().await?,
        };
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("JWKS fetch from {url} failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("JWKS fetch from {url} returned {status}"));
        }
        let set: JwkSet = response
            .json()
            .await
            .map_err(|e| format!("JWKS from {url} is not a valid key set: {e}"))?;
        Ok((url, set))
    }

    async fn discover_jwks_url(&self) -> Result<String, String> {
        let base = self.cfg.issuer.trim_end_matches('/');
        let url = format!("{base}/.well-known/openid-configuration");
        let doc: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("OIDC discovery from {url} failed: {e}"))?
            .error_for_status()
            .map_err(|e| format!("OIDC discovery from {url} failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("OIDC discovery document at {url} is not JSON: {e}"))?;
        doc.get("jwks_uri")
            .and_then(|v| v.as_str())
            .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
            .map(str::to_string)
            .ok_or_else(|| format!("discovery document at {url} has no usable jwks_uri"))
    }
}

/// The string values of a claim: a string claim yields one value, an array
/// claim its string items, and anything else (or absence) yields `None` —
/// an unusable claim, distinct from an empty array.
fn claim_strings<'a>(
    claims: &'a serde_json::Map<String, serde_json::Value>,
    claim: &str,
) -> Option<Vec<&'a str>> {
    match claims.get(claim)? {
        serde_json::Value::String(s) => Some(vec![s.as_str()]),
        serde_json::Value::Array(items) => Some(items.iter().filter_map(|v| v.as_str()).collect()),
        _ => None,
    }
}

/// Pick the JWK matching `kid`; without a `kid` header, accept the set only
/// when it holds exactly one key (no guessing).
fn select_jwk<'a>(jwks: &'a JwkSet, kid: Option<&str>) -> Option<&'a jsonwebtoken::jwk::Jwk> {
    match kid {
        Some(kid) => jwks.find(kid),
        None if jwks.keys.len() == 1 => jwks.keys.first(),
        None => None,
    }
}

/// The signature algorithm to validate with, taken from the (trusted) JWK
/// rather than the token header, restricted to asymmetric RS/EC algs —
/// symmetric `oct` keys must never authenticate bearer JWTs.
fn jwk_algorithm(jwk: &jsonwebtoken::jwk::Jwk) -> Option<Algorithm> {
    use jsonwebtoken::jwk::KeyAlgorithm;
    let declared = jwk.common.key_algorithm.as_ref().and_then(|a| match a {
        KeyAlgorithm::RS256 => Some(Algorithm::RS256),
        KeyAlgorithm::RS384 => Some(Algorithm::RS384),
        KeyAlgorithm::RS512 => Some(Algorithm::RS512),
        KeyAlgorithm::ES256 => Some(Algorithm::ES256),
        KeyAlgorithm::ES384 => Some(Algorithm::ES384),
        _ => None,
    });
    let inferred = match &jwk.algorithm {
        // RS256 is the conventional default for bare RSA JWKs; EC keys have
        // no such default — the algorithm must follow the curve, or a P-384
        // key without `alg` would reject its own valid ES384 tokens.
        AlgorithmParameters::RSA(_) => Some(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(params) => match params.curve {
            jsonwebtoken::jwk::EllipticCurve::P256 => Some(Algorithm::ES256),
            jsonwebtoken::jwk::EllipticCurve::P384 => Some(Algorithm::ES384),
            // P-521 (ES512) and Ed25519 are not supported by jsonwebtoken.
            _ => None,
        },
        _ => None,
    };
    asymmetric_algorithm(declared.or(inferred)?)
}

fn asymmetric_algorithm(alg: Algorithm) -> Option<Algorithm> {
    match alg {
        Algorithm::RS256
        | Algorithm::RS384
        | Algorithm::RS512
        | Algorithm::ES256
        | Algorithm::ES384 => Some(alg),
        _ => None,
    }
}

fn key_hashes(key: &aura_config::ApiKeyConfig) -> Vec<[u8; 32]> {
    // Config validation already guarantees both decode; re-decode defensively
    // and skip anything malformed rather than panic at request time.
    let mut hashes = Vec::with_capacity(2);
    if let Ok(h) = aura_config::auth::decode_key_hash(&key.key_hash) {
        hashes.push(h);
    }
    if let Some(prev) = &key.key_hash_previous
        && let Ok(h) = aura_config::auth::decode_key_hash(prev)
    {
        hashes.push(h);
    }
    hashes
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Length-then-XOR constant-time comparison. Both sides are fixed-size
/// digests, so the length branch never leaks.
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// OpenAI-compatible 401/403 error envelope so OpenAI clients surface the
/// problem instead of choking on an unknown shape.
#[derive(Serialize)]
struct AuthErrorEnvelope {
    error: AuthErrorDetail,
}

#[derive(Serialize)]
struct AuthErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
    param: Option<String>,
    code: String,
}

fn auth_error(status: StatusCode, message: &str, code: &str) -> Response {
    (
        status,
        Json(AuthErrorEnvelope {
            error: AuthErrorDetail {
                message: message.to_string(),
                error_type: "authentication_error".to_string(),
                param: None,
                code: code.to_string(),
            },
        }),
    )
        .into_response()
}

/// Extract the candidate key from `Authorization: Bearer` (the form every
/// OpenAI-compatible client sends) or the `x-api-key` fallback used by some
/// gateways.
fn candidate_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) {
        let value = auth.to_str().ok()?;
        let (scheme, rest) = value.split_once(' ')?;
        if scheme.eq_ignore_ascii_case("bearer") && !rest.trim().is_empty() {
            return Some(rest.trim());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// The canonical origin this server believes it is reached at, for RFC 9728
/// metadata and challenges: forwarded headers win (deployments sit behind
/// proxies), else localhost is assumed http and everything else https.
fn canonical_origin(headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(axum::http::header::HOST))
        .and_then(|v| v.to_str().ok())?
        .split(',')
        .next()?
        .trim()
        .to_string();
    if host.is_empty() {
        return None;
    }
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| *s == "http" || *s == "https")
        .map(str::to_string)
        .unwrap_or_else(|| {
            let bare = host.split(':').next().unwrap_or("");
            if matches!(bare, "localhost" | "127.0.0.1" | "[::1]" | "::1") {
                "http".to_string()
            } else {
                "https".to_string()
            }
        });
    Some(format!("{scheme}://{host}"))
}

/// RFC 6750 §3 / RFC 9728 §3.1 `WWW-Authenticate` challenge. With OIDC
/// configured, advertises this server's protected-resource metadata so
/// OAuth-aware clients (MCP, spec-compliant CLIs) can discover the
/// authorization server without prior knowledge.
fn bearer_challenge(origin: Option<&str>) -> String {
    match origin {
        Some(origin) => format!(
            "Bearer realm=\"aura\", resource_metadata=\"{origin}/.well-known/oauth-protected-resource\""
        ),
        None => "Bearer realm=\"aura\"".to_string(),
    }
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Middleware: reject unauthenticated requests when the deployment has an
/// `[auth]` authenticator. Runs inside `shutdown_guard` so draining still
/// answers 503 before auth challenges.
///
/// Responses follow RFC 6750 §3.1 alongside the OpenAI-shaped JSON body:
/// a request with no credential gets a bare challenge; a request whose
/// credential failed gets `error="invalid_token"` with a safe description;
/// a verified credential denied by the claim gate gets 403
/// `insufficient_scope` (re-authenticating cannot fix it); and an
/// unreachable IdP gets 503 with `Retry-After` and no challenge, since the
/// caller's credentials are not the problem.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(authenticator) = &state.auth else {
        return next.run(request).await;
    };
    if is_open_path(request.uri().path()) {
        return next.run(request).await;
    }
    let presented = candidate_key(request.headers()).is_some();
    let outcome = match candidate_key(request.headers()) {
        Some(key) => authenticator.authenticate(key).await,
        None => AuthOutcome::Rejected(Rejection::InvalidToken("no credential presented")),
    };
    let oidc = matches!(authenticator.as_ref(), Authenticator::Oidc(_));
    let origin = oidc.then(|| canonical_origin(request.headers())).flatten();
    let challenge = bearer_challenge(origin.as_deref());
    match outcome {
        AuthOutcome::Authenticated(ctx) => {
            // The task-local makes the principal reachable from code paths
            // the axum extensions cannot reach — notably the A2A executor,
            // which runs deep inside the a2a framework's handler.
            request.extensions_mut().insert(ctx.clone());
            CURRENT_AUTH.scope(ctx, next.run(request)).await
        }
        AuthOutcome::Unavailable => {
            warn!(
                path = request.uri().path(),
                "identity provider unreachable; rejecting request"
            );
            // No challenge: the caller's credential is not at fault, and a
            // client that re-logins on a bearer challenge would loop
            // against the down IdP.
            let mut response = auth_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The identity provider is unreachable; the request could not be authenticated. Retry shortly.",
                "identity_provider_unreachable",
            );
            if let Ok(value) = HeaderValue::from_str(&FETCH_BACKOFF.as_secs().to_string()) {
                response
                    .headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, value);
            }
            response
        }
        AuthOutcome::Rejected(Rejection::InsufficientScope) => {
            warn!(
                path = request.uri().path(),
                "authenticated principal denied by [auth.oidc] claim gate"
            );
            let mut response = auth_error(
                StatusCode::FORBIDDEN,
                "The token is valid but does not satisfy this deployment's access policy.",
                "insufficient_scope",
            );
            if let Ok(value) =
                HeaderValue::from_str(&format!("{challenge}, error=\"insufficient_scope\""))
            {
                response
                    .headers_mut()
                    .insert(axum::http::header::WWW_AUTHENTICATE, value);
            }
            response
        }
        AuthOutcome::Rejected(Rejection::InvalidToken(reason)) => {
            warn!(
                path = request.uri().path(),
                "rejected unauthenticated request"
            );
            // RFC 6750 §3.1: no credential presented gets the bare
            // challenge; a rejected one names the error and a safe
            // description.
            let challenge = if presented {
                format!(
                    "{challenge}, error=\"invalid_token\", error_description={}",
                    quote(reason)
                )
            } else {
                challenge
            };
            let mut response = match authenticator.as_ref() {
                Authenticator::ApiKeys(_) => auth_error(
                    StatusCode::UNAUTHORIZED,
                    "Invalid API key provided. Provide your key via the `Authorization: Bearer` header.",
                    "invalid_api_key",
                ),
                Authenticator::Oidc(_) => auth_error(
                    StatusCode::UNAUTHORIZED,
                    "Invalid or expired bearer token. Provide a valid OIDC access token via the `Authorization: Bearer` header.",
                    "invalid_token",
                ),
            };
            if let Ok(value) = HeaderValue::from_str(&challenge) {
                response
                    .headers_mut()
                    .insert(axum::http::header::WWW_AUTHENTICATE, value);
            }
            response
        }
    }
}

/// RFC 9728 protected resource metadata: lets an OAuth-aware client
/// discover the authorization server(s) and expected audience from a
/// 401's `resource_metadata` challenge. Served only in OIDC mode; api-keys
/// deployments have no authorization server to discover.
pub async fn protected_resource_metadata(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let Some(Authenticator::Oidc(validator)) = state.auth.as_deref() else {
        return auth_error(
            StatusCode::NOT_FOUND,
            "This server does not use an OIDC authorization server.",
            "not_found",
        );
    };
    let Some(origin) = canonical_origin(&headers) else {
        return auth_error(
            StatusCode::BAD_REQUEST,
            "Cannot determine the canonical resource URI from request headers.",
            "invalid_request",
        );
    };
    Json(serde_json::json!({
        "resource": origin,
        "authorization_servers": [validator.issuer()],
        "bearer_methods_supported": ["header"],
    }))
    .into_response()
}

/// The OpenAI-compatible 403 body for a key that is valid but not allowed to
/// invoke the requested agent.
pub fn forbidden_response(key_name: &str, agent_id: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(AuthErrorEnvelope {
            error: AuthErrorDetail {
                message: format!(
                    "The API key `{key_name}` does not have access to the model `{agent_id}`."
                ),
                error_type: "permission_error".to_string(),
                param: Some("model".to_string()),
                code: "model_not_found".to_string(),
            },
        }),
    )
        .into_response()
}

/// Compile the `[auth]` tables of all loaded configs into the deployment
/// authenticator, warning once about mixed-mode setups. Returns `None` when
/// no loaded config enables auth. Modes are
/// deployment-wide and mutually exclusive: a server is either api-keys or
/// oidc, never both.
pub fn build_authenticator(configs: &[Config]) -> Result<Option<Arc<Authenticator>>, String> {
    let modes: Vec<AuthMode> = configs
        .iter()
        .filter_map(|c| c.auth.as_ref().map(|a| a.mode))
        .collect();
    let enabled_modes: Vec<AuthMode> = modes
        .iter()
        .copied()
        .filter(|m| *m != AuthMode::Off)
        .collect();
    if enabled_modes.contains(&AuthMode::ApiKeys) && enabled_modes.contains(&AuthMode::Oidc) {
        return Err(
            "cannot mix [auth] mode = \"api-keys\" and mode = \"oidc\" across loaded configs; \
             pick one mechanism per server"
                .to_string(),
        );
    }
    if modes.contains(&AuthMode::Off) && !enabled_modes.is_empty() {
        warn!(
            "some configs set [auth] mode = \"off\" while others enable auth; \
             auth is enabled for the whole server"
        );
    }
    if enabled_modes.first() == Some(&AuthMode::Oidc) {
        let mut seen = 0usize;
        for config in configs {
            // Only configs that *enable* OIDC contribute settings: a config
            // with `mode = "off"` and a leftover `[auth.oidc]` table passes
            // validation, and must not shadow the enabled config's issuer,
            // audience, claim gate, or agent allowlist.
            let Some(oidc) = config
                .auth
                .as_ref()
                .filter(|a| a.mode == AuthMode::Oidc)
                .and_then(|a| a.oidc.clone())
            else {
                continue;
            };
            if seen == 0 {
                // The config crate allows plaintext http (a local test IdP
                // is legitimate); flag non-local plaintext here, where an
                // operator is watching.
                warn_if_plaintext(&oidc.issuer, "issuer");
                if let Some(url) = &oidc.jwks_url {
                    warn_if_plaintext(url, "jwks_url");
                }
            }
            seen += 1;
            if seen == 1 {
                return Ok(Some(Arc::new(Authenticator::Oidc(Box::new(
                    OidcValidator::new(oidc),
                )))));
            }
            warn!(
                "multiple configs define [auth.oidc]; using the first issuer ({}) and \
                 ignoring the rest",
                oidc.issuer
            );
        }
        return Err("mode = \"oidc\" but no [auth.oidc] table found".to_string());
    }
    KeyRing::from_configs(configs).map(|ring| ring.map(|r| Arc::new(Authenticator::ApiKeys(r))))
}

/// Warn when an OIDC endpoint travels over plaintext http off-localhost.
fn warn_if_plaintext(url: &str, field: &str) {
    let Some(rest) = url.strip_prefix("http://") else {
        return;
    };
    let host = rest
        .split(['/', '?'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    if !matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        warn!(
            "[auth.oidc].{field} uses plaintext http ({url}); tokens and key \
             material cross the network unencrypted — use https in production"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::ToolResultMode;
    use crate::types::{ActiveRequestTracker, AppState};
    use aura_config::AuthConfig;
    use axum::Router;
    use axum::routing::get;
    use tower::ServiceExt;

    const RAW_KEY: &str = "aura-test-key-0001";

    fn auth_config(mode: AuthMode, keys: Vec<aura_config::ApiKeyConfig>) -> AuthConfig {
        AuthConfig {
            mode,
            keys,
            oidc: None,
        }
    }

    fn key(name: &str, raw: &str, agents: Vec<String>) -> aura_config::ApiKeyConfig {
        aura_config::ApiKeyConfig {
            name: name.to_string(),
            key_hash: aura_config::hash_api_key(raw),
            key_hash_previous: None,
            agents,
        }
    }

    fn config_with_auth(auth: Option<AuthConfig>) -> Config {
        Config {
            auth,
            ..Config::default()
        }
    }

    pub(super) fn test_state(auth: Option<Arc<Authenticator>>) -> Arc<AppState> {
        Arc::new(AppState {
            configs: Arc::new(vec![]),
            tool_result_mode: ToolResultMode::default(),
            tool_result_max_length: 0,
            streaming_buffer_size: 0,
            aura_custom_events: false,
            aura_emit_reasoning: false,
            debug_provider_errors: false,
            streaming_timeout_secs: 0,
            first_chunk_timeout_secs: 0,
            stream_inactivity_timeout_secs: 0,
            shutdown_token: tokio_util::sync::CancellationToken::new(),
            stream_shutdown_token: tokio_util::sync::CancellationToken::new(),
            active_requests: Arc::new(ActiveRequestTracker::default()),
            default_agent: None,
            additional_tools: Arc::new(Vec::new),
            slack_api: None,
            pending_approvals: aura::hitl::PendingApprovals::new(),
            hitl_webhook_hmac: None,
            session_store: Arc::new(crate::session_store::InMemorySessionStore::new()),
            auth,
        })
    }

    /// Minimal app: middleware + a probe route that echoes the principal.
    pub(super) fn app_with_auth(state: Arc<AppState>) -> Router {
        async fn probe(ext: Option<axum::Extension<AuthContext>>) -> String {
            ext.map(|e| e.name.clone())
                .unwrap_or_else(|| "anonymous".to_string())
        }
        Router::new()
            .route("/v1/models", get(probe))
            .route("/health", get(probe))
            .route("/v1/approvals/{id}", get(probe))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_auth,
            ))
            .with_state(state)
    }

    pub(super) fn request(path: &str, headers: &[(&str, String)]) -> Request {
        let mut builder = axum::http::Request::builder().method("GET").uri(path);
        for (k, v) in headers {
            builder = builder.header(*k, v);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn no_auth_table_disables_the_ring() {
        let configs = vec![config_with_auth(None)];
        assert!(build_authenticator(&configs).unwrap().is_none());
        let configs = vec![config_with_auth(Some(auth_config(AuthMode::Off, vec![])))];
        assert!(build_authenticator(&configs).unwrap().is_none());
    }

    #[test]
    fn authenticates_current_and_previous_hashes() {
        let mut k = key("ops", RAW_KEY, vec!["*".to_string()]);
        k.key_hash_previous = Some(aura_config::hash_api_key("old-key"));
        let ring = KeyRing {
            entries: vec![KeyEntry {
                name: k.name.clone(),
                hashes: key_hashes(&k),
                allowed_agents: k.agents.clone(),
            }],
        };
        assert_eq!(ring.authenticate(RAW_KEY).unwrap().name, "ops");
        assert_eq!(ring.authenticate("old-key").unwrap().name, "ops");
        assert!(ring.authenticate("wrong").is_none());
        assert!(ring.authenticate("").is_none());
    }

    #[test]
    fn allowlist_matching() {
        let ctx = AuthContext {
            name: "k".into(),
            allowed_agents: vec!["incident-response".into()],
        };
        assert!(ctx.allows("incident-response", None));
        assert!(!ctx.allows("other", None));
        // Name and alias both match.
        assert!(ctx.allows("underlying-name", Some("incident-response")));
        assert!(!ctx.allows("underlying-name", None));
        let all = AuthContext {
            name: "k".into(),
            allowed_agents: vec!["*".into()],
        };
        assert!(all.allows("anything", None));
    }

    #[test]
    fn duplicate_names_across_configs_rejected() {
        let configs = vec![
            config_with_auth(Some(auth_config(
                AuthMode::ApiKeys,
                vec![key("dup", RAW_KEY, vec!["*".into()])],
            ))),
            config_with_auth(Some(auth_config(
                AuthMode::ApiKeys,
                vec![key("dup", "other-raw", vec!["*".into()])],
            ))),
        ];
        let err = build_authenticator(&configs).unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[tokio::test]
    async fn middleware_disabled_passes_through() {
        let state = test_state(None);
        let resp = app_with_auth(state)
            .oneshot(request("/v1/models", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn middleware_rejects_missing_and_wrong_keys() {
        let ring = build_authenticator(&[config_with_auth(Some(auth_config(
            AuthMode::ApiKeys,
            vec![key("ops", RAW_KEY, vec!["*".into()])],
        )))])
        .unwrap()
        .unwrap();
        let state = test_state(Some(ring));
        let app = app_with_auth(state);

        let resp = app
            .clone()
            .oneshot(request("/v1/models", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let resp = app
            .clone()
            .oneshot(request(
                "/v1/models",
                &[("authorization", "Bearer nope".to_string())],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // Non-bearer schemes are not accepted.
        let resp = app
            .oneshot(request(
                "/v1/models",
                &[("authorization", format!("Basic {RAW_KEY}"))],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn challenges_follow_rfc_6750() {
        let ring = build_authenticator(&[config_with_auth(Some(auth_config(
            AuthMode::ApiKeys,
            vec![key("ops", RAW_KEY, vec!["*".into()])],
        )))])
        .unwrap()
        .unwrap();
        let app = app_with_auth(test_state(Some(ring)));

        // No credential presented: bare challenge, no error parameter
        // (RFC 6750 section 3.1).
        let resp = app
            .clone()
            .oneshot(request("/v1/models", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()
                .get("www-authenticate")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer realm=\"aura\""
        );

        // Credential presented but rejected: names the error with a safe
        // description. api-keys mode has no authorization server, so no
        // resource_metadata parameter.
        let resp = app
            .oneshot(request(
                "/v1/models",
                &[("authorization", "Bearer nope".to_string())],
            ))
            .await
            .unwrap();
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(challenge.contains("error=\"invalid_token\""), "{challenge}");
        assert!(challenge.contains("error_description="), "{challenge}");
        assert!(!challenge.contains("resource_metadata"), "{challenge}");
    }

    #[tokio::test]
    async fn middleware_accepts_bearer_and_x_api_key() {
        let ring = build_authenticator(&[config_with_auth(Some(auth_config(
            AuthMode::ApiKeys,
            vec![key("ops", RAW_KEY, vec!["*".into()])],
        )))])
        .unwrap()
        .unwrap();
        let state = test_state(Some(ring));
        let app = app_with_auth(state);

        let resp = app
            .clone()
            .oneshot(request(
                "/v1/models",
                &[("authorization", format!("Bearer {RAW_KEY}"))],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(request("/v1/models", &[("x-api-key", RAW_KEY.to_string())]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn open_paths_stay_reachable() {
        let ring = build_authenticator(&[config_with_auth(Some(auth_config(
            AuthMode::ApiKeys,
            vec![key("ops", RAW_KEY, vec!["*".into()])],
        )))])
        .unwrap()
        .unwrap();
        let state = test_state(Some(ring));
        let app = app_with_auth(state);

        let resp = app.clone().oneshot(request("/health", &[])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(request("/v1/approvals/dec_1", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_context_reaches_handlers() {
        let ring = build_authenticator(&[config_with_auth(Some(auth_config(
            AuthMode::ApiKeys,
            vec![key("ops", RAW_KEY, vec!["*".into()])],
        )))])
        .unwrap()
        .unwrap();
        let state = test_state(Some(ring));
        let resp = app_with_auth(state)
            .oneshot(request(
                "/v1/models",
                &[("authorization", format!("Bearer {RAW_KEY}"))],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ops");
    }

    #[test]
    fn ct_eq_boundaries() {
        let a = sha256(b"a");
        let mut b = a;
        assert!(ct_eq(&a, &b));
        b[31] ^= 1;
        assert!(!ct_eq(&a, &b));
    }
}

#[cfg(test)]
mod oidc_tests {
    use super::*;
    use aura_config::{AuthConfig, AuthMode};
    use base64::Engine;
    use rsa::RsaPrivateKey;
    use rsa::pkcs8::EncodePrivateKey;
    use rsa::traits::PublicKeyParts;
    use std::sync::OnceLock;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const KID: &str = "test-key-1";
    const AUDIENCE: &str = "aura";

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs()
    }

    /// One RSA keypair for the whole module: key generation dominates the
    /// runtime otherwise.
    struct TestIdp {
        private_pem: String,
        jwks: serde_json::Value,
    }

    fn idp() -> &'static TestIdp {
        static IDP: OnceLock<TestIdp> = OnceLock::new();
        IDP.get_or_init(|| {
            let mut rng = rsa::rand_core::OsRng;
            let key = RsaPrivateKey::new(&mut rng, 2048).expect("keygen");
            let private_pem = key
                .to_pkcs8_pem(pkcs8::LineEnding::LF)
                .expect("pem")
                .to_string();
            let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            let jwks = serde_json::json!({
                "keys": [{
                    "kty": "RSA",
                    "use": "sig",
                    "alg": "RS256",
                    "kid": KID,
                    "n": b64.encode(key.n().to_bytes_be()),
                    "e": b64.encode(key.e().to_bytes_be()),
                }]
            });
            TestIdp { private_pem, jwks }
        })
    }

    /// Mint an RS256 JWT; `extra` merges into the claims (e.g. `groups`).
    fn token(issuer: &str, audience: &str, lifetime_secs: i64, extra: serde_json::Value) -> String {
        let now = now_unix() as i64;
        let mut claims = serde_json::json!({
            "sub": "alice@example.com",
            "iss": issuer,
            "aud": audience,
            "iat": now,
            "exp": now + lifetime_secs,
        });
        if let (Some(map), Some(extra)) = (claims.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                map.insert(k.clone(), v.clone());
            }
        }
        let header = jsonwebtoken::Header {
            alg: jsonwebtoken::Algorithm::RS256,
            kid: Some(KID.to_string()),
            ..Default::default()
        };
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_rsa_pem(idp().private_pem.as_bytes())
                .expect("encoding key"),
        )
        .expect("encode")
    }

    /// Mint a token with one standard claim removed entirely (e.g. `aud`).
    /// A signed token that omits a claim must not skip that claim's check.
    fn token_without(issuer: &str, drop: &str) -> String {
        let now = now_unix() as i64;
        let mut claims = serde_json::json!({
            "sub": "alice@example.com",
            "iss": issuer,
            "aud": AUDIENCE,
            "iat": now,
            "exp": now + 300,
        });
        claims.as_object_mut().unwrap().remove(drop);
        let header = jsonwebtoken::Header {
            alg: jsonwebtoken::Algorithm::RS256,
            kid: Some(KID.to_string()),
            ..Default::default()
        };
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_rsa_pem(idp().private_pem.as_bytes())
                .expect("encoding key"),
        )
        .expect("encode")
    }

    fn oidc_cfg(issuer: String, jwks_url: Option<String>) -> OidcConfig {
        OidcConfig {
            issuer,
            audience: AUDIENCE.to_string(),
            jwks_url,
            principal_claim: "sub".to_string(),
            allowed_agents: vec!["*".to_string()],
            claim_agents: vec![],
            require_claim: None,
            require_claim_values: vec![],
            clock_skew_secs: 0,
            jwks_refresh_secs: 3600,
        }
    }

    async fn mock_idp() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(idp().jwks.clone()))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn valid_token_authenticates_with_sub_as_principal() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(
            issuer.clone(),
            Some(format!("{issuer}/jwks.json")),
        ));
        validator.warm().await.expect("warm");
        let ctx = match validator
            .authenticate(&token(&issuer, AUDIENCE, 300, serde_json::json!({})))
            .await
        {
            AuthOutcome::Authenticated(ctx) => ctx,
            other => panic!("expected authenticated, got {other:?}"),
        };
        assert_eq!(ctx.name, "alice@example.com");
        assert!(ctx.allows("any-agent", None));
    }

    #[tokio::test]
    async fn resolves_jwks_url_via_discovery() {
        let server = mock_idp().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": server.uri(),
                "jwks_uri": format!("{}/jwks.json", server.uri()),
            })))
            .mount(&server)
            .await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(issuer.clone(), None));
        validator.warm().await.expect("discovery warm");
        assert!(matches!(
            validator
                .authenticate(&token(&issuer, AUDIENCE, 300, serde_json::json!({})))
                .await,
            AuthOutcome::Authenticated(_)
        ));
    }

    #[tokio::test]
    async fn rejects_expired_wrong_aud_wrong_iss_and_garbage() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(
            issuer.clone(),
            Some(format!("{issuer}/jwks.json")),
        ));
        validator.warm().await.unwrap();
        // expired (beyond zero skew)
        assert!(matches!(
            validator
                .authenticate(&token(&issuer, AUDIENCE, -300, serde_json::json!({})))
                .await,
            AuthOutcome::Rejected(Rejection::InvalidToken(_))
        ));
        // wrong audience
        assert!(matches!(
            validator
                .authenticate(&token(&issuer, "someone-else", 300, serde_json::json!({})))
                .await,
            AuthOutcome::Rejected(Rejection::InvalidToken(_))
        ));
        // wrong issuer
        assert!(matches!(
            validator
                .authenticate(&token(
                    "http://mallory.invalid",
                    AUDIENCE,
                    300,
                    serde_json::json!({})
                ))
                .await,
            AuthOutcome::Rejected(Rejection::InvalidToken(_))
        ));
        // not a JWT at all
        assert!(matches!(
            validator.authenticate("not-a-jwt").await,
            AuthOutcome::Rejected(Rejection::InvalidToken(_))
        ));
    }

    #[tokio::test]
    async fn rejects_token_with_future_nbf() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(
            issuer.clone(),
            Some(format!("{issuer}/jwks.json")),
        ));
        validator.warm().await.unwrap();
        // nbf five minutes ahead with zero skew: must not be accepted yet.
        let future = now_unix() as i64 + 300;
        assert!(matches!(
            validator
                .authenticate(&token(
                    &issuer,
                    AUDIENCE,
                    600,
                    serde_json::json!({"nbf": future}),
                ))
                .await,
            AuthOutcome::Rejected(Rejection::InvalidToken(_))
        ));
    }

    #[tokio::test]
    async fn claim_gate_matches_string_and_array_claims() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let mut cfg = oidc_cfg(issuer.clone(), Some(format!("{issuer}/jwks.json")));
        cfg.require_claim = Some("groups".to_string());
        cfg.require_claim_values = vec!["sre".to_string()];
        let validator = OidcValidator::new(cfg);
        validator.warm().await.unwrap();

        let passing = token(
            &issuer,
            AUDIENCE,
            300,
            serde_json::json!({"groups": ["sre", "other"]}),
        );
        assert!(matches!(
            validator.authenticate(&passing).await,
            AuthOutcome::Authenticated(_)
        ));

        let failing = token(
            &issuer,
            AUDIENCE,
            300,
            serde_json::json!({"groups": ["docs"]}),
        );
        assert!(matches!(
            validator.authenticate(&failing).await,
            AuthOutcome::Rejected(Rejection::InsufficientScope)
        ));

        let missing = token(&issuer, AUDIENCE, 300, serde_json::json!({}));
        assert!(matches!(
            validator.authenticate(&missing).await,
            AuthOutcome::Rejected(Rejection::InsufficientScope)
        ));

        let string_claim = token(&issuer, AUDIENCE, 300, serde_json::json!({"groups": "sre"}));
        assert!(matches!(
            validator.authenticate(&string_claim).await,
            AuthOutcome::Authenticated(_)
        ));

        // "Present and non-empty": IdPs emit `""` / `[""]` for missing
        // membership; those must not satisfy the gate.
        let empty_string = token(&issuer, AUDIENCE, 300, serde_json::json!({"groups": ""}));
        assert!(matches!(
            validator.authenticate(&empty_string).await,
            AuthOutcome::Rejected(Rejection::InsufficientScope)
        ));
        let empty_array = token(&issuer, AUDIENCE, 300, serde_json::json!({"groups": [""]}));
        assert!(matches!(
            validator.authenticate(&empty_array).await,
            AuthOutcome::Rejected(Rejection::InsufficientScope)
        ));
    }

    #[tokio::test]
    async fn refresh_picks_up_rotated_keys() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let mut cfg = oidc_cfg(issuer.clone(), Some(format!("{issuer}/jwks.json")));
        cfg.jwks_refresh_secs = 1;
        let validator = OidcValidator::new(cfg);
        validator.warm().await.unwrap();

        // Swap the served key set to one without the token's kid. Once the
        // refresh interval elapses, the next validation refreshes, finds no
        // matching key, and rejects.
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"keys": []})))
            .mount(&server)
            .await;
        let tok = token(&issuer, AUDIENCE, 300, serde_json::json!({}));
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(matches!(
            validator.authenticate(&tok).await,
            AuthOutcome::Rejected(Rejection::InvalidToken(_))
        ));

        // Serve the key again: after the next interval the refresh picks the
        // rotated key up and validation succeeds again.
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(idp().jwks.clone()))
            .mount(&server)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(matches!(
            validator.authenticate(&tok).await,
            AuthOutcome::Authenticated(_)
        ));
    }

    #[test]
    fn mixed_modes_are_rejected() {
        let api = aura_config::Config {
            auth: Some(AuthConfig {
                mode: AuthMode::ApiKeys,
                keys: vec![aura_config::ApiKeyConfig {
                    name: "k".into(),
                    key_hash: aura_config::hash_api_key("x"),
                    key_hash_previous: None,
                    agents: vec!["*".into()],
                }],
                oidc: None,
            }),
            ..Default::default()
        };
        let oidc = aura_config::Config {
            auth: Some(AuthConfig {
                mode: AuthMode::Oidc,
                keys: vec![],
                oidc: Some(oidc_cfg("https://idp.invalid".into(), None)),
            }),
            ..Default::default()
        };
        let err = build_authenticator(&[api, oidc]).unwrap_err();
        assert!(err.contains("cannot mix"), "{err}");
    }

    #[tokio::test]
    async fn middleware_accepts_oidc_bearer_token() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(
            issuer.clone(),
            Some(format!("{issuer}/jwks.json")),
        ));
        validator.warm().await.unwrap();
        let state =
            super::tests::test_state(Some(Arc::new(Authenticator::Oidc(Box::new(validator)))));
        let app = super::tests::app_with_auth(state);
        let jwt = token(&issuer, AUDIENCE, 300, serde_json::json!({}));
        let resp = tower::ServiceExt::oneshot(
            app,
            super::tests::request("/v1/models", &[("authorization", format!("Bearer {jwt}"))]),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"alice@example.com");
    }

    #[tokio::test]
    async fn warm_populates_the_cache() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(
            issuer.clone(),
            Some(format!("{issuer}/jwks.json")),
        ));
        validator.warm().await.unwrap();
        // Kill the endpoint: with the warm set cached and not yet stale,
        // validation must succeed without any fetch.
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        assert!(matches!(
            validator
                .authenticate(&token(&issuer, AUDIENCE, 300, serde_json::json!({})))
                .await,
            AuthOutcome::Authenticated(_)
        ));
    }

    #[tokio::test]
    async fn serves_stale_jwks_when_refresh_fails() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let mut cfg = oidc_cfg(issuer.clone(), Some(format!("{issuer}/jwks.json")));
        cfg.jwks_refresh_secs = 1;
        let validator = OidcValidator::new(cfg);
        validator.warm().await.unwrap();
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        // Past the refresh interval: the refresh fails, but the cached set
        // must keep validating tokens instead of locking everyone out.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert!(matches!(
            validator
                .authenticate(&token(&issuer, AUDIENCE, 300, serde_json::json!({})))
                .await,
            AuthOutcome::Authenticated(_)
        ));
    }

    #[tokio::test]
    async fn idp_down_without_cache_reports_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(
            issuer.clone(),
            Some(format!("{issuer}/jwks.json")),
        ));
        assert!(matches!(
            validator
                .authenticate(&token(&issuer, AUDIENCE, 300, serde_json::json!({})))
                .await,
            AuthOutcome::Unavailable
        ));
        // A second attempt inside the backoff window must not re-probe the
        // IdP; it short-circuits to Unavailable. (Verified behaviorally: the
        // request still fails closed, fast.)
        assert!(matches!(
            validator
                .authenticate(&token(&issuer, AUDIENCE, 300, serde_json::json!({})))
                .await,
            AuthOutcome::Unavailable
        ));
    }

    #[tokio::test]
    async fn middleware_answers_503_when_idp_unreachable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(
            issuer.clone(),
            Some(format!("{issuer}/jwks.json")),
        ));
        let state =
            super::tests::test_state(Some(Arc::new(Authenticator::Oidc(Box::new(validator)))));
        let app = super::tests::app_with_auth(state);
        let jwt = token(&issuer, AUDIENCE, 300, serde_json::json!({}));
        let resp = tower::ServiceExt::oneshot(
            app,
            super::tests::request("/v1/models", &[("authorization", format!("Bearer {jwt}"))]),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        // The caller's credential is not at fault: Retry-After, and no
        // bearer challenge that would trigger a pointless re-login.
        assert_eq!(
            resp.headers().get("retry-after").unwrap().to_str().unwrap(),
            "30"
        );
        assert!(resp.headers().get("www-authenticate").is_none());
    }

    #[test]
    fn namespace_session_is_idempotent_and_collision_free() {
        // Idempotent: echoing the namespaced id back keeps the same session.
        let once = namespace_session("alice", "cs_1");
        assert_eq!(namespace_session("alice", &once), once);
        // Delimiter collisions across principals are impossible: `a` + `b:x`
        // must not equal `a:b` + `x`.
        assert_ne!(namespace_session("a", "b:x"), namespace_session("a:b", "x"));
        // Distinct principals get distinct namespaces for the same id.
        assert_ne!(
            namespace_session("alice", "cs_1"),
            namespace_session("bob", "cs_1")
        );
    }

    #[test]
    fn disabled_oidc_table_does_not_shadow_enabled_config() {
        // A config with `mode = "off"` and a leftover [auth.oidc] table
        // passes validation; it must never contribute settings.
        let mut disabled = oidc_cfg("https://mallory.invalid".into(), None);
        disabled.audience = "not-aura".into();
        let configs = vec![
            aura_config::Config {
                auth: Some(AuthConfig {
                    mode: AuthMode::Off,
                    keys: vec![],
                    oidc: Some(disabled),
                }),
                ..Default::default()
            },
            aura_config::Config {
                auth: Some(AuthConfig {
                    mode: AuthMode::Oidc,
                    keys: vec![],
                    oidc: Some(oidc_cfg("https://idp.invalid".into(), None)),
                }),
                ..Default::default()
            },
        ];
        let auth = build_authenticator(&configs).unwrap().unwrap();
        match auth.as_ref() {
            Authenticator::Oidc(v) => assert_eq!(v.cfg.issuer, "https://idp.invalid"),
            other => panic!("expected oidc authenticator, got {other:?}"),
        }
    }

    #[test]
    fn ec_algorithm_is_inferred_from_the_curve() {
        // A bare EC JWK has no `alg`; the algorithm must follow the curve,
        // or a P-384 key would reject its own valid ES384 tokens.
        for (crv, expected) in [
            ("P-256", Some(Algorithm::ES256)),
            ("P-384", Some(Algorithm::ES384)),
        ] {
            let len = if crv == "P-256" { 32 } else { 48 };
            let c = "AAAA".repeat(len / 4);
            let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_str(&format!(
                r#"{{"kty":"EC","crv":"{crv}","kid":"k1","x":"{c}","y":"{c}"}}"#
            ))
            .expect("valid EC JWK");
            assert_eq!(jwk_algorithm(&jwk), expected, "crv={crv}");
        }
        // P-521 is unsupported by jsonwebtoken (ring): rejected, not
        // silently validated with the wrong algorithm.
        let c = "AAAA".repeat(22); // 66 bytes, P-521 coordinate length
        let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_str(&format!(
            r#"{{"kty":"EC","crv":"P-521","kid":"k1","x":"{c}","y":"{c}"}}"#
        ))
        .expect("valid EC JWK");
        assert_eq!(jwk_algorithm(&jwk), None);
    }

    #[tokio::test]
    async fn claim_gate_denial_is_403_insufficient_scope() {
        // A valid, correctly signed token denied by the claim gate is
        // authenticated-but-not-authorized: 403, never 401, so a compliant
        // client does not discard a fine token and re-login forever.
        let server = mock_idp().await;
        let issuer = server.uri();
        let mut cfg = oidc_cfg(issuer.clone(), Some(format!("{issuer}/jwks.json")));
        cfg.require_claim = Some("groups".to_string());
        cfg.require_claim_values = vec!["sre".to_string()];
        let validator = OidcValidator::new(cfg);
        validator.warm().await.unwrap();
        let state =
            super::tests::test_state(Some(Arc::new(Authenticator::Oidc(Box::new(validator)))));
        let app = super::tests::app_with_auth(state);
        let jwt = token(
            &issuer,
            AUDIENCE,
            300,
            serde_json::json!({"groups": ["docs"]}),
        );
        let resp = tower::ServiceExt::oneshot(
            app,
            super::tests::request("/v1/models", &[("authorization", format!("Bearer {jwt}"))]),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            challenge.contains("error=\"insufficient_scope\""),
            "{challenge}"
        );
    }

    #[tokio::test]
    async fn expired_token_challenge_says_expired_and_advertises_metadata() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(
            issuer.clone(),
            Some(format!("{issuer}/jwks.json")),
        ));
        validator.warm().await.unwrap();
        let state =
            super::tests::test_state(Some(Arc::new(Authenticator::Oidc(Box::new(validator)))));
        let app = super::tests::app_with_auth(state);
        let jwt = token(&issuer, AUDIENCE, -300, serde_json::json!({}));
        let resp = tower::ServiceExt::oneshot(
            app,
            super::tests::request(
                "/v1/models",
                &[
                    ("authorization", format!("Bearer {jwt}")),
                    ("host", "aura.example.com".to_string()),
                ],
            ),
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let challenge = resp
            .headers()
            .get("www-authenticate")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            challenge.contains("error_description=\"token has expired\""),
            "{challenge}"
        );
        // OIDC mode advertises the RFC 9728 metadata document.
        assert!(challenge.contains("resource_metadata=\""), "{challenge}");
    }

    #[tokio::test]
    async fn protected_resource_metadata_is_discoverable_unauthenticated() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(issuer.clone(), None));
        let state =
            super::tests::test_state(Some(Arc::new(Authenticator::Oidc(Box::new(validator)))));
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("aura.example.com"));
        let resp = protected_resource_metadata(axum::extract::State(state), headers).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["resource"], "https://aura.example.com");
        assert_eq!(body["authorization_servers"], serde_json::json!([issuer]));
        assert_eq!(
            body["bearer_methods_supported"],
            serde_json::json!(["header"])
        );
    }

    #[tokio::test]
    async fn protected_resource_metadata_absent_in_api_keys_mode() {
        let ring = build_authenticator(&[aura_config::Config {
            auth: Some(AuthConfig {
                mode: AuthMode::ApiKeys,
                keys: vec![aura_config::ApiKeyConfig {
                    name: "ops".into(),
                    key_hash: aura_config::hash_api_key("x"),
                    key_hash_previous: None,
                    agents: vec!["*".into()],
                }],
                oidc: None,
            }),
            ..Default::default()
        }])
        .unwrap()
        .unwrap();
        let state = super::tests::test_state(Some(ring));
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("aura.example.com"));
        let resp = protected_resource_metadata(axum::extract::State(state), headers).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejects_token_missing_aud_or_iss() {
        // jsonwebtoken only enforces claim *presence* through
        // required_spec_claims (default: exp alone), so a correctly signed
        // token that omits `aud` or `iss` entirely would otherwise skip
        // those checks and authenticate against any deployment trusting
        // the issuer — Keycloak clients without an audience mapper emit
        // exactly such tokens.
        let server = mock_idp().await;
        let issuer = server.uri();
        let validator = OidcValidator::new(oidc_cfg(
            issuer.clone(),
            Some(format!("{issuer}/jwks.json")),
        ));
        validator.warm().await.unwrap();
        for claim in ["aud", "iss"] {
            assert!(
                matches!(
                    validator.authenticate(&token_without(&issuer, claim)).await,
                    AuthOutcome::Rejected(Rejection::InvalidToken(
                        "token is missing a required claim"
                    ))
                ),
                "missing {claim} must be rejected"
            );
        }
    }

    fn claim_agents_cfg(issuer: String, jwks_url: String) -> OidcConfig {
        let mut cfg = oidc_cfg(issuer, Some(jwks_url));
        cfg.allowed_agents = vec!["assistant".to_string()];
        cfg.claim_agents = vec![
            aura_config::ClaimAgentsRule {
                claim: "groups".into(),
                value: "sre".into(),
                agents: vec!["incident-response".into(), "assistant".into()],
            },
            aura_config::ClaimAgentsRule {
                claim: "groups".into(),
                value: "oncall".into(),
                agents: vec!["pager".into(), "assistant".into()],
            },
            aura_config::ClaimAgentsRule {
                claim: "team".into(),
                value: "platform".into(),
                agents: vec!["platform-tools".into()],
            },
        ];
        cfg
    }

    #[tokio::test]
    async fn claim_agents_union_replaces_default_when_matched() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let validator = OidcValidator::new(claim_agents_cfg(
            issuer.clone(),
            format!("{issuer}/jwks.json"),
        ));
        validator.warm().await.unwrap();

        // Two matching rules: union in first-seen order, deduplicated; the
        // global default is replaced, not extended.
        let tok = token(
            &issuer,
            AUDIENCE,
            300,
            serde_json::json!({"groups": ["sre", "oncall"]}),
        );
        let AuthOutcome::Authenticated(ctx) = validator.authenticate(&tok).await else {
            panic!("expected authenticated");
        };
        assert_eq!(
            ctx.allowed_agents,
            ["incident-response", "assistant", "pager"]
        );

        // A string claim matches by equality, not containment.
        let tok = token(&issuer, AUDIENCE, 300, serde_json::json!({"groups": "sre"}));
        let AuthOutcome::Authenticated(ctx) = validator.authenticate(&tok).await else {
            panic!("expected authenticated");
        };
        assert_eq!(ctx.allowed_agents, ["incident-response", "assistant"]);

        // No rule matches: the global default applies.
        let tok = token(
            &issuer,
            AUDIENCE,
            300,
            serde_json::json!({"groups": ["docs"]}),
        );
        let AuthOutcome::Authenticated(ctx) = validator.authenticate(&tok).await else {
            panic!("expected authenticated");
        };
        assert_eq!(ctx.allowed_agents, ["assistant"]);

        // A non-string/array claim value is unusable, not a match.
        let tok = token(&issuer, AUDIENCE, 300, serde_json::json!({"groups": 42}));
        let AuthOutcome::Authenticated(ctx) = validator.authenticate(&tok).await else {
            panic!("expected authenticated");
        };
        assert_eq!(ctx.allowed_agents, ["assistant"]);
    }

    #[tokio::test]
    async fn claim_agents_match_does_not_bypass_the_claim_gate() {
        let server = mock_idp().await;
        let issuer = server.uri();
        let mut cfg = claim_agents_cfg(issuer.clone(), format!("{issuer}/jwks.json"));
        cfg.require_claim = Some("approved".into());
        cfg.require_claim_values = vec!["yes".into()];
        let validator = OidcValidator::new(cfg);
        validator.warm().await.unwrap();

        // Matches the sre rule but fails the gate: still 403.
        let tok = token(
            &issuer,
            AUDIENCE,
            300,
            serde_json::json!({"groups": ["sre"]}),
        );
        assert!(matches!(
            validator.authenticate(&tok).await,
            AuthOutcome::Rejected(Rejection::InsufficientScope)
        ));

        // Passes the gate and matches a rule: rule union applies.
        let tok = token(
            &issuer,
            AUDIENCE,
            300,
            serde_json::json!({"groups": ["sre"], "approved": "yes"}),
        );
        let AuthOutcome::Authenticated(ctx) = validator.authenticate(&tok).await else {
            panic!("expected authenticated");
        };
        assert_eq!(ctx.allowed_agents, ["incident-response", "assistant"]);
    }
}
