//! Tool for reading result artifacts from execution persistence.
//!
//! Re-exported from the artifact owner module in `persistence::artifacts`.

pub use crate::orchestration::persistence::artifacts::ReadArtifactTool;

#[cfg(test)]
pub use crate::orchestration::persistence::artifacts::{
    ReadArtifactArgs, ReadArtifactError, ReadArtifactOutput,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_compile() {
        // Ensure the facade re-exports are reachable with the expected names.
        let _: Option<ReadArtifactTool> = None;
        let _: Option<ReadArtifactArgs> = None;
        let _: Option<ReadArtifactOutput> = None;
        let _: Option<ReadArtifactError> = None;
    }
}
