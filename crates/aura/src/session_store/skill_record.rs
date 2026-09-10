//! The storage form of a skill-tool invocation: which skill tool a session's
//! LLM called, with what arguments, and where in the client-visible
//! conversation history it fired.
//!
//! Records deliberately hold only the invocation, never the skill content —
//! skill files ship with the agent config on every instance, so rehydration
//! re-reads them from disk (`crate::skill_rehydration`) and the store stays
//! small and never serves stale content.
//!
//! Records are keyed by the client-supplied chat session id, the same trust
//! boundary conversational approvals and A2A context history rely on; see
//! `docs/design/session-storage.md` §7 for what a caller holding an id can do.

use serde::{Deserialize, Serialize};

use crate::skill_tool::{LOAD_SKILL_TOOL_NAME, READ_SKILL_FILE_TOOL_NAME};

/// Record schema version.
pub const SKILL_INVOCATION_RECORD_VERSION: u32 = 1;

/// Round-trippable storage form of one skill-tool invocation. Field and tag
/// names are a persisted contract shared by every instance reading the store —
/// rename only with a migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInvocationRecord {
    pub version: u32,
    pub invocation: SkillInvocation,
    /// Tool-call id pairing this invocation's call with its result.
    pub tool_call_id: String,
    /// Position in the client-visible conversation history where this
    /// invocation fired.
    pub anchor: u32,
    /// Order among invocations sharing an anchor.
    pub seq: u32,
    pub invoked_at: chrono::DateTime<chrono::Utc>,
}

/// Why a stored record could not be restored.
#[derive(Debug, thiserror::Error)]
pub enum SkillRecordDecodeError {
    #[error("skill invocation record is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("skill invocation record version {found} is not supported (expected {supported})")]
    Version { found: u32, supported: u32 },
}

impl SkillInvocationRecord {
    /// Restore a record from its stored JSON. The version is checked before
    /// the full parse, so a record written on another schema is refused by
    /// version rather than by whichever field happens to differ.
    pub fn decode(json: &str) -> Result<Self, SkillRecordDecodeError> {
        #[derive(Deserialize)]
        struct VersionProbe {
            version: u32,
        }
        let probe: VersionProbe = serde_json::from_str(json)?;
        if probe.version != SKILL_INVOCATION_RECORD_VERSION {
            return Err(SkillRecordDecodeError::Version {
                found: probe.version,
                supported: SKILL_INVOCATION_RECORD_VERSION,
            });
        }
        Ok(serde_json::from_str(json)?)
    }
}

/// A skill-tool call: the tool and its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tool", rename_all = "snake_case")]
pub enum SkillInvocation {
    LoadSkill { name: String },
    ReadSkillFile { skill: String, path: String },
}

impl SkillInvocation {
    /// Stable identity of this invocation within a session: the key
    /// [`SkillInvocationStore::record`] dedupes on.
    ///
    /// [`SkillInvocationStore::record`]: super::SkillInvocationStore::record
    #[must_use]
    pub fn dedup_key(&self) -> String {
        match self {
            Self::LoadSkill { name } => format!("{LOAD_SKILL_TOOL_NAME}:{name}"),
            Self::ReadSkillFile { skill, path } => {
                format!("{READ_SKILL_FILE_TOOL_NAME}:{skill}:{path}")
            }
        }
    }

    /// Name of the skill this invocation resolves against.
    #[must_use]
    pub fn skill_name(&self) -> &str {
        match self {
            Self::LoadSkill { name } => name,
            Self::ReadSkillFile { skill, .. } => skill,
        }
    }

