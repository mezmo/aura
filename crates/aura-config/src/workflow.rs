//! Ordered workflows reuse named workers and their MCP permissions.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ConfigError, OrchestrationConfig};

/// An explicitly configured stage. Inputs are JSON pointers into
/// `{ "input": ..., "stages": { "stage_id": result } }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStage {
    pub id: String,
    pub worker: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    pub operation: StageOperation,
    /// Validate the complete result before making it available downstream.
    pub output_schema: Value,
    /// MCP notifications dispatched at offsets from the approval request.
    #[serde(default)]
    pub on_approval_wait: Vec<WaitNotification>,
}

/// Uses the named worker's MCP permissions; recipient routing stays in that MCP.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitNotification {
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    pub after_secs: u64,
    pub worker: String,
    pub tool: String,
    pub arguments: Value,
}

/// Execution semantics are configuration, never a model-generated plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StageOperation {
    Worker {
        prompt: String,
    },
    Tool {
        tool: String,
        arguments: Value,
    },
    Verify {
        /// Explicit declaration that repeated probes cannot mutate the target.
        read_only: bool,
        tool: String,
        arguments: Value,
        success_schema: Value,
        failure_schema: Option<Value>,
        interval_secs: u64,
        timeout_secs: u64,
    },
    Wait {
        seconds: u64,
    },
}

/// Compile schemas without permitting remote or local-file references.
pub fn compile_schema(schema: &Value) -> Result<jsonschema::Validator, ConfigError> {
    fn reject_external(value: &Value) -> bool {
        match value {
            Value::Object(map) => map.iter().any(|(key, value)| {
                ((key == "$ref" || key == "$dynamicRef")
                    && value.as_str().is_some_and(|r| !r.starts_with('#')))
                    || reject_external(value)
            }),
            Value::Array(values) => values.iter().any(reject_external),
            _ => false,
        }
    }
    if reject_external(schema) {
        return Err(ConfigError::Validation(
            "workflow schemas require local fragment references".into(),
        ));
    }
    jsonschema::validator_for(schema)
        .map_err(|error| ConfigError::Validation(format!("invalid workflow schema: {error}")))
}

impl OrchestrationConfig {
    pub fn validate_stages(&self) -> Result<(), ConfigError> {
        let mut ids = HashSet::new();
        for stage in &self.stages {
            if stage.id.is_empty()
                || !stage
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(ConfigError::Validation(
                    "workflow stage ids must contain only letters, digits and underscores".into(),
                ));
            }
            if !self.workers.contains_key(&stage.worker) {
                return Err(ConfigError::Validation(format!(
                    "stage {} references unknown worker {}",
                    stage.id, stage.worker
                )));
            }
            for pointer in stage.inputs.values() {
                let parts: Vec<_> = pointer.split('/').collect();
                let valid = pointer.starts_with('/')
                    && valid_pointer(pointer)
                    && (parts.get(1) == Some(&"input")
                        || parts.get(1) == Some(&"run")
                        || (parts.get(1) == Some(&"stages")
                            && parts.get(2).is_some_and(|id| ids.contains(*id))));
                if !valid {
                    return Err(ConfigError::Validation(format!(
                        "stage {} input must reference input or an earlier stage: {pointer}",
                        stage.id
                    )));
                }
            }
            if !ids.insert(stage.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate workflow stage {}",
                    stage.id
                )));
            }
            compile_schema(&stage.output_schema)?;
            match &stage.operation {
                StageOperation::Tool { tool, arguments }
                | StageOperation::Verify {
                    tool, arguments, ..
                } if tool.is_empty()
                    || !arguments.is_object()
                    || stage.inputs.keys().any(|key| arguments.get(key).is_some()) =>
                {
                    return Err(ConfigError::Validation(
                        "tool stages need a name, object arguments and non-overlapping inputs"
                            .into(),
                    ));
                }
                StageOperation::Wait { seconds } if *seconds > MAX_WAIT_SECS => {
                    return Err(ConfigError::Validation("wait exceeds one year".into()));
                }
                _ => {}
            }
            for notification in &stage.on_approval_wait {
                if notification.after_secs > MAX_WAIT_SECS
                    || !notification.arguments.is_object()
                    || notification.tool.is_empty()
                    || !self.workers.contains_key(&notification.worker)
                {
                    return Err(ConfigError::Validation(
                        "invalid approval notification".into(),
                    ));
                }
            }

            if let StageOperation::Verify {
                success_schema,
                failure_schema,
                interval_secs,
                timeout_secs,
                read_only,
                ..
            } = &stage.operation
            {
                if !read_only
                    || *interval_secs == 0
                    || *timeout_secs == 0
                    || *timeout_secs > MAX_WAIT_SECS
                    || interval_secs > timeout_secs
                {
                    return Err(ConfigError::Validation(
                        "verification requires a positive interval within its timeout".into(),
                    ));
                }
                compile_schema(success_schema)?;
                if let Some(schema) = failure_schema {
                    compile_schema(schema)?;
                }
            }
        }
        if !self.stages.is_empty() && !self.enabled {
            return Err(ConfigError::Validation(
                "configured stages require orchestration.enabled".into(),
            ));
        }
        Ok(())
    }
}

/// Maximum persisted wait horizon: one year.
const MAX_WAIT_SECS: u64 = 31_536_000;

fn valid_pointer(pointer: &str) -> bool {
    let mut chars = pointer.chars();
    while let Some(ch) = chars.next() {
        if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return false;
        }
    }
    true
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_external_references_and_invalid_schemas() {
        assert!(compile_schema(&json!({"$ref":"file:///etc/passwd"})).is_err());
        assert!(compile_schema(&json!({"$ref":"https://example.org/schema"})).is_err());
        assert!(compile_schema(&json!({"type":"not-a-type"})).is_err());
        let schema = compile_schema(&json!({"type":"object", "required":["approved"], "properties":{"approved":{"type":"boolean"}}})).unwrap();
        assert!(!schema.is_valid(&json!({"approved":"yes"})));
        assert!(schema.is_valid(&json!({"approved":true})));
    }
}
