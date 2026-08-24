//! Admission backends behind [`crate::TurnAdmission`]. `LocalAdmission`
//! serves single-instance deployments (and the `off` mode);
//! `PgAdmission` serves multi-instance deployments against the Postgres
//! claims authority. Constructed only through [`crate::build_admission`],
//! which shares one arbiter and one pod identity across the process. The
//! acting pod is always the backend's own — callers never supply it.
//!
//! Each backend module also defines a zero-sized proof token whose
//! constructor is private to that module. The matching
//! [`crate::state::AcquiredClaim`] constructor demands the token, so the
//! backend/lease pairing (local -> static, pg -> heartbeat) is
//! structural: `PgAdmission` cannot construct [`local::LocalLeaseProof`],
//! and vice versa.

pub(crate) mod local;
pub(crate) mod pg;

pub use local::LocalAdmission;
pub use pg::PgAdmission;
