//! The park admission surface: the validated relation between the decision
//! route, park mode, orchestration, and the retention age.
//!
//! The channel contract fixes exclusivity by construction: conversational and
//! webhook-sync routes never durable-park, and webhook-poll delivery parks only
//! with orchestration enabled. [`ParkRouteAdmission`] is the one admitted
//! vocabulary — a parking deployment and a non-parking route — so an
//! inadmissible combination cannot travel past validation as data; it can only
//! be rejected as [`ParkAdmissionError`].
//!
//! The retention age ([`ParkTtl`]) is disk retention, separate from each
//! approval's route timeout: park mode requires a nonzero age at least as long
//! as the route timeout. The absolute retention deadline arithmetic (checked
//! addition against a publication timestamp) lives with the checkpoint commit
//! in the `aura` crate; this module owns only the age and the relation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::HitlConfig;

/// A validated park retention age in seconds: nonzero by construction.
///
/// This is disk retention (`[hitl.park].park_ttl`), not a decision window:
/// each approval's deadline stays derived from the route timeout, and the two
/// are compared — never conflated — at admission. A configured age of zero
/// fails config parsing here, so no downstream code re-checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct ParkTtl(u64);

/// A retention age of zero: the only invalid raw value, refused at
/// construction so no downstream code re-checks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("park retention age must be greater than zero")]
pub struct ParkTtlZero;

impl Default for ParkTtl {
    /// The channel contract's default retention: one hour of disk evidence.
    fn default() -> Self {
        Self(3600)
    }
}

impl TryFrom<u64> for ParkTtl {
    type Error = ParkTtlZero;

    fn try_from(secs: u64) -> Result<Self, Self::Error> {
        Self::try_new(secs)
    }
}

impl From<ParkTtl> for u64 {
    fn from(ttl: ParkTtl) -> Self {
        ttl.0
    }
}

impl ParkTtl {
    /// Accept any nonzero age in seconds.
    pub fn try_new(secs: u64) -> Result<Self, ParkTtlZero> {
        if secs == 0 {
            Err(ParkTtlZero)
        } else {
            Ok(Self(secs))
        }
    }

    /// The age in seconds.
    #[must_use]
    pub fn as_secs(&self) -> u64 {
        self.0
    }
}

/// The route timeout in seconds, as a labeled carrier.
///
/// Parity with `[hitl.route]`'s raw `timeout_secs` fields; any zero policy
/// belongs to the admission relation, not to this wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RouteTimeoutSecs(u64);

impl RouteTimeoutSecs {
    /// Wrap the configured route timeout.
    #[must_use]
    pub fn new(secs: u64) -> Self {
        Self(secs)
    }

    /// The timeout in seconds.
    #[must_use]
    pub fn as_secs(&self) -> u64 {
        self.0
    }
}

/// The admitted parking-route payload: the validated retention age and the
/// route timeout it dominates. Fields are private so the
/// `park_ttl >= route.timeout_secs` relation cannot be forged — the only
/// constructor is the body of [`validate_park_admission`], the one admission
/// authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmittedParkRoute {
    park_ttl: ParkTtl,
    route_timeout: RouteTimeoutSecs,
}

impl AdmittedParkRoute {
    /// Wrap the pair after admission established the relation. Private to
    /// this module: only [`validate_park_admission`] constructs the
    /// payload.
    #[expect(
        dead_code,
        reason = "constructed only by validate_park_admission's R1 fill body"
    )]
    fn from_admission(park_ttl: ParkTtl, route_timeout: RouteTimeoutSecs) -> Self {
        Self {
            park_ttl,
            route_timeout,
        }
    }

    /// The validated `[hitl.park].park_ttl`, proven to cover the route
    /// timeout.
    #[must_use]
    pub fn park_ttl(&self) -> ParkTtl {
        self.park_ttl
    }

    /// The route timeout the retention age was proven to cover.
    #[must_use]
    pub fn route_timeout(&self) -> RouteTimeoutSecs {
        self.route_timeout
    }
}

/// An admitted route/park combination. The two variants are the whole legal
/// vocabulary: a webhook-poll parking deployment with orchestration, or a
/// route that never durable-parks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkRouteAdmission {
    /// Webhook-poll delivery with park mode and orchestration enabled — the
    /// only supported durable-park path. Carries the admitted payload
    /// proving the retention age covers the route timeout.
    PollOrchestration(AdmittedParkRoute),
    /// A route that never durable-parks: conversational inline approval or a
    /// held webhook-sync POST.
    NonParking(NonParkingRoute),
}

/// Which non-parking channel a deployment runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonParkingRoute {
    /// Attended: inline prompt plus the `/v1/approvals` decision endpoint.
    Conversational,
    /// Unattended: one held governance POST carrying the decision.
    WebhookSync,
}

/// Why a route/park combination is not admissible. Variants carry the
/// validated inputs they rule on, never bare seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ParkAdmissionError {
    /// `park.enabled = true` on the conversational route, which never
    /// durable-parks.
    #[error(
        "`hitl.park.enabled = true` is not supported on the conversational route: approvals stay inline"
    )]
    ParkWithConversational,
    /// `park.enabled = true` on webhook-sync delivery, which holds one POST
    /// and never parks.
    #[error(
        "`hitl.park.enabled = true` is not supported on webhook sync delivery: the request is held, not parked"
    )]
    ParkWithWebhookSync,
    /// Poll delivery without orchestration: single-agent mode has no
    /// park/reify path.
    #[error(
        "webhook poll delivery requires orchestration: single-agent mode has no park/reify path"
    )]
    PollWithoutOrchestration,
    /// Poll delivery without park mode: parked approvals may be long-lived
    /// and requests are never held open.
    #[error("webhook poll delivery requires `hitl.park.enabled = true`")]
    PollWithoutPark,
    /// The retention age does not cover the route timeout, so evidence could
    /// outlive its retention window.
    #[error(
        "park retention age {}s is shorter than the route timeout {}s: retention must cover the decision window",
        park_ttl.as_secs(),
        route_timeout.as_secs()
    )]
    TtlBelowRouteTimeout {
        /// The validated retention age.
        park_ttl: ParkTtl,
        /// The route timeout it failed to cover.
        route_timeout: RouteTimeoutSecs,
    },
}

