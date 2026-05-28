//! Sealed allow-list of property value types.
//!
//! Every property a telemetry event can carry must be one of these
//! variants. There is no `String` or `Free(_)` variant by design — to add
//! a new free-form value, a new typed variant must be added here, and the
//! PR adding it must update the public event table in `docs/telemetry.md`
//! per the reviewer checklist.
//!
//! Phase 1 ships a minimal allow-list; later phases extend it.

use std::collections::HashMap;

/// Coarse-grained operating-system family. Never sends arch, kernel
/// version, or distro — too fingerprinty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsFamily {
    Linux,
    Macos,
    Windows,
    Other,
}

impl OsFamily {
    /// Detect the current OS family at compile time.
    pub const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
            Self::Other => "other",
        }
    }
}

/// Where the event was emitted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    WebServer,
    Cli,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WebServer => "web-server",
            Self::Cli => "cli",
        }
    }
}

/// Coarse deployment-method tag (mirrors OpenSRE's `deployment_method`).
/// Settable via `AURA_DEPLOYMENT_METHOD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeploymentMethod {
    Local,
    Docker,
    K8s,
    StandaloneCli,
    Other,
}

impl DeploymentMethod {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("docker") => Self::Docker,
            Some("k8s") | Some("kubernetes") => Self::K8s,
            Some("standalone-cli") | Some("standalone_cli") => Self::StandaloneCli,
            Some("local") | None | Some("") => Self::Local,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Docker => "docker",
            Self::K8s => "k8s",
            Self::StandaloneCli => "standalone-cli",
            Self::Other => "other",
        }
    }
}

/// The Phase-1 sealed allow-list of values that may appear in an event's
/// **property map** (i.e. the per-event key/value bag that PostHog
/// receives under `properties`).
///
/// Envelope-level values that must never be repeated in the property map
/// — most importantly the install UUID, which is the top-level
/// `distinct_id` — live on the (private) envelope type instead. Keeping
/// them out of `PropertyValue` makes the rule structural: a
/// `#[derive(Event)]` struct cannot carry an install UUID as a field
/// because there is no type it could declare to do so.
///
/// Variants are added together with the docs/telemetry.md row that
/// justifies them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyValue {
    Bool(bool),
    OsFamily(OsFamily),
    Source(Source),
    DeploymentMethod(DeploymentMethod),
    /// Static-str event name or version — never holds runtime user input.
    Static(&'static str),
    /// `env!("CARGO_PKG_VERSION")` value; resolved at compile time.
    AuraVersion,
    /// Session UUID — random per-process; not linked to install identity.
    SessionUuid(uuid::Uuid),
}

impl PropertyValue {
    /// Render the property to a JSON value for the wire payload.
    pub fn to_json(&self) -> serde_json::Value {
        use serde_json::Value;
        match self {
            Self::Bool(b) => Value::Bool(*b),
            Self::OsFamily(v) => Value::String(v.as_str().into()),
            Self::Source(v) => Value::String(v.as_str().into()),
            Self::DeploymentMethod(v) => Value::String(v.as_str().into()),
            Self::Static(s) => Value::String((*s).into()),
            Self::AuraVersion => Value::String(env!("CARGO_PKG_VERSION").into()),
            Self::SessionUuid(u) => Value::String(u.to_string()),
        }
    }
}

/// Trait gating which Rust types are allowed as event-struct fields.
///
/// Implemented only for the typed allow-list (booleans + the per-domain
/// enums in this module). Notably **NOT implemented for `String`,
/// `&str`, `i32`, `u64`, `serde_json::Value`, or any other free-form
/// type.** The `#[derive(Event)]` macro emits code that calls
/// `.into_telemetry_property()` on every field; a field whose type does
/// not implement this trait fails to compile with a message naming the
/// trait. That is the compile-time anti-PII gate referenced by the spec.
///
/// To add a new field type: extend [`PropertyValue`] with a new variant
/// AND add an `IntoTelemetryProperty` impl for the source Rust type,
/// AND add a row to `docs/telemetry.md`. All three should be in the
/// same PR.
pub trait IntoTelemetryProperty {
    fn into_telemetry_property(self) -> PropertyValue;
}

