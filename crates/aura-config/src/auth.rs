//! `[auth]` configuration: API-key authentication for the web server's
//! HTTP surface.
//!
//! Presence of the `[auth]` table is the enable bit (same posture as
//! `[hitl]`). Keys are stored as SHA-256 hashes so live secrets never sit in
//! the config file; the raw key is supplied by the client as an
//! `Authorization: Bearer` (or `x-api-key`) header and hashed at verify time.
//! Hash material can arrive via `{{ env.* }}` templating, mirroring how LLM
//! API keys are supplied.
//!
//! ```toml
//! [auth]
//! mode = "api-keys"
//!
//! [[auth.keys]]
//! name = "sre-team"
//! key_hash = "{{ env.AURA_KEY_SRE_SHA256 }}"
//! agents = ["incident-response"]        # optional allowlist, default ["*"]
//! ```

use crate::error::ConfigError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How the web server authenticates inbound HTTP requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthMode {
    /// Authentication is explicitly disabled.
    Off,
    /// Static bearer keys from [`AuthConfig::keys`].
    #[default]
    ApiKeys,
    /// OIDC bearer JWTs validated against [`AuthConfig::oidc`].
    Oidc,
}

/// The `[auth]` TOML table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Authentication mode. Defaults to `api-keys` when the table is present;
    /// set `"off"` to keep the table but disable auth (e.g. checked-in default
    /// overridden per environment).
    #[serde(default)]
    pub mode: AuthMode,
    /// The keyring. Required (non-empty) when `mode = "api-keys"`.
    #[serde(default)]
    pub keys: Vec<ApiKeyConfig>,
    /// OIDC settings. Required when `mode = "oidc"`, rejected otherwise.
    #[serde(default)]
    pub oidc: Option<OidcConfig>,
}

/// The `[auth.oidc]` TOML table: bearer JWTs from an external identity
/// provider, validated by signature (JWKS), issuer, and audience.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcConfig {
    /// Expected token issuer (`iss` claim). Also the OIDC discovery base when
    /// `jwks_url` is unset: `{issuer}/.well-known/openid-configuration`.
    pub issuer: String,
    /// Expected audience (`aud` claim; the token must list it).
    pub audience: String,
    /// Explicit JWKS endpoint. Defaults to the issuer's published
    /// `jwks_uri` from its discovery document.
    #[serde(default)]
    pub jwks_url: Option<String>,
    /// Claim whose value names the principal in logs and session namespacing
    /// (default: `"sub"`).
    #[serde(default = "default_principal_claim")]
    pub principal_claim: String,
    /// Agent names/aliases OIDC principals may invoke. `"*"` allows all.
    /// Defaults to `["*"]`; an explicit empty list allows none.
    #[serde(default = "default_agents")]
    pub allowed_agents: Vec<String>,
    /// Optional claim used to gate access (e.g. `"groups"` or `"roles"`):
    /// the claim's value must be this string, or a list containing one of
    /// [`OidcConfig::require_claim_values`].
    #[serde(default)]
    pub require_claim: Option<String>,
    /// Accepted values for [`OidcConfig::require_claim`]. Non-empty only
    /// when `require_claim` is set.
    #[serde(default)]
    pub require_claim_values: Vec<String>,
    /// Clock-skew allowance for `exp`/`nbf`, in seconds (default: 60).
    #[serde(default = "default_clock_skew_secs")]
    pub clock_skew_secs: u64,
    /// Minimum interval between JWKS refreshes, in seconds (default: 3600).
    /// An unknown `kid` in a token triggers an off-schedule refresh once per
    /// this window, which is how key rotation is picked up.
    #[serde(default = "default_jwks_refresh_secs")]
    pub jwks_refresh_secs: u64,
}

fn default_principal_claim() -> String {
    "sub".to_string()
}

fn default_clock_skew_secs() -> u64 {
    60
}

fn default_jwks_refresh_secs() -> u64 {
    3600
}