    /// The rig tool name that produced this invocation.
    #[must_use]
    pub fn tool_name(&self) -> &'static str {
        match self {
            Self::LoadSkill { .. } => LOAD_SKILL_TOOL_NAME,
            Self::ReadSkillFile { .. } => READ_SKILL_FILE_TOOL_NAME,
        }
    }

    /// The tool-call arguments as the JSON object the tool's schema defines.
    #[must_use]
    pub fn arguments(&self) -> serde_json::Value {
        match self {
            Self::LoadSkill { name } => serde_json::json!({ "name": name }),
            Self::ReadSkillFile { skill, path } => {
                serde_json::json!({ "skill": skill, "path": path })
            }
        }
    }

    /// Short human-readable label (for events and logs).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::LoadSkill { name } => name.clone(),
            Self::ReadSkillFile { skill, path } => format!("{skill}/{path}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(invocation: SkillInvocation) -> SkillInvocationRecord {
        SkillInvocationRecord {
            version: SKILL_INVOCATION_RECORD_VERSION,
            invocation,
            tool_call_id: "call_abc123".to_string(),
            anchor: 3,
            seq: 1,
            invoked_at: chrono::Utc::now(),
        }
    }

    /// Record → JSON → record: every field survives storage.
    fn assert_round_trip(record: SkillInvocationRecord) {
        let json = serde_json::to_string(&record).expect("record serializes");
        let stored: SkillInvocationRecord = serde_json::from_str(&json).expect("record parses");
        assert_eq!(stored, record);
    }

    #[test]
    fn load_skill_round_trips() {
        assert_round_trip(record(SkillInvocation::LoadSkill {
            name: "code-review".to_string(),
        }));
    }

    #[test]
    fn read_skill_file_round_trips() {
        assert_round_trip(record(SkillInvocation::ReadSkillFile {
            skill: "code-review".to_string(),
            path: "references/CHECKLIST.md".to_string(),
        }));
    }

    #[test]
    fn decode_accepts_the_current_version() {
        let record = record(SkillInvocation::LoadSkill {
            name: "s".to_string(),
        });
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(SkillInvocationRecord::decode(&json).unwrap(), record);
    }

    /// The version check fires before the shape check: a foreign-version
    /// record missing a field this schema requires is still refused by
    /// version.
    #[test]
    fn decode_refuses_other_versions_by_version() {
        let mut json = serde_json::to_value(record(SkillInvocation::LoadSkill {
            name: "s".to_string(),
        }))
        .unwrap();
        json["version"] = serde_json::json!(SKILL_INVOCATION_RECORD_VERSION + 1);
        json.as_object_mut().unwrap().remove("anchor");

        let err = SkillInvocationRecord::decode(&json.to_string()).unwrap_err();
        assert!(
            matches!(err, SkillRecordDecodeError::Version { found, .. }
                if found == SKILL_INVOCATION_RECORD_VERSION + 1),
            "expected a version refusal, got: {err}"
        );
    }

    #[test]
    fn decode_refuses_malformed_json() {
        assert!(matches!(
            SkillInvocationRecord::decode("not json"),
            Err(SkillRecordDecodeError::Json(_))
        ));
    }

    #[test]
    fn invocation_tags_are_stable_snake_case() {
        let load = serde_json::to_value(SkillInvocation::LoadSkill {
            name: "s".to_string(),
        })
        .unwrap();
        assert_eq!(load["tool"], "load_skill");
        let read = serde_json::to_value(SkillInvocation::ReadSkillFile {
            skill: "s".to_string(),
            path: "references/R.md".to_string(),
        })
        .unwrap();
        assert_eq!(read["tool"], "read_skill_file");
    }

    #[test]
    fn dedup_keys_distinguish_tools_and_arguments() {
        let load_a = SkillInvocation::LoadSkill {
            name: "a".to_string(),
        };
        let load_b = SkillInvocation::LoadSkill {
            name: "b".to_string(),
        };
        let read_a = SkillInvocation::ReadSkillFile {
            skill: "a".to_string(),
            path: "references/R.md".to_string(),
        };
        assert_ne!(load_a.dedup_key(), load_b.dedup_key());
        assert_ne!(load_a.dedup_key(), read_a.dedup_key());
        assert_eq!(load_a.dedup_key(), load_a.clone().dedup_key());
    }

    #[test]
    fn arguments_match_tool_schemas() {
        let load = SkillInvocation::LoadSkill {
            name: "code-review".to_string(),
        };
        assert_eq!(
            load.arguments(),
            serde_json::json!({ "name": "code-review" })
        );
        let read = SkillInvocation::ReadSkillFile {
            skill: "code-review".to_string(),
            path: "references/R.md".to_string(),
        };
        assert_eq!(
            read.arguments(),
            serde_json::json!({ "skill": "code-review", "path": "references/R.md" })
        );
    }
}
