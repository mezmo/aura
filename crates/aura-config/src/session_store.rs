//! Session-store configuration: which cross-pod session-state backend the
//! server uses and how to reach it (when not in-memory).
//!
//! Configured **only via environment variables** — never in agent TOML files.
//! TOML configs are per-agent (a server loads N of them), while the session
//! store is deployment infrastructure with exactly one instance per server;
//! a TOML surface would ambiguously imply one store per agent config.
//!
//! See `docs/design/session-storage.md` §8.
//!
//! | Env var                                   | Meaning                                     |
//! | ----------------------------------------- | ------------------------------------------- |
//! | `AURA_SESSION_STORE`                      | backend: `memory` (default) or `redis`      |
//! | `AURA_SESSION_STORE_URL`                  | `redis://` / `rediss://` (Valkey ok)        |
//! | `AURA_SESSION_STORE_PREFIX`               | key/topic namespace (default `aura`)        |
//! | `AURA_SESSION_STORE_CONNECT_TIMEOUT_SECS` | connection timeout (default 5)              |
//! | `AURA_SESSION_STORE_TASK_TTL_SECS`        | A2A task TTL, 0 = no expiry (default 86400) |

use crate::error::ConfigError;
use std::fmt;
use std::num::NonZeroU64;
use std::time::Duration;

/// The session-state backend implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SessionStoreBackend {
    /// Process-local state (single-pod behavior).
    #[default]
    Memory,
    /// Redis/Valkey-backed shared state.
    Redis,
}

impl fmt::Display for SessionStoreBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SessionStoreBackend::Memory => "memory",
            SessionStoreBackend::Redis => "redis",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for SessionStoreBackend {
    type Err = ConfigError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "memory" => Ok(SessionStoreBackend::Memory),
            "redis" | "valkey" => Ok(SessionStoreBackend::Redis),
            other => Err(ConfigError::Validation(format!(
                "unknown session store backend '{other}' (expected 'memory' or 'redis')"
            ))),
        }
    }
}

/// The effective session-store configuration for one server deployment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionStoreConfig {
    /// Process-local state (single-pod behavior).
    #[default]
    Memory,
    /// Redis/Valkey-backed shared state.
    Redis(RedisSessionStoreConfig),
}

/// Connection settings for the Redis/Valkey backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisSessionStoreConfig {
    /// Backend connection URL (`redis://` or `rediss://`; Valkey compatible).
    pub url: String,
    /// Key/topic namespace, so multiple deployments can share a cluster.
    pub key_prefix: String,
    /// Backend connection timeout.
    pub connect_timeout: Duration,
    /// A2A task record TTL in seconds.
    pub task_ttl_secs: Option<NonZeroU64>,
}

const DEFAULT_KEY_PREFIX: &str = "aura";
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_TASK_TTL_SECS: u64 = 86_400;

impl SessionStoreConfig {
    /// Build the deployment's session-store configuration from the
    /// `AURA_SESSION_STORE*` environment variables, defaulting to in-memory
    /// when unset. The Redis connection vars are only read (and the URL only
    /// required) when the backend is `redis`.
    pub fn from_env() -> Result<Self, ConfigError> {
        let backend = env_var("AURA_SESSION_STORE")
            .map(|raw| raw.parse())
            .transpose()?
            .unwrap_or_default();
        match backend {
            SessionStoreBackend::Memory => Ok(Self::Memory),
            SessionStoreBackend::Redis => Ok(Self::Redis(RedisSessionStoreConfig::from_env()?)),
        }
    }

    /// The backend tag for this configuration.
    #[must_use]
    pub fn backend(&self) -> SessionStoreBackend {
        match self {
            Self::Memory => SessionStoreBackend::Memory,
            Self::Redis(_) => SessionStoreBackend::Redis,
        }
    }
}

impl RedisSessionStoreConfig {
    /// Read the Redis connection settings from the environment. The URL is
    /// required; everything else falls back to its default. A TTL of `0`
    /// disables task expiry.
    fn from_env() -> Result<Self, ConfigError> {
        let url = env_var("AURA_SESSION_STORE_URL").ok_or_else(|| {
            ConfigError::Validation(
                "AURA_SESSION_STORE=redis requires AURA_SESSION_STORE_URL".to_string(),
            )
        })?;
        Ok(Self {
            url,
            key_prefix: env_var("AURA_SESSION_STORE_PREFIX")
                .unwrap_or_else(|| DEFAULT_KEY_PREFIX.to_string()),
            connect_timeout: env_var_u64("AURA_SESSION_STORE_CONNECT_TIMEOUT_SECS")?
                .map_or(DEFAULT_CONNECT_TIMEOUT, Duration::from_secs),
            task_ttl_secs: NonZeroU64::new(
                env_var_u64("AURA_SESSION_STORE_TASK_TTL_SECS")?.unwrap_or(DEFAULT_TASK_TTL_SECS),
            ),
        })
    }
}

