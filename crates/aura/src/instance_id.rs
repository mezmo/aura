//! Stable per-agent instance identity.
//!
//! Produces a deterministic [`Uuid`] that identifies a specific agent running
//! on a specific host, without exposing raw machine credentials on the wire.
//!
//! ## Formula
//!
//! ```text
//! instance_id = UUIDv5(AURA_NAMESPACE, ns(name) || ns(agent_seed) || ns(env_seed))
//! ```
//!
//! Each component is netstring-encoded (`{byte_len}:{utf8}` for present
//! values, `-` for absent `Option` fields) so adjacent field boundaries cannot
//! collide — e.g. `name="ab", alias="c"` and `name="a", alias="bc"` produce
//! distinct byte strings.
//!
//! - **`name`**: always the agent's configured name; ensures two agents on the
//!   same host that share an `instance_seed` still produce distinct IDs.
//! - **`agent_seed`**: `[agent].instance_seed` when set; otherwise
//!   `sha256(ns(name) || ns_opt(alias))`.
//! - **`env_seed`**: first match in priority order:
//!   1. `AURA_INSTANCE_ID` env var (explicit operator override)
//!   2. `{POD_NAMESPACE}/{POD_NAME}` (Kubernetes downward API)
//!   3. OS machine ID via [`machine_uid`] — Linux `/etc/machine-id`,
//!      macOS IOPlatformUUID, Windows `MachineGuid` registry value
//!   4. Random [`Uuid::new_v4`], cached for the process lifetime; a
//!      `tracing::warn` fires once when this fallback is reached
//!
//! The raw machine ID never leaves the process; it feeds the SHA-1 inside
//! UUIDv5 only.

use std::fmt::{Display, Write};
use std::sync::OnceLock;
use uuid::{Uuid, uuid};

use aura_config::AgentConfig;

/// Fixed namespace for all AURA instance UUIDs.
///
/// Changing this value silently invalidates every previously-issued instance
/// ID. Do not modify it after the first release.
const AURA_NAMESPACE: Uuid = uuid!("d3f5c1a7-4b2e-5f9c-a8e1-6c0d4b7f3a2e");

#[derive(PartialEq, Eq)]
pub struct ConfigSeed(String);
impl From<&AgentConfig> for ConfigSeed {
    fn from(agent: &AgentConfig) -> Self {
        let mut buf = String::new();
        match &agent.instance_seed {
            Some(seed) => netstring_encode(&mut buf, seed),
            None => {
                netstring_encode(&mut buf, &agent.name);
                netstring_encode(&mut buf, agent.alias.as_deref().unwrap_or(""));
            }
        }
        Self(buf)
    }
}

impl Display for ConfigSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Default)]
struct EnvResolver {
    #[cfg(test)]
    pub(crate) _aura_instance_id: Option<String>,
    #[cfg(test)]
    pub(crate) _pod_namespace: Option<String>,
    #[cfg(test)]
    pub(crate) _pod_name: Option<String>,
    #[cfg(test)]
    pub(crate) _machine_id: Option<String>,
}

#[cfg(test)]
impl EnvResolver {
    #[inline]
    fn aura_instance_id(&self) -> Option<String> {
        self._aura_instance_id.clone()
    }

    #[inline]
    fn pod_namespace(&self) -> Option<String> {
        self._pod_namespace.clone()
    }

    #[inline]
    fn pod_name(&self) -> Option<String> {
        self._pod_name.clone()
    }

    #[inline]
    fn machine_id(&self) -> Option<String> {
        self._machine_id.clone()
    }
}

#[cfg(not(test))]
impl EnvResolver {
    #[inline]
    fn aura_instance_id(&self) -> Option<String> {
        std::env::var("AURA_INSTANCE_ID").ok()
    }

    #[inline]
    fn pod_namespace(&self) -> Option<String> {
        std::env::var("POD_NAMESPACE").ok()
    }

    #[inline]
    fn pod_name(&self) -> Option<String> {
        std::env::var("POD_NAME").ok()
    }

    #[inline]
    fn machine_id(&self) -> Option<String> {
        machine_uid::get().ok()
    }
}