/// One `[[auth.keys]]` entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyConfig {
    /// Human-readable principal name. Appears in logs and namespaces the
    /// caller's chat sessions.
    pub name: String,
    /// SHA-256 of the raw key, as 64 hex characters (case-insensitive).
    pub key_hash: String,
    /// Previous key hash, still accepted while rotating. Lets an operator
    /// swap a key without a window where both clients and the server must
    /// change atomically.
    #[serde(default)]
    pub key_hash_previous: Option<String>,
    /// Agent names/aliases this key may invoke. `"*"` allows all agents.
    /// Defaults to `["*"]`; an explicit empty list allows none.
    #[serde(default = "default_agents")]
    pub agents: Vec<String>,
}

fn default_agents() -> Vec<String> {
    vec!["*".to_string()]
}

/// SHA-256 of a raw API key, lowercase hex — the form `key_hash` expects.
/// Used by operators minting hashes (`printf '%s' "$KEY" | sha256sum`
/// produces the same value) and by the verifier at request time.
pub fn hash_api_key(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decode a configured hash into its 32 raw bytes, rejecting anything that is
/// not 64 hex characters.
pub fn decode_key_hash(hash: &str) -> Result<[u8; 32], String> {
    let trimmed = hash.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "expected 64 hex characters (SHA-256), got {len} characters",
            len = trimmed.len()
        ));
    }
    let mut bytes = [0u8; 32];
    for (i, chunk) in trimmed.as_bytes().chunks(2).enumerate() {
        let chunk = std::str::from_utf8(chunk).expect("ascii hex");
        bytes[i] = u8::from_str_radix(chunk, 16).expect("validated hex");
    }
    Ok(bytes)
}

impl AuthConfig {
    /// Validate the table: a usable mode/keyring combination.
    pub fn validate(&self) -> Result<(), ConfigError> {
        match self.mode {
            AuthMode::Off => return Ok(()),
            AuthMode::ApiKeys => {
                if self.oidc.is_some() {
                    return Err(ConfigError::Validation(
                        "[auth.oidc] is only valid with mode = \"oidc\"".to_string(),
                    ));
                }
            }
            AuthMode::Oidc => {
                if !self.keys.is_empty() {
                    return Err(ConfigError::Validation(
                        "[[auth.keys]] is only valid with mode = \"api-keys\"".to_string(),
                    ));
                }
                return self
                    .oidc
                    .as_ref()
                    .ok_or_else(|| {
                        ConfigError::Validation(
                            "[auth] mode = \"oidc\" requires a [auth.oidc] table".to_string(),
                        )
                    })
                    .and_then(|o| o.validate());
            }
        }
        if self.keys.is_empty() {
            return Err(ConfigError::Validation(
                "[auth] mode = \"api-keys\" requires at least one [[auth.keys]] entry".to_string(),
            ));
        }
        let mut names = std::collections::HashSet::new();
        for (i, key) in self.keys.iter().enumerate() {
            if key.name.trim().is_empty() {
                return Err(ConfigError::Validation(format!(
                    "[auth.keys[{i}]].name must not be empty"
                )));
            }
            if !names.insert(key.name.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate [auth.keys] name '{}'",
                    key.name
                )));
            }
            decode_key_hash(&key.key_hash)
                .map_err(|e| ConfigError::Validation(format!("[auth.keys[{i}]].key_hash {e}")))?;
            if let Some(prev) = &key.key_hash_previous {
                decode_key_hash(prev).map_err(|e| {
                    ConfigError::Validation(format!("[auth.keys[{i}]].key_hash_previous {e}"))
                })?;
            }
            for agent in &key.agents {
                if agent.trim().is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "[auth.keys[{i}]].agents must not contain empty entries"
                    )));
                }
            }
        }
        Ok(())
    }
}

