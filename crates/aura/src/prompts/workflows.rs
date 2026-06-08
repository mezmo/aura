//! Built-in workflow preambles.
//!
//! A *workflow* is a bundled system-prompt preamble that AURA ships
//! inside the binary so operators can opt into specialized agent
//! behavior without authoring the discipline themselves. Selected via
//! `[agent].workflow = "<id>"` in the operator's TOML config.
//!
//! When `[agent].workflow` is set, the resolver below returns the
//! bundled preamble; `aura-config`'s builder prepends it to the
//! operator's own `agent.system_prompt`, separated by a horizontal
//! rule. This lets operators layer substrate-specific instructions
//! (tool catalog names, MCP namespacing, benchmark-specific framing)
//! *below* AURA's universal investigation discipline without having
//! to copy the discipline content into every config.
//!
//! Adding a new workflow:
//! 1. Drop the markdown content in `crates/aura/src/prompts/<id>_workflow_preamble.md`
//! 2. Add a `pub const` here via `include_str!()`
//! 3. Add a match arm in [`resolve_workflow_preamble`]
//! 4. Document the new id in the AgentConfig::workflow doc-comment

/// Substrate-agnostic SRE investigation discipline. Applies regardless
/// of infrastructure substrate (Kubernetes, ECS, EC2, Lambda, Docker,
/// bare-metal). Covers: causal-chain rule, symptom-vs-cause distinction,
/// read-before-write, cross-reference signals, anti-anchoring, and
/// orchestration-mode coordinator/worker discipline.
pub const SRE_WORKFLOW_PREAMBLE: &str = include_str!("sre_workflow_preamble.md");

/// Resolve a workflow id to its bundled preamble.
///
/// Returns `None` for unknown workflow ids; the caller (typically
/// `aura-config`'s builder) is responsible for surfacing that as a
/// configuration error rather than silently falling back. Silent
/// fallback would mask typos in operator TOMLs — a config that says
/// `workflow = "site-reliability"` should fail loudly, not run as
/// though no workflow had been requested.
pub fn resolve_workflow_preamble(workflow_id: &str) -> Option<&'static str> {
    match workflow_id {
        "sre" => Some(SRE_WORKFLOW_PREAMBLE),
        _ => None,
    }
}

/// List of every workflow id this build of AURA knows about. Used by
/// error messages so a typo'd `workflow = "..."` config gets a helpful
/// "supported: [...]" suggestion instead of an unhelpful "unknown".
pub const SUPPORTED_WORKFLOWS: &[&str] = &["sre"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sre_preamble_is_non_empty_and_includes_discipline_headers() {
        // Sanity-check the bundled preamble actually loaded — guards
        // against a build that lost the include_str! target file.
        assert!(!SRE_WORKFLOW_PREAMBLE.is_empty());
        // Spot-check a few of the load-bearing section headers; if any
        // of these drift in the markdown file, that's a content review
        // signal, not a silent breakage.
        assert!(SRE_WORKFLOW_PREAMBLE.contains("Causal-chain rule"));
        assert!(SRE_WORKFLOW_PREAMBLE.contains("Symptom-vs-cause"));
    }

    #[test]
    fn resolve_sre_returns_the_bundled_preamble() {
        let resolved = resolve_workflow_preamble("sre")
            .expect("sre must resolve to a bundled preamble");
        assert_eq!(resolved, SRE_WORKFLOW_PREAMBLE);
    }

    #[test]
    fn resolve_unknown_workflow_returns_none() {
        assert!(resolve_workflow_preamble("not-a-real-workflow").is_none());
        assert!(resolve_workflow_preamble("").is_none());
        assert!(resolve_workflow_preamble("SRE").is_none(), "case-sensitive");
    }

    #[test]
    fn supported_workflows_list_matches_resolver() {
        // Every id in SUPPORTED_WORKFLOWS must resolve. If someone adds
        // a workflow id without wiring it into the resolver (or vice
        // versa), this catches the drift immediately.
        for id in SUPPORTED_WORKFLOWS {
            assert!(
                resolve_workflow_preamble(id).is_some(),
                "SUPPORTED_WORKFLOWS lists '{id}' but resolver returns None",
            );
        }
    }
}