impl IntoTelemetryProperty for bool {
    fn into_telemetry_property(self) -> PropertyValue {
        PropertyValue::Bool(self)
    }
}
impl IntoTelemetryProperty for OsFamily {
    fn into_telemetry_property(self) -> PropertyValue {
        PropertyValue::OsFamily(self)
    }
}
impl IntoTelemetryProperty for Source {
    fn into_telemetry_property(self) -> PropertyValue {
        PropertyValue::Source(self)
    }
}
impl IntoTelemetryProperty for DeploymentMethod {
    fn into_telemetry_property(self) -> PropertyValue {
        PropertyValue::DeploymentMethod(self)
    }
}
impl IntoTelemetryProperty for &'static str {
    fn into_telemetry_property(self) -> PropertyValue {
        PropertyValue::Static(self)
    }
}

// IntoTelemetryProperty is deliberately NOT implemented for
// PropertyValue itself. Allowing it would mean a future `PropertyValue`
// variant intended only for the envelope (e.g. an install UUID) could
// be smuggled into an event's property map via a struct field of type
// `PropertyValue`. Forcing every field to be one of the typed source
// types keeps the allow-list a structural property of the codebase
// instead of a convention. The compile_fail test
// `property_value_field.rs` locks this in.

/// Marker newtype for a session UUID. Use this on event fields to avoid
/// ambiguity with install UUID (different envelope position, different
/// audit semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionUuid(pub uuid::Uuid);

impl IntoTelemetryProperty for SessionUuid {
    fn into_telemetry_property(self) -> PropertyValue {
        PropertyValue::SessionUuid(self.0)
    }
}

/// A bag of allow-listed properties for one event.
#[derive(Debug, Default, Clone)]
pub struct Properties(HashMap<&'static str, PropertyValue>);

impl Properties {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&mut self, key: &'static str, value: PropertyValue) {
        self.0.insert(key, value);
    }
    pub fn get(&self, key: &str) -> Option<&PropertyValue> {
        self.0.get(key)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &PropertyValue)> {
        self.0.iter().map(|(k, v)| (*k, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn os_family_renders_lowercase() {
        assert_eq!(OsFamily::Linux.as_str(), "linux");
        assert_eq!(OsFamily::Macos.as_str(), "macos");
        assert_eq!(OsFamily::Windows.as_str(), "windows");
        assert_eq!(OsFamily::Other.as_str(), "other");
    }

    #[test]
    fn deployment_method_parse_defaults_to_local() {
        assert_eq!(DeploymentMethod::parse(None), DeploymentMethod::Local);
        assert_eq!(DeploymentMethod::parse(Some("")), DeploymentMethod::Local);
        assert_eq!(
            DeploymentMethod::parse(Some("local")),
            DeploymentMethod::Local
        );
    }

    #[test]
    fn deployment_method_parse_known_values() {
        assert_eq!(
            DeploymentMethod::parse(Some("docker")),
            DeploymentMethod::Docker
        );
        assert_eq!(
            DeploymentMethod::parse(Some("Kubernetes")),
            DeploymentMethod::K8s
        );
        assert_eq!(
            DeploymentMethod::parse(Some("k8s")),
            DeploymentMethod::K8s
        );
        assert_eq!(
            DeploymentMethod::parse(Some("standalone-cli")),
            DeploymentMethod::StandaloneCli
        );
    }

    #[test]
    fn deployment_method_unknown_maps_to_other() {
        assert_eq!(
            DeploymentMethod::parse(Some("ec2")),
            DeploymentMethod::Other
        );
    }

    #[test]
    fn property_to_json_is_string_for_enums() {
        assert_eq!(
            PropertyValue::OsFamily(OsFamily::Linux).to_json(),
            json!("linux")
        );
        assert_eq!(
            PropertyValue::Source(Source::WebServer).to_json(),
            json!("web-server")
        );
    }

    #[test]
    fn property_to_json_static_str() {
        assert_eq!(PropertyValue::Static("http").to_json(), json!("http"));
    }

    #[test]
    fn aura_version_renders_cargo_pkg_version() {
        let v = PropertyValue::AuraVersion.to_json();
        // The literal value is whatever Cargo.toml says; we only check
        // the shape — it's a non-empty string.
        let s = v.as_str().expect("AuraVersion should serialize as string");
        assert!(!s.is_empty());
    }
}
