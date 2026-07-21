//! Forwarded-header classification for parkable runs (ADR 2026-07-21,
//! decision 13).

use serde::{Deserialize, Serialize};

/// Per-header TOML classification of a forwarded request header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeaderClass {
    /// A reified user id behind a trusted gateway, safe to persist in a
    /// checkpoint.
    Identity,
    /// A secret. The default classification.
    Credential,
}

/// Where the credential for an outbound call comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSource {
    StaticConfig,
    RequestForwarded,
    ServiceIdentity,
    BrokeredDelegation,
}

/// An identity-classified header captured in the checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityHeader {
    pub name: String,
    pub value: String,
}

/// Park refusal: the run holds a credential-classified header, which is
/// never persisted, so the run cannot park (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnparkableCredential {
    pub header: String,
}

impl std::fmt::Display for UnparkableCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "run holds credential-classified header '{}' and cannot park",
            self.header
        )
    }
}

impl std::error::Error for UnparkableCredential {}