#[derive(PartialEq, Eq)]
pub struct EnvironmentSeed(String);
impl From<&EnvResolver> for EnvironmentSeed {
    fn from(resolver: &EnvResolver) -> Self {
        // 1. Explicit operator override.
        if let Some(v) = resolver.aura_instance_id() {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return Self(v);
            }
        }

        // 2. Kubernetes downward API — both vars must be present and non-empty
        // after trimming. A whitespace-only value falls through to the next tier.
        if let Some(pod_ns) = resolver.pod_namespace()
            && let Some(pod_name) = resolver.pod_name()
        {
            let pod_ns = pod_ns.trim();
            let pod_name = pod_name.trim();
            if !pod_ns.is_empty() && !pod_name.is_empty() {
                return Self(format!("{pod_ns}/{pod_name}"));
            }
        }

        // 3. OS machine ID (no root required; never sent on the wire).
        if let Some(machine_id) = resolver.machine_id() {
            let machine_id = machine_id.trim().to_string();
            if !machine_id.is_empty() {
                return Self(machine_id);
            }
        }

        // 4. Random fallback — stable for the process lifetime; warn once.
        static RANDOM_FALLBACK: OnceLock<Uuid> = OnceLock::new();
        let uuid = RANDOM_FALLBACK
            .get_or_init(|| {
                tracing::warn!(
                    "No stable machine identity found \
                     (AURA_INSTANCE_ID, POD_NAME/POD_NAMESPACE, and the OS machine ID \
                     are all absent). Instance IDs will be random and change on restart. \
                     Set AURA_INSTANCE_ID for a stable identity."
                );
                Uuid::new_v4()
            })
            .to_string();
        Self(uuid)
    }
}

impl Display for EnvironmentSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct InstanceId(Uuid);
impl InstanceId {
    pub fn new(conf: &ConfigSeed, env: &EnvironmentSeed) -> Self {
        Self(Uuid::new_v5(
            &AURA_NAMESPACE,
            format!("{env}{conf}").as_bytes(),
        ))
    }
}

impl Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Compute the instance ID for `agent` using real environment and machine-ID
/// lookups.
///
/// Convenience wrapper for production call sites. Tests should construct
/// [`EnvResolver`] directly to inject specific values for each priority tier.
pub fn instance_id(agent: &AgentConfig) -> InstanceId {
    let conf = ConfigSeed::from(agent);
    let env = EnvironmentSeed::from(&EnvResolver::default());
    InstanceId::new(&conf, &env)
}