/// Read an env var, treating unset and empty as absent.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_var_u64(name: &str) -> Result<Option<u64>, ConfigError> {
    env_var(name)
        .map(|v| {
            v.trim().parse::<u64>().map_err(|_| {
                ConfigError::Validation(format!("{name} must be a non-negative integer, got '{v}'"))
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env_lock;

    fn clear_env() {
        for var in [
            "AURA_SESSION_STORE",
            "AURA_SESSION_STORE_URL",
            "AURA_SESSION_STORE_PREFIX",
            "AURA_SESSION_STORE_CONNECT_TIMEOUT_SECS",
            "AURA_SESSION_STORE_TASK_TTL_SECS",
        ] {
            unsafe { std::env::remove_var(var) };
        }
    }

    fn expect_redis(config: SessionStoreConfig) -> RedisSessionStoreConfig {
        match config {
            SessionStoreConfig::Redis(redis) => redis,
            other => panic!("expected the redis backend, got {other:?}"),
        }
    }

    #[test]
    fn defaults_to_memory() {
        let _guard = test_env_lock::lock();
        clear_env();
        let config = SessionStoreConfig::from_env().unwrap();
        assert_eq!(config, SessionStoreConfig::Memory);
        assert_eq!(config.backend(), SessionStoreBackend::Memory);
    }

    #[test]
    fn reads_all_env_vars() {
        let _guard = test_env_lock::lock();
        clear_env();
        unsafe {
            std::env::set_var("AURA_SESSION_STORE", "redis");
            std::env::set_var("AURA_SESSION_STORE_URL", "redis://envhost:6379");
            std::env::set_var("AURA_SESSION_STORE_PREFIX", "aura:env");
            std::env::set_var("AURA_SESSION_STORE_CONNECT_TIMEOUT_SECS", "2");
            std::env::set_var("AURA_SESSION_STORE_TASK_TTL_SECS", "3600");
        }
        let config = SessionStoreConfig::from_env();
        clear_env();
        let redis = expect_redis(config.unwrap());
        assert_eq!(redis.url, "redis://envhost:6379");
        assert_eq!(redis.key_prefix, "aura:env");
        assert_eq!(redis.connect_timeout, Duration::from_secs(2));
        assert_eq!(redis.task_ttl_secs, NonZeroU64::new(3600));
    }

    #[test]
    fn redis_fills_defaults_for_unset_vars() {
        let _guard = test_env_lock::lock();
        clear_env();
        unsafe {
            std::env::set_var("AURA_SESSION_STORE", "redis");
            std::env::set_var("AURA_SESSION_STORE_URL", "redis://envhost:6379");
        }
        let config = SessionStoreConfig::from_env();
        clear_env();
        let redis = expect_redis(config.unwrap());
        assert_eq!(redis.key_prefix, "aura");
        assert_eq!(redis.connect_timeout, Duration::from_secs(5));
        assert_eq!(redis.task_ttl_secs, NonZeroU64::new(86_400));
    }

    #[test]
    fn valkey_is_an_alias_for_redis() {
        assert_eq!(
            "valkey".parse::<SessionStoreBackend>().unwrap(),
            SessionStoreBackend::Redis
        );
    }

    #[test]
    fn redis_requires_url() {
        let _guard = test_env_lock::lock();
        clear_env();
        unsafe { std::env::set_var("AURA_SESSION_STORE", "redis") };
        let err = SessionStoreConfig::from_env().unwrap_err();
        clear_env();
        assert!(err.to_string().contains("AURA_SESSION_STORE_URL"));
    }

    #[test]
    fn unknown_backend_rejected() {
        let _guard = test_env_lock::lock();
        clear_env();
        unsafe { std::env::set_var("AURA_SESSION_STORE", "postgres") };
        let err = SessionStoreConfig::from_env().unwrap_err();
        clear_env();
        assert!(err.to_string().contains("unknown session store backend"));
    }

    #[test]
    fn non_numeric_ttl_rejected() {
        let _guard = test_env_lock::lock();
        clear_env();
        unsafe {
            std::env::set_var("AURA_SESSION_STORE", "redis");
            std::env::set_var("AURA_SESSION_STORE_URL", "redis://envhost:6379");
            std::env::set_var("AURA_SESSION_STORE_TASK_TTL_SECS", "soon");
        }
        let err = SessionStoreConfig::from_env().unwrap_err();
        clear_env();
        assert!(
            err.to_string()
                .contains("AURA_SESSION_STORE_TASK_TTL_SECS must be a non-negative integer")
        );
    }

    #[test]
    fn zero_ttl_disables_expiry() {
        let _guard = test_env_lock::lock();
        clear_env();
        unsafe {
            std::env::set_var("AURA_SESSION_STORE", "redis");
            std::env::set_var("AURA_SESSION_STORE_URL", "redis://envhost:6379");
            std::env::set_var("AURA_SESSION_STORE_TASK_TTL_SECS", "0");
        }
        let config = SessionStoreConfig::from_env();
        clear_env();
        assert_eq!(expect_redis(config.unwrap()).task_ttl_secs, None);
    }

    #[test]
    fn redis_vars_are_ignored_for_memory_backend() {
        let _guard = test_env_lock::lock();
        clear_env();
        unsafe { std::env::set_var("AURA_SESSION_STORE_TASK_TTL_SECS", "soon") };
        let config = SessionStoreConfig::from_env();
        clear_env();
        assert_eq!(config.unwrap(), SessionStoreConfig::Memory);
    }

    #[test]
    fn empty_env_vars_are_ignored() {
        let _guard = test_env_lock::lock();
        clear_env();
        unsafe {
            std::env::set_var("AURA_SESSION_STORE", "");
            std::env::set_var("AURA_SESSION_STORE_PREFIX", "  ");
        }
        let config = SessionStoreConfig::from_env();
        clear_env();
        assert_eq!(config.unwrap(), SessionStoreConfig::Memory);
    }
}
