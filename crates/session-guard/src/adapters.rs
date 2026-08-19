//! Admission backends behind [`crate::TurnAdmission`]. `LocalAdmission`
//! serves single-instance deployments (and the `off` mode);
//! `ClaimFileAdmission` serves multi-instance deployments against a
//! shared Archil disk. Constructed only through
//! [`crate::build_admission`], which shares one arbiter and one instance
//! identity across the process. The acting instance is always the
//! backend's own - callers never supply it.
//!
//! Each backend module also defines a zero-sized proof token whose
//! constructor is private to that module. The matching
//! [`crate::state::AcquiredClaim`] constructor demands the token, so the
//! backend/lease pairing (local -> static, claim-file -> heartbeat) is
//! structural: `ClaimFileAdmission` cannot construct
//! [`local::LocalLeaseProof`], and vice versa.

pub(crate) mod claim_file;
pub(crate) mod local;

pub use claim_file::ClaimFileAdmission;
pub use local::LocalAdmission;