/// Validate the route/park/orchestration relation and the retention age.
///
/// The complete admission authority: every exclusivity rule of the channel
/// contract resolves here, so runtime park admission downstream re-derives
/// from [`ParkRouteAdmission`] instead of re-checking raw config. The
/// retention age is `[hitl.park].park_ttl` on `hitl`; orchestration
/// enablement is the caller's resolved mode flag.
#[expect(
    unused_variables,
    reason = "todo!() body; filled by P45 wave fill units"
)]
pub fn validate_park_admission(
    hitl: &HitlConfig,
    orchestration_enabled: bool,
) -> Result<ParkRouteAdmission, ParkAdmissionError> {
    todo!("P45 wave fill unit R1: mode/park/orchestration validation and the park_ttl relation")
}

/// The admission matrix: each case satisfies or violates exactly one
/// channel-contract rule, asserted as the complete typed outcome.
#[cfg(test)]
mod tests {
    use super::{NonParkingRoute, ParkAdmissionError, ParkRouteAdmission, validate_park_admission};
    use crate::config::HitlConfig;

    const CONVERSATIONAL_ROUTE: &str = "mode = \"conversational\"\ntimeout_secs = 300";
    const SYNC_ROUTE: &str = "mode = \"webhook\"\n\
                              url = \"https://approvals.example.com/decide\"\n\
                              delivery = \"sync\"\n\
                              timeout_secs = 300";
    const POLL_ROUTE: &str = "mode = \"webhook\"\n\
                              url = \"https://approvals.example.com/decide\"\n\
                              delivery = \"poll\"\n\
                              timeout_secs = 300";

    /// Parse a real `[hitl]` TOML shape: the given `[route]` body plus the
    /// park table carrying the retention age. Orchestration is the caller's
    /// resolved mode flag.
    fn hitl_config(route_body: &str, park_enabled: bool, park_ttl: u64) -> HitlConfig {
        let toml = format!(
            "require_approval = [\"kubectl_*\"]\n\n\
             [route]\n{route_body}\n\n\
             [park]\nenabled = {park_enabled}\npark_ttl = {park_ttl}\n"
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn park_admission_conversational_non_park() {
        let hitl = hitl_config(CONVERSATIONAL_ROUTE, false, 3600);
        assert_eq!(
            validate_park_admission(&hitl, false),
            Ok(ParkRouteAdmission::NonParking(
                NonParkingRoute::Conversational
            ))
        );
    }

    #[test]
    fn park_admission_sync_non_park() {
        let hitl = hitl_config(SYNC_ROUTE, false, 3600);
        assert_eq!(
            validate_park_admission(&hitl, false),
            Ok(ParkRouteAdmission::NonParking(NonParkingRoute::WebhookSync))
        );
    }

    #[test]
    fn park_admission_poll_orchestration_park() {
        // Retention equal to the route timeout is the admitted boundary.
        let hitl = hitl_config(POLL_ROUTE, true, 300);
        let admitted = match validate_park_admission(&hitl, true) {
            Ok(ParkRouteAdmission::PollOrchestration(admitted)) => admitted,
            other => panic!("expected an admitted poll route, got {other:?}"),
        };
        assert_eq!(admitted.park_ttl().as_secs(), 300);
        assert_eq!(admitted.route_timeout().as_secs(), 300);
    }

    #[test]
    fn park_admission_rejects_conversational_park() {
        let hitl = hitl_config(CONVERSATIONAL_ROUTE, true, 3600);
        assert_eq!(
            validate_park_admission(&hitl, true),
            Err(ParkAdmissionError::ParkWithConversational)
        );
    }

    #[test]
    fn park_admission_rejects_sync_park() {
        let hitl = hitl_config(SYNC_ROUTE, true, 3600);
        assert_eq!(
            validate_park_admission(&hitl, true),
            Err(ParkAdmissionError::ParkWithWebhookSync)
        );
    }

    #[test]
    fn park_admission_rejects_poll_without_orchestration() {
        let hitl = hitl_config(POLL_ROUTE, true, 3600);
        assert_eq!(
            validate_park_admission(&hitl, false),
            Err(ParkAdmissionError::PollWithoutOrchestration)
        );
    }

    #[test]
    fn park_admission_rejects_poll_without_park() {
        let hitl = hitl_config(POLL_ROUTE, false, 3600);
        assert_eq!(
            validate_park_admission(&hitl, true),
            Err(ParkAdmissionError::PollWithoutPark)
        );
    }

    #[test]
    fn park_admission_rejects_ttl_below_timeout() {
        let hitl = hitl_config(POLL_ROUTE, true, 299);
        let (park_ttl, route_timeout) = match validate_park_admission(&hitl, true) {
            Err(ParkAdmissionError::TtlBelowRouteTimeout {
                park_ttl,
                route_timeout,
            }) => (park_ttl, route_timeout),
            other => panic!("expected a TTL-below-timeout rejection, got {other:?}"),
        };
        assert_eq!((park_ttl.as_secs(), route_timeout.as_secs()), (299, 300));
    }
}