impl OidcConfig {
    /// Validate the OIDC settings: parseable URLs and a coherent claim gate.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_http_url("issuer", &self.issuer)?;
        if let Some(url) = &self.jwks_url {
            validate_http_url("jwks_url", url)?;
        }
        if self.audience.trim().is_empty() {
            return Err(ConfigError::Validation(
                "[auth.oidc].audience must not be empty".to_string(),
            ));
        }
        if self.principal_claim.trim().is_empty() {
            return Err(ConfigError::Validation(
                "[auth.oidc].principal_claim must not be empty".to_string(),
            ));
        }
        if self.require_claim.is_none() && !self.require_claim_values.is_empty() {
            return Err(ConfigError::Validation(
                "[auth.oidc].require_claim_values requires require_claim to be set".to_string(),
            ));
        }
        for agent in &self.allowed_agents {
            if agent.trim().is_empty() {
                return Err(ConfigError::Validation(
                    "[auth.oidc].allowed_agents must not contain empty entries".to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// Reject anything that is not an absolute http(s) URL with a host. Plaintext
/// http is allowed (a local test IdP is legitimate) but the web server warns
/// at startup when it points off-localhost.
fn validate_http_url(field: &str, url: &str) -> Result<(), ConfigError> {
    let parsed = url.parse::<http::Uri>().map_err(|e| {
        ConfigError::Validation(format!("[auth.oidc].{field} is not a valid URL: {e}"))
    })?;
    let scheme = parsed.scheme_str().unwrap_or_default();
    if scheme != "http" && scheme != "https" {
        return Err(ConfigError::Validation(format!(
            "[auth.oidc].{field} must be an http(s) URL, got scheme {scheme:?}"
        )));
    }
    if parsed.host().is_none() {
        return Err(ConfigError::Validation(format!(
            "[auth.oidc].{field} must be an absolute URL with a host"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toml_with_keys(keys: &str) -> String {
        format!(
            r#"
[agent]
name = "test"
system_prompt = "test"

[agent.llm]
provider = "ollama"
model = "test"

[auth]
{keys}
"#
        )
    }

    const GOOD_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// Parse TOML then run full config validation, unifying both error types.
    fn parse_and_validate(src: &str) -> Result<crate::Config, String> {
        let config: crate::Config = toml::from_str(src).map_err(|e| e.to_string())?;
        config.validate().map_err(|e| e.to_string())?;
        Ok(config)
    }

    #[test]
    fn hash_api_key_matches_known_sha256() {
        // sha256("aura") — cross-checked against sha256sum.
        assert_eq!(
            hash_api_key("aura"),
            "6d7c0eb420852d2e6288be29c47c975ff15d81eeacd4eef100022f931a827702"
        );
        assert_eq!(hash_api_key(""), GOOD_HASH);
    }

    #[test]
    fn parses_minimal_table_with_defaults() {
        let toml = toml_with_keys(&format!(
            r#"
[[auth.keys]]
name = "ops"
key_hash = "{GOOD_HASH}"
"#
        ));
        let config = parse_and_validate(&toml).unwrap();
        let auth = config.auth.as_ref().expect("auth table");
        assert_eq!(auth.mode, AuthMode::ApiKeys);
        assert_eq!(auth.keys.len(), 1);
        assert_eq!(auth.keys[0].agents, vec!["*".to_string()]);
        assert!(auth.keys[0].key_hash_previous.is_none());
    }

    #[test]
    fn mode_off_is_valid_without_keys() {
        let toml = toml_with_keys("mode = \"off\"\n");
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.auth.unwrap().mode, AuthMode::Off);
    }

    #[test]
    fn api_keys_mode_requires_a_key() {
        let toml = toml_with_keys("mode = \"api-keys\"\n");
        let err = parse_and_validate(&toml).expect_err("empty keyring must fail");
        assert!(err.contains("at least one"), "{err}");
    }

    #[test]
    fn rejects_malformed_hash() {
        let toml = toml_with_keys(
            r#"
[[auth.keys]]
name = "ops"
key_hash = "not-a-hash"
"#,
        );
        let err = parse_and_validate(&toml).expect_err("bad hash must fail");
        assert!(err.contains("64 hex"), "{err}");
    }

    #[test]
    fn rejects_duplicate_names() {
        let toml = toml_with_keys(&format!(
            r#"
[[auth.keys]]
name = "ops"
key_hash = "{GOOD_HASH}"

[[auth.keys]]
name = "ops"
key_hash = "{GOOD_HASH}"
"#
        ));
        let err = parse_and_validate(&toml).expect_err("duplicate names must fail");
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn accepts_uppercase_hash_and_previous_rotation() {
        let toml = toml_with_keys(&format!(
            r#"
[[auth.keys]]
name = "ops"
key_hash = "{}"
key_hash_previous = "{GOOD_HASH}"
agents = ["a", "b"]
"#,
            GOOD_HASH.to_uppercase()
        ));
        let config = parse_and_validate(&toml).unwrap();
        assert_eq!(config.auth.unwrap().keys[0].agents, vec!["a", "b"]);
    }

    #[test]
    fn decode_key_hash_round_trips() {
        let bytes = decode_key_hash(GOOD_HASH).unwrap();
        assert_eq!(bytes[0], 0xe3);
        assert_eq!(bytes[31], 0x55);
        assert!(decode_key_hash(&GOOD_HASH[..63]).is_err());
        assert!(decode_key_hash(&format!("{}g", &GOOD_HASH[..63])).is_err());
    }

    // -------------------------------------------------------------------
    // [auth.oidc]
    // -------------------------------------------------------------------

    fn oidc_toml(body: &str) -> String {
        toml_with_keys(&format!("mode = \"oidc\"\n\n[auth.oidc]\n{body}"))
    }

    #[test]
    fn oidc_parses_with_defaults() {
        let toml = oidc_toml(
            r#"
issuer = "https://idp.example.com/"
audience = "aura"
"#,
        );
        let config = parse_and_validate(&toml).unwrap();
        let oidc = config.auth.unwrap().oidc.unwrap();
        assert_eq!(oidc.principal_claim, "sub");
        assert_eq!(oidc.allowed_agents, vec!["*".to_string()]);
        assert_eq!(oidc.clock_skew_secs, 60);
        assert_eq!(oidc.jwks_refresh_secs, 3600);
        assert!(oidc.jwks_url.is_none());
    }

    #[test]
    fn oidc_requires_the_table() {
        let toml = toml_with_keys("mode = \"oidc\"\n");
        let err = parse_and_validate(&toml).expect_err("missing [auth.oidc] must fail");
        assert!(err.contains("requires a [auth.oidc] table"), "{err}");
    }

    #[test]
    fn oidc_rejects_keys_and_apikeys_rejects_oidc() {
        let toml = toml_with_keys(&format!(
            "mode = \"oidc\"\n\n[[auth.keys]]\nname = \"x\"\nkey_hash = \"{GOOD_HASH}\"\n\n[auth.oidc]\nissuer = \"https://idp.example.com\"\naudience = \"aura\"\n"
        ));
        let err = parse_and_validate(&toml).expect_err("keys with oidc must fail");
        assert!(err.contains("only valid with mode"), "{err}");

        let toml = toml_with_keys(&format!(
            "[[auth.keys]]\nname = \"ops\"\nkey_hash = \"{GOOD_HASH}\"\n\n[auth.oidc]\nissuer = \"https://idp.example.com\"\naudience = \"aura\"\n"
        ));
        let err = parse_and_validate(&toml).expect_err("oidc table with api-keys must fail");
        assert!(err.contains("only valid with mode"), "{err}");
    }

    #[test]
    fn oidc_rejects_bad_urls_and_empty_audience() {
        let err = parse_and_validate(&oidc_toml("issuer = \"not-a-url\"\naudience = \"aura\"\n"))
            .expect_err("bad issuer must fail");
        assert!(err.contains("issuer"), "{err}");

        let err = parse_and_validate(&oidc_toml(
            "issuer = \"ftp://idp.example.com\"\naudience = \"aura\"\n",
        ))
        .expect_err("non-http scheme must fail");
        assert!(err.contains("scheme"), "{err}");

        let err = parse_and_validate(&oidc_toml(
            "issuer = \"https://idp.example.com\"\naudience = \"\"\n",
        ))
        .expect_err("empty audience must fail");
        assert!(err.contains("audience"), "{err}");
    }

    #[test]
    fn oidc_claim_gate_requires_a_claim_name() {
        let err = parse_and_validate(&oidc_toml(
            r#"
issuer = "https://idp.example.com"
audience = "aura"
require_claim_values = ["sre"]
"#,
        ))
        .expect_err("values without a claim name must fail");
        assert!(err.contains("require_claim"), "{err}");
    }
}