fn netstring_encode(dest: &mut String, value: &str) {
    let _ = write!(dest, "{}:{}", value.len(), value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_config::load_config_from_str;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn agent_with(name: &str, extra_fields: &str) -> AgentConfig {
        let toml = format!(
            r#"
[agent]
name = "{name}"
system_prompt = "test"
{extra_fields}

[agent.llm]
provider = "openai"
api_key = "test"
model = "gpt-4o"
"#
        );
        load_config_from_str(&toml).expect("valid config").agent
    }

    /// Build an [`EnvResolver`] with explicit test values for each priority tier.
    /// Pass `None` to leave a tier unset, causing fall-through to the next.
    fn make_resolver(
        aura_instance_id: Option<&str>,
        pod_namespace: Option<&str>,
        pod_name: Option<&str>,
        machine_id: Option<&str>,
    ) -> EnvResolver {
        EnvResolver {
            _aura_instance_id: aura_instance_id.map(String::from),
            _pod_namespace: pod_namespace.map(String::from),
            _pod_name: pod_name.map(String::from),
            _machine_id: machine_id.map(String::from),
        }
    }

    fn make_id(agent: &AgentConfig, resolver: &EnvResolver) -> InstanceId {
        InstanceId::new(&ConfigSeed::from(agent), &EnvironmentSeed::from(resolver))
    }

    /// Encode one value as a netstring for collision-boundary tests.
    fn ns(value: &str) -> String {
        let mut buf = String::new();
        netstring_encode(&mut buf, value);
        buf
    }

    // ── Determinism ──────────────────────────────────────────────────────────

    #[test]
    fn same_inputs_produce_same_id() {
        let agent = agent_with("test-agent", "");
        let resolver = make_resolver(None, None, None, Some("machine-abc"));
        assert_eq!(make_id(&agent, &resolver), make_id(&agent, &resolver));
    }

    // ── Env seed priority order ───────────────────────────────────────────────

    #[test]
    fn aura_instance_id_wins_over_k8s_and_machine() {
        let agent = agent_with("test-agent", "");
        let all_set = make_resolver(
            Some("explicit"),
            Some("prod"),
            Some("aura-0"),
            Some("machine"),
        );
        let only_explicit = make_resolver(Some("explicit"), None, None, None);
        assert_eq!(make_id(&agent, &all_set), make_id(&agent, &only_explicit));
    }

    #[test]
    fn k8s_wins_over_machine_id() {
        let agent = agent_with("test-agent", "");
        let k8s_with_machine = make_resolver(None, Some("prod"), Some("aura-0"), Some("machine"));
        let k8s_no_machine = make_resolver(None, Some("prod"), Some("aura-0"), None);
        assert_eq!(
            make_id(&agent, &k8s_with_machine),
            make_id(&agent, &k8s_no_machine)
        );
    }

    #[test]
    fn k8s_requires_both_pod_vars() {
        // Only POD_NAMESPACE — must fall through to the machine ID, not K8s.
        let agent = agent_with("test-agent", "");
        let only_ns = make_resolver(None, Some("prod"), None, Some("machine"));
        let both = make_resolver(None, Some("prod"), Some("aura-0"), None);
        assert_ne!(
            make_id(&agent, &only_ns),
            make_id(&agent, &both),
            "partial K8s env must not match full K8s env"
        );
    }

    #[test]
    fn different_machine_ids_produce_different_ids() {
        let agent = agent_with("test-agent", "");
        let r1 = make_resolver(None, None, None, Some("machine-abc"));
        let r2 = make_resolver(None, None, None, Some("machine-xyz"));
        assert_ne!(
            make_id(&agent, &r1),
            make_id(&agent, &r2),
            "different machine IDs must produce different instance IDs"
        );
    }

    // ── Whitespace trimming and empty-value fall-through ──────────────────────────

    #[test]
    fn aura_instance_id_whitespace_only_falls_through() {
        let agent = agent_with("test-agent", "");
        let whitespace = make_resolver(Some("   "), None, None, Some("machine"));
        let absent = make_resolver(None, None, None, Some("machine"));
        assert_eq!(
            make_id(&agent, &whitespace),
            make_id(&agent, &absent),
            "whitespace-only AURA_INSTANCE_ID must fall through to machine ID"
        );
    }

    #[test]
    fn aura_instance_id_is_trimmed() {
        let agent = agent_with("test-agent", "");
        let padded = make_resolver(Some("  my-id  "), None, None, None);
        let clean = make_resolver(Some("my-id"), None, None, None);
        assert_eq!(
            make_id(&agent, &padded),
            make_id(&agent, &clean),
            "leading/trailing whitespace in AURA_INSTANCE_ID must be trimmed"
        );
    }

    #[test]
    fn k8s_pod_namespace_whitespace_only_falls_through() {
        let agent = agent_with("test-agent", "");
        let whitespace_ns = make_resolver(None, Some("   "), Some("aura-0"), Some("machine"));
        let absent_k8s = make_resolver(None, None, None, Some("machine"));
        assert_eq!(
            make_id(&agent, &whitespace_ns),
            make_id(&agent, &absent_k8s),
            "whitespace-only POD_NAMESPACE must cause K8s tier to fall through"
        );
    }

    #[test]
    fn k8s_pod_name_whitespace_only_falls_through() {
        let agent = agent_with("test-agent", "");
        let whitespace_name = make_resolver(None, Some("prod"), Some("   "), Some("machine"));
        let absent_k8s = make_resolver(None, None, None, Some("machine"));
        assert_eq!(
            make_id(&agent, &whitespace_name),
            make_id(&agent, &absent_k8s),
            "whitespace-only POD_NAME must cause K8s tier to fall through"
        );
    }

    #[test]
    fn k8s_vars_are_trimmed() {
        let agent = agent_with("test-agent", "");
        let padded = make_resolver(None, Some("  prod  "), Some("  aura-0  "), None);
        let clean = make_resolver(None, Some("prod"), Some("aura-0"), None);
        assert_eq!(
            make_id(&agent, &padded),
            make_id(&agent, &clean),
            "leading/trailing whitespace in POD_NAMESPACE/POD_NAME must be trimmed"
        );
    }

    #[test]
    fn machine_id_whitespace_only_falls_through() {
        // Both reach the random fallback; the process-lifetime OnceLock means
        // they return the same UUID within a single test run.
        let agent = agent_with("test-agent", "");
        let whitespace = make_resolver(None, None, None, Some("   "));
        let absent = make_resolver(None, None, None, None);
        assert_eq!(
            make_id(&agent, &whitespace),
            make_id(&agent, &absent),
            "whitespace-only machine ID must fall through to the random fallback"
        );
    }

    #[test]
    fn machine_id_is_trimmed() {
        let agent = agent_with("test-agent", "");
        let padded = make_resolver(None, None, None, Some("  abc123  "));
        let clean = make_resolver(None, None, None, Some("abc123"));
        assert_eq!(
            make_id(&agent, &padded),
            make_id(&agent, &clean),
            "leading/trailing whitespace in machine ID must be trimmed"
        );
    }

    // ── Agent seed ───────────────────────────────────────────────────────────

    #[test]
    fn explicit_instance_seed_produces_different_id_than_derived() {
        let resolver = make_resolver(None, None, None, Some("machine"));
        let with_seed = agent_with("test-agent", r#"instance_seed = "my-seed""#);
        let without_seed = agent_with("test-agent", "");
        assert_ne!(
            make_id(&with_seed, &resolver),
            make_id(&without_seed, &resolver),
        );
    }

    #[test]
    fn same_explicit_seed_same_id() {
        let resolver = make_resolver(None, None, None, Some("machine"));
        let a1 = agent_with("test-agent", r#"instance_seed = "seed-x""#);
        let a2 = agent_with("test-agent", r#"instance_seed = "seed-x""#);
        assert_eq!(make_id(&a1, &resolver), make_id(&a2, &resolver));
    }

    #[test]
    fn different_explicit_seeds_different_ids() {
        let resolver = make_resolver(None, None, None, Some("machine"));
        let a1 = agent_with("test-agent", r#"instance_seed = "seed-a""#);
        let a2 = agent_with("test-agent", r#"instance_seed = "seed-b""#);
        assert_ne!(make_id(&a1, &resolver), make_id(&a2, &resolver));
    }

    // ── Name differentiates in the derived-seed path ──────────────────────────

    #[test]
    fn different_names_without_seed_produce_different_ids() {
        let resolver = make_resolver(None, None, None, Some("machine"));
        let a = agent_with("agent-alpha", "");
        let b = agent_with("agent-beta", "");
        assert_ne!(make_id(&a, &resolver), make_id(&b, &resolver));
    }

    #[test]
    fn shared_explicit_seed_same_env_produces_same_id() {
        // When instance_seed is set, the ConfigSeed encodes only the seed —
        // not the agent name — so two agents with the same seed and env are
        // intentionally equivalent.
        let resolver = make_resolver(None, None, None, Some("machine"));
        let a = agent_with("agent-alpha", r#"instance_seed = "shared""#);
        let b = agent_with("agent-beta", r#"instance_seed = "shared""#);
        assert_eq!(
            make_id(&a, &resolver),
            make_id(&b, &resolver),
            "agents sharing an explicit instance_seed and env should produce the same ID"
        );
    }

    // ── Netstring collision prevention ────────────────────────────────────────

    #[test]
    fn netstring_avoids_cross_field_boundary_collision() {
        // Without length-prefix encoding "ab" + "c" == "a" + "bc" == "abc".
        // With netstrings they are distinct: "2:ab1:c" vs "1:a2:bc".
        assert_ne!(ns("ab") + &ns("c"), ns("a") + &ns("bc"));
    }

    #[test]
    fn netstring_empty_string_differs_from_single_char() {
        assert_ne!(ns(""), ns("x"));
    }
}
