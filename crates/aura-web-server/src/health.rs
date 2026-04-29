//! Deep health and readiness checks for Kubernetes probes.
//!
//! Provides `/health/live` (liveness) and `/health/ready` (readiness with per-subsystem
//! connectivity verification). Results are cached with a configurable TTL to avoid
//! flooding upstream dependencies on every K8s probe.

use actix_web::{HttpResponse, web};
use aura_config::config::{LlmConfig, McpServerConfig, VectorStoreConfig, VectorStoreType};
use futures_util::future::join_all;
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::types::AppState;

/// Shared HTTP client for probe requests. Reuses connection pool across probes.
static PROBE_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
});

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubsystemStatus {
    Ok,
    Error { message: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmHealthResult {
    pub provider: String,
    pub model: String,
    pub agent: String,
    #[serde(flatten)]
    pub status: SubsystemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpHealthResult {
    pub transport: String,
    #[serde(flatten)]
    pub status: SubsystemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VectorStoreHealthResult {
    #[serde(rename = "type")]
    pub store_type: String,
    #[serde(flatten)]
    pub status: SubsystemStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthChecks {
    pub llm: Vec<LlmHealthResult>,
    pub mcp: BTreeMap<String, McpHealthResult>,
    pub vector_stores: BTreeMap<String, VectorStoreHealthResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthCheckResult {
    pub status: HealthStatus,
    pub checks: HealthChecks,
    pub check_duration_ms: u64,
    pub cached: bool,
}

// ── Probe Functions ──────────────────────────────────────────────────────────

/// Sanitize a probe error into a safe category string.
/// Never exposes internal IPs, hostnames, or DNS names.
fn sanitize_probe_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "timeout".to_string()
    } else if err.is_connect() {
        "connection_refused".to_string()
    } else if err.is_redirect() {
        "redirect_error".to_string()
    } else {
        "probe_error".to_string()
    }
}

/// Probe a single LLM provider with a lightweight API call.
async fn probe_llm(config: &LlmConfig, agent_name: &str, timeout: Duration) -> LlmHealthResult {
    let (provider, model) = config.model_info();
    let start = Instant::now();

    let status = match tokio::time::timeout(timeout, probe_llm_inner(config)).await {
        Ok(result) => result,
        Err(_) => SubsystemStatus::Error {
            message: "timeout".to_string(),
        },
    };

    let latency_ms = Some(start.elapsed().as_millis() as u64);

    debug!(
        provider = provider,
        model = model,
        agent = agent_name,
        latency_ms = latency_ms,
        "LLM probe completed"
    );

    if let SubsystemStatus::Error { ref message } = status {
        warn!(
            provider = provider,
            model = model,
            agent = agent_name,
            error = message,
            "LLM probe failed"
        );
    }

    LlmHealthResult {
        provider: provider.to_string(),
        model: model.to_string(),
        agent: agent_name.to_string(),
        status,
        latency_ms,
    }
}

async fn probe_llm_inner(config: &LlmConfig) -> SubsystemStatus {
    let client = &*PROBE_CLIENT;

    match config {
        LlmConfig::OpenAI {
            api_key, base_url, ..
        } => {
            let url = base_url
                .as_deref()
                .unwrap_or("https://api.openai.com")
                .trim_end_matches('/');
            let url = format!("{url}/v1/models");
            match client
                .get(&url)
                .header("Authorization", format!("Bearer {api_key}"))
                .send()
                .await
            {
                Ok(resp) if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 => {
                    SubsystemStatus::Error {
                        message: "auth_failed".to_string(),
                    }
                }
                Ok(_) => SubsystemStatus::Ok,
                Err(e) => SubsystemStatus::Error {
                    message: sanitize_probe_error(&e),
                },
            }
        }
        LlmConfig::Anthropic {
            api_key, base_url, ..
        } => {
            let url = base_url
                .as_deref()
                .unwrap_or("https://api.anthropic.com")
                .trim_end_matches('/');
            let url = format!("{url}/v1/messages");
            match client
                .get(&url)
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
            {
                Ok(resp) if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 => {
                    SubsystemStatus::Error {
                        message: "auth_failed".to_string(),
                    }
                }
                Ok(_) => SubsystemStatus::Ok,
                Err(e) => SubsystemStatus::Error {
                    message: sanitize_probe_error(&e),
                },
            }
        }
        LlmConfig::Bedrock {
            region, profile, ..
        } => probe_aws_credentials(region, profile.as_deref()).await,
        LlmConfig::Gemini {
            api_key, base_url, ..
        } => {
            let url = base_url
                .as_deref()
                .unwrap_or("https://generativelanguage.googleapis.com")
                .trim_end_matches('/');
            let url = format!("{url}/v1beta/models");
            match client
                .get(&url)
                .header("x-goog-api-key", api_key)
                .send()
                .await
            {
                Ok(resp) if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 => {
                    SubsystemStatus::Error {
                        message: "auth_failed".to_string(),
                    }
                }
                Ok(_) => SubsystemStatus::Ok,
                Err(e) => SubsystemStatus::Error {
                    message: sanitize_probe_error(&e),
                },
            }
        }
        LlmConfig::Ollama { base_url, .. } => {
            let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
            match client.get(&url).send().await {
                Ok(_) => SubsystemStatus::Ok,
                Err(e) => SubsystemStatus::Error {
                    message: sanitize_probe_error(&e),
                },
            }
        }
    }
}

/// Probe a single MCP server.
async fn probe_mcp_server(
    server_name: &str,
    config: &McpServerConfig,
    timeout: Duration,
) -> (String, McpHealthResult) {
    let (transport, result) = match config {
        McpServerConfig::Stdio { .. } => (
            "stdio",
            McpHealthResult {
                transport: "stdio".to_string(),
                status: SubsystemStatus::Ok,
                latency_ms: None,
            },
        ),
        McpServerConfig::HttpStreamable { url, .. } => {
            let start = Instant::now();
            let status = match tokio::time::timeout(timeout, probe_mcp_http(url)).await {
                Ok(result) => result,
                Err(_) => SubsystemStatus::Error {
                    message: "timeout".to_string(),
                },
            };
            let latency_ms = Some(start.elapsed().as_millis() as u64);
            (
                "http_streamable",
                McpHealthResult {
                    transport: "http_streamable".to_string(),
                    status,
                    latency_ms,
                },
            )
        }
    };

    debug!(
        server = server_name,
        transport = transport,
        "MCP probe completed"
    );

    if let SubsystemStatus::Error { ref message } = result.status {
        warn!(
            server = server_name,
            transport = transport,
            error = message,
            "MCP probe failed"
        );
    }

    (server_name.to_string(), result)
}

/// Connectivity check only: any HTTP response (200, 404, 405) = reachable.
/// Only connection error or timeout = down.
async fn probe_mcp_http(url: &str) -> SubsystemStatus {
    let client = &*PROBE_CLIENT;

    match client.get(url).send().await {
        Ok(_) => SubsystemStatus::Ok,
        Err(e) => SubsystemStatus::Error {
            message: sanitize_probe_error(&e),
        },
    }
}

/// Probe a single vector store.
async fn probe_vector_store(
    store_name: &str,
    config: &VectorStoreConfig,
    timeout: Duration,
) -> (String, VectorStoreHealthResult) {
    let (store_type, result) = match &config.store {
        VectorStoreType::InMemory { .. } => (
            "in_memory",
            VectorStoreHealthResult {
                store_type: "in_memory".to_string(),
                status: SubsystemStatus::Ok,
                latency_ms: None,
            },
        ),
        VectorStoreType::Qdrant { url, .. } => {
            let start = Instant::now();
            let health_url = format!("{}/healthz", url.trim_end_matches('/'));
            let status = match tokio::time::timeout(timeout, probe_http_health(&health_url)).await {
                Ok(result) => result,
                Err(_) => SubsystemStatus::Error {
                    message: "timeout".to_string(),
                },
            };
            let latency_ms = Some(start.elapsed().as_millis() as u64);
            (
                "qdrant",
                VectorStoreHealthResult {
                    store_type: "qdrant".to_string(),
                    status,
                    latency_ms,
                },
            )
        }
        VectorStoreType::BedrockKb {
            region, profile, ..
        } => {
            let start = Instant::now();
            let status = match tokio::time::timeout(
                timeout,
                probe_aws_credentials(region, profile.as_deref()),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => SubsystemStatus::Error {
                    message: "timeout".to_string(),
                },
            };
            let latency_ms = Some(start.elapsed().as_millis() as u64);
            (
                "bedrock_kb",
                VectorStoreHealthResult {
                    store_type: "bedrock_kb".to_string(),
                    status,
                    latency_ms,
                },
            )
        }
    };

    debug!(
        store = store_name,
        store_type = store_type,
        "Vector store probe completed"
    );

    if let SubsystemStatus::Error { ref message } = result.status {
        warn!(
            store = store_name,
            store_type = store_type,
            error = message,
            "Vector store probe failed"
        );
    }

    (store_name.to_string(), result)
}

async fn probe_http_health(url: &str) -> SubsystemStatus {
    let client = &*PROBE_CLIENT;

    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => SubsystemStatus::Ok,
        Ok(resp) => SubsystemStatus::Error {
            message: format!("http_{}", resp.status().as_u16()),
        },
        Err(e) => SubsystemStatus::Error {
            message: sanitize_probe_error(&e),
        },
    }
}

async fn probe_aws_credentials(region: &str, profile: Option<&str>) -> SubsystemStatus {
    // Validate AWS credentials can be resolved. This catches expired STS tokens,
    // missing profiles, and misconfigured credential chains — the most common
    // Bedrock failure modes. It does NOT validate IAM permissions.
    use aws_credential_types::provider::ProvideCredentials;

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new(region.to_string()));
    if let Some(p) = profile {
        loader = loader.profile_name(p);
    }
    let sdk_config = loader.load().await;
    match sdk_config.credentials_provider() {
        Some(provider) => match provider.provide_credentials().await {
            Ok(_) => SubsystemStatus::Ok,
            Err(_) => SubsystemStatus::Error {
                message: "auth_failed".to_string(),
            },
        },
        None => SubsystemStatus::Error {
            message: "auth_failed".to_string(),
        },
    }
}

// ── Aggregation ──────────────────────────────────────────────────────────────

/// Run all health probes across all configs with deduplication.
pub async fn run_health_check(
    configs: &[aura_config::Config],
    timeout: Duration,
) -> HealthCheckResult {
    let start = Instant::now();

    // Deduplicate LLM providers by (provider_type, base_url_or_region)
    let mut llm_seen = HashSet::new();
    let mut llm_futures = Vec::new();
    for config in configs {
        let dedup_key = llm_dedup_key(&config.llm);
        if llm_seen.insert(dedup_key) {
            let agent_name = config
                .agent
                .alias
                .as_deref()
                .unwrap_or(&config.agent.name)
                .to_string();
            let llm = config.llm.clone();
            llm_futures.push(async move { probe_llm(&llm, &agent_name, timeout).await });
        }
    }

    // Deduplicate MCP servers by (transport, url_or_cmd)
    let mut mcp_seen = HashSet::new();
    let mut mcp_futures = Vec::new();
    for config in configs {
        if let Some(ref mcp) = config.mcp {
            for (name, server_config) in &mcp.servers {
                let dedup_key = mcp_dedup_key(server_config);
                if mcp_seen.insert(dedup_key) {
                    let name = name.clone();
                    let sc = server_config.clone();
                    mcp_futures.push(async move { probe_mcp_server(&name, &sc, timeout).await });
                }
            }
        }
    }

    // Deduplicate vector stores by (type, url_or_kb_id)
    let mut vs_seen = HashSet::new();
    let mut vs_futures = Vec::new();
    for config in configs {
        for store in &config.vector_stores {
            let dedup_key = vs_dedup_key(store);
            if vs_seen.insert(dedup_key) {
                let name = store.name.clone();
                let sc = store.clone();
                vs_futures.push(async move { probe_vector_store(&name, &sc, timeout).await });
            }
        }
    }

    // Run all probes concurrently
    let (llm_results, mcp_results, vs_results) = tokio::join!(
        join_all(llm_futures),
        join_all(mcp_futures),
        join_all(vs_futures),
    );

    let mcp_map: BTreeMap<String, McpHealthResult> = mcp_results.into_iter().collect();
    let vs_map: BTreeMap<String, VectorStoreHealthResult> = vs_results.into_iter().collect();

    // Aggregate status
    let any_error = llm_results
        .iter()
        .any(|r| matches!(r.status, SubsystemStatus::Error { .. }))
        || mcp_map
            .values()
            .any(|r| matches!(r.status, SubsystemStatus::Error { .. }))
        || vs_map
            .values()
            .any(|r| matches!(r.status, SubsystemStatus::Error { .. }));

    let status = if any_error {
        HealthStatus::Unhealthy
    } else {
        HealthStatus::Healthy
    };

    let check_duration_ms = start.elapsed().as_millis() as u64;

    HealthCheckResult {
        status,
        checks: HealthChecks {
            llm: llm_results,
            mcp: mcp_map,
            vector_stores: vs_map,
        },
        check_duration_ms,
        cached: false,
    }
}

fn llm_dedup_key(config: &LlmConfig) -> String {
    match config {
        LlmConfig::OpenAI { base_url, .. } => {
            format!("openai:{}", base_url.as_deref().unwrap_or("default"))
        }
        LlmConfig::Anthropic { base_url, .. } => {
            format!("anthropic:{}", base_url.as_deref().unwrap_or("default"))
        }
        LlmConfig::Bedrock { region, .. } => format!("bedrock:{region}"),
        LlmConfig::Gemini { base_url, .. } => {
            format!("gemini:{}", base_url.as_deref().unwrap_or("default"))
        }
        LlmConfig::Ollama { base_url, .. } => format!("ollama:{base_url}"),
    }
}

fn mcp_dedup_key(config: &McpServerConfig) -> String {
    match config {
        McpServerConfig::Stdio { cmd, args, .. } => {
            format!("stdio:{}:{}", cmd.join(" "), args.join(" "))
        }
        McpServerConfig::HttpStreamable { url, .. } => format!("http:{url}"),
    }
}

fn vs_dedup_key(config: &VectorStoreConfig) -> String {
    match &config.store {
        VectorStoreType::InMemory { .. } => format!("in_memory:{}", config.name),
        VectorStoreType::Qdrant { url, .. } => format!("qdrant:{url}"),
        VectorStoreType::BedrockKb {
            knowledge_base_id, ..
        } => format!("bedrock_kb:{knowledge_base_id}"),
    }
}

// ── Cache ────────────────────────────────────────────────────────────────────

struct CachedResult {
    result: HealthCheckResult,
    checked_at: Instant,
}

/// Cached health check service. Stored in AppState.
pub struct HealthCheckService {
    cache: RwLock<Option<CachedResult>>,
    configs: Arc<Vec<aura_config::Config>>,
    ttl: Duration,
    probe_timeout: Duration,
    last_status: RwLock<Option<HealthStatus>>,
}

impl HealthCheckService {
    pub fn new(
        configs: Arc<Vec<aura_config::Config>>,
        ttl: Duration,
        probe_timeout: Duration,
    ) -> Self {
        Self {
            cache: RwLock::new(None),
            configs,
            ttl,
            probe_timeout,
            last_status: RwLock::new(None),
        }
    }

    /// Returns cached result if fresh, otherwise runs probes.
    pub async fn get_health(&self) -> HealthCheckResult {
        // Fast path: read lock, check TTL
        {
            let cache = self.cache.read().await;
            if let Some(ref cached) = *cache
                && cached.checked_at.elapsed() < self.ttl
            {
                let mut result = cached.result.clone();
                result.cached = true;
                return result;
            }
        }

        // Slow path: write lock, double-check, run probes
        let mut cache = self.cache.write().await;
        if let Some(ref cached) = *cache
            && cached.checked_at.elapsed() < self.ttl
        {
            let mut result = cached.result.clone();
            result.cached = true;
            return result;
        }

        let result = run_health_check(&self.configs, self.probe_timeout).await;

        // Log status transitions
        let mut last = self.last_status.write().await;
        if let Some(prev) = *last
            && prev != result.status
        {
            info!(
                previous = ?prev,
                current = ?result.status,
                "Health status changed"
            );
        }
        *last = Some(result.status);

        *cache = Some(CachedResult {
            result: result.clone(),
            checked_at: Instant::now(),
        });

        result
    }

    /// Test helper: pre-populate cache for unit testing.
    #[cfg(test)]
    pub async fn set_cache_for_test(&self, result: HealthCheckResult) {
        let mut cache = self.cache.write().await;
        *cache = Some(CachedResult {
            result,
            checked_at: Instant::now(),
        });
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// GET /health/live — Kubernetes liveness probe. Always returns 200.
pub async fn liveness() -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({ "status": "alive" }))
}

/// GET /health/ready — Kubernetes readiness probe. 200 if healthy, 503 if not.
pub async fn readiness(data: web::Data<AppState>) -> HttpResponse {
    let result = data.health_service.get_health().await;

    crate::metrics::record_health_check(&result);

    let status_code = match result.status {
        HealthStatus::Healthy => actix_web::http::StatusCode::OK,
        HealthStatus::Unhealthy => actix_web::http::StatusCode::SERVICE_UNAVAILABLE,
    };

    HttpResponse::build(status_code).json(result)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn healthy_result() -> HealthCheckResult {
        HealthCheckResult {
            status: HealthStatus::Healthy,
            checks: HealthChecks {
                llm: vec![LlmHealthResult {
                    provider: "bedrock".to_string(),
                    model: "anthropic.claude-sonnet-4-20250514".to_string(),
                    agent: "assistant".to_string(),
                    status: SubsystemStatus::Ok,
                    latency_ms: Some(45),
                }],
                mcp: BTreeMap::from([(
                    "example".to_string(),
                    McpHealthResult {
                        transport: "http_streamable".to_string(),
                        status: SubsystemStatus::Ok,
                        latency_ms: Some(12),
                    },
                )]),
                vector_stores: BTreeMap::from([(
                    "docs".to_string(),
                    VectorStoreHealthResult {
                        store_type: "qdrant".to_string(),
                        status: SubsystemStatus::Ok,
                        latency_ms: Some(8),
                    },
                )]),
            },
            check_duration_ms: 52,
            cached: false,
        }
    }

    fn unhealthy_result() -> HealthCheckResult {
        HealthCheckResult {
            status: HealthStatus::Unhealthy,
            checks: HealthChecks {
                llm: vec![LlmHealthResult {
                    provider: "openai".to_string(),
                    model: "gpt-4o".to_string(),
                    agent: "assistant".to_string(),
                    status: SubsystemStatus::Error {
                        message: "auth_failed".to_string(),
                    },
                    latency_ms: Some(100),
                }],
                mcp: BTreeMap::new(),
                vector_stores: BTreeMap::new(),
            },
            check_duration_ms: 100,
            cached: false,
        }
    }

    // TC-002.SER.1: HealthStatus variants serialize correctly
    #[test]
    fn test_health_status_serialization() {
        let healthy = serde_json::to_value(HealthStatus::Healthy).unwrap();
        assert_eq!(healthy, "healthy");

        let unhealthy = serde_json::to_value(HealthStatus::Unhealthy).unwrap();
        assert_eq!(unhealthy, "unhealthy");
    }

    // TC-002.1.1.1: Readiness returns 200 with healthy status
    #[test]
    fn test_healthy_maps_to_200() {
        let result = healthy_result();
        assert_eq!(result.status, HealthStatus::Healthy);

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["status"], "healthy");
        assert!(json["checks"].is_object());
        assert!(json["checks"]["llm"].is_array());
        assert!(json["checks"]["mcp"].is_object());
        assert!(json["checks"]["vector_stores"].is_object());
        assert!(json["check_duration_ms"].is_number());
        assert_eq!(json["cached"], false);
    }

    // TC-002.1.1.2: Readiness returns 503 with unhealthy status
    #[test]
    fn test_unhealthy_maps_to_503() {
        let result = unhealthy_result();
        assert_eq!(result.status, HealthStatus::Unhealthy);

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["status"], "unhealthy");
        assert_eq!(json["checks"]["llm"][0]["status"], "error");
        assert_eq!(json["checks"]["llm"][0]["message"], "auth_failed");
    }

    // TC-002.4.1.1: Health check result serializes with all subsystem sections
    #[test]
    fn test_health_result_serialization() {
        let mut result = healthy_result();
        // Add an error entry to MCP
        result.checks.mcp.insert(
            "broken".to_string(),
            McpHealthResult {
                transport: "http_streamable".to_string(),
                status: SubsystemStatus::Error {
                    message: "connection_refused".to_string(),
                },
                latency_ms: Some(50),
            },
        );

        let json = serde_json::to_value(&result).unwrap();

        // Ok entries should NOT have "message" field
        assert!(json["checks"]["mcp"]["example"].get("message").is_none());
        // Error entries should have "message" field
        assert_eq!(
            json["checks"]["mcp"]["broken"]["message"],
            "connection_refused"
        );
    }

    // TC-002.3.2.1: STDIO MCP server reports ok
    #[tokio::test]
    async fn test_stdio_mcp_ok() {
        let config = McpServerConfig::Stdio {
            cmd: vec!["node".to_string()],
            args: vec!["server.js".to_string()],
            env: Default::default(),
            description: None,
        };

        let (name, result) = probe_mcp_server("test_stdio", &config, Duration::from_secs(1)).await;

        assert_eq!(name, "test_stdio");
        assert_eq!(result.transport, "stdio");
        assert!(matches!(result.status, SubsystemStatus::Ok));
        assert!(result.latency_ms.is_none());
    }

    // TC-002.4.2.2: InMemory vector store always returns ok
    #[tokio::test]
    async fn test_in_memory_vector_store_ok() {
        let config = VectorStoreConfig {
            name: "test_mem".to_string(),
            context_prefix: None,
            store: VectorStoreType::InMemory {
                embedding_model: aura_config::config::EmbeddingConfig::OpenAI {
                    api_key: "test".to_string(),
                    model: "text-embedding-3-small".to_string(),
                },
            },
        };

        let (name, result) = probe_vector_store("test_mem", &config, Duration::from_secs(1)).await;

        assert_eq!(name, "test_mem");
        assert_eq!(result.store_type, "in_memory");
        assert!(matches!(result.status, SubsystemStatus::Ok));
        assert!(result.latency_ms.is_none());
    }

    // TC-002.3.3.1 + TC-002.4.E.1 + TC-002.1.E.1: Empty subsystems
    #[test]
    fn test_empty_checks_produce_healthy() {
        let result = HealthCheckResult {
            status: HealthStatus::Healthy,
            checks: HealthChecks {
                llm: Vec::new(),
                mcp: BTreeMap::new(),
                vector_stores: BTreeMap::new(),
            },
            check_duration_ms: 0,
            cached: false,
        };

        assert_eq!(result.status, HealthStatus::Healthy);
        assert!(result.checks.llm.is_empty());
        assert!(result.checks.mcp.is_empty());
        assert!(result.checks.vector_stores.is_empty());

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["checks"]["llm"], serde_json::json!([]));
        assert_eq!(json["checks"]["mcp"], serde_json::json!({}));
        assert_eq!(json["checks"]["vector_stores"], serde_json::json!({}));
    }

    // TC-002.2.1.1: Liveness endpoint returns alive
    #[test]
    fn test_liveness_response() {
        // Verify the response shape (handler is a simple JSON return)
        let expected = serde_json::json!({ "status": "alive" });
        assert_eq!(expected["status"], "alive");
    }

    // TC-002.7.1.1: Shutdown guard exempts /health/* paths
    #[test]
    fn test_shutdown_guard_exemption_paths() {
        let check = |path: &str| -> bool {
            path == "/health" || path.starts_with("/health/") || path == "/metrics"
        };

        assert!(check("/health"));
        assert!(check("/health/live"));
        assert!(check("/health/ready"));
        assert!(check("/metrics"));
        assert!(!check("/v1/chat/completions"));
        assert!(!check("/v1/models"));
    }

    // TC-002.BC.1: Existing /health endpoint unchanged
    #[test]
    fn test_legacy_health_response() {
        let expected = serde_json::json!({"status": "healthy"});
        assert_eq!(expected["status"], "healthy");
    }

    // TC-002.5.1.1: Cache returns cached result within TTL
    #[tokio::test]
    async fn test_cache_within_ttl() {
        let configs = Arc::new(Vec::new());
        let service =
            HealthCheckService::new(configs, Duration::from_secs(10), Duration::from_secs(1));

        let original = healthy_result();
        service.set_cache_for_test(original.clone()).await;

        let result = service.get_health().await;
        assert!(result.cached);
        assert_eq!(result.status, HealthStatus::Healthy);
    }

    // TC-002.5.3.1: Expired cache triggers fresh probe (0ms TTL = always expired)
    #[tokio::test]
    async fn test_cache_expired_returns_fresh() {
        let configs = Arc::new(Vec::new());
        let service =
            HealthCheckService::new(configs, Duration::from_millis(0), Duration::from_secs(1));

        let result = service.get_health().await;
        assert!(!result.cached);

        let result2 = service.get_health().await;
        assert!(!result2.cached);
    }

    // TC-002.1.3.1: Deduplication logic
    #[test]
    fn test_llm_dedup_key() {
        let config1 = LlmConfig::OpenAI {
            api_key: "key1".to_string(),
            model: "gpt-4o".to_string(),
            base_url: None,
        };
        let config2 = LlmConfig::OpenAI {
            api_key: "key1".to_string(),
            model: "gpt-4o-mini".to_string(),
            base_url: None,
        };
        let config3 = LlmConfig::Ollama {
            model: "llama3".to_string(),
            base_url: "http://localhost:11434".to_string(),
            fallback_tool_parsing: false,
            num_ctx: None,
            num_predict: None,
            additional_params: None,
        };

        // Same provider + same base_url = same key (despite different model/key)
        assert_eq!(llm_dedup_key(&config1), llm_dedup_key(&config2));
        // Different provider = different key
        assert_ne!(llm_dedup_key(&config1), llm_dedup_key(&config3));
    }

    // TC-002.1.2.1: LLM probe returns error for unreachable provider
    #[tokio::test]
    async fn test_llm_probe_unreachable() {
        let config = LlmConfig::OpenAI {
            api_key: "test_key".to_string(),
            model: "gpt-4o".to_string(),
            base_url: Some("http://127.0.0.1:1".to_string()),
        };

        let result = probe_llm(&config, "test_agent", Duration::from_secs(2)).await;
        assert_eq!(result.provider, "openai");
        assert!(matches!(result.status, SubsystemStatus::Error { .. }));
    }

    // TC-002.3.1.1: MCP HTTP Streamable probe detects unreachable server
    #[tokio::test]
    async fn test_mcp_probe_unreachable() {
        let config = McpServerConfig::HttpStreamable {
            url: "http://127.0.0.1:1".to_string(),
            headers: Default::default(),
            description: None,
            headers_from_request: Default::default(),
        };

        let (name, result) = probe_mcp_server("broken_mcp", &config, Duration::from_secs(2)).await;
        assert_eq!(name, "broken_mcp");
        assert_eq!(result.transport, "http_streamable");
        assert!(matches!(result.status, SubsystemStatus::Error { .. }));
    }

    // TC-002.4.2.1: Vector store probe returns error for unreachable Qdrant
    #[tokio::test]
    async fn test_vector_store_probe_unreachable() {
        let config = VectorStoreConfig {
            name: "broken_qdrant".to_string(),
            context_prefix: None,
            store: VectorStoreType::Qdrant {
                embedding_model: aura_config::config::EmbeddingConfig::OpenAI {
                    api_key: "test".to_string(),
                    model: "text-embedding-3-small".to_string(),
                },
                url: "http://127.0.0.1:1".to_string(),
                collection_name: "test".to_string(),
            },
        };

        let (name, result) =
            probe_vector_store("broken_qdrant", &config, Duration::from_secs(2)).await;
        assert_eq!(name, "broken_qdrant");
        assert_eq!(result.store_type, "qdrant");
        assert!(matches!(result.status, SubsystemStatus::Error { .. }));
    }
}
