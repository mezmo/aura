//! Reconnaissance and routing tools for capability-aware planning.
//!
//! These tools are added to the coordinator agent to allow dynamic
//! inspection of available tools during planning, and to make structured
//! routing decisions (respond directly, create plan, or request clarification).

pub(crate) mod get_conversation_context;
mod inspect_tool_params;
mod list_tools;
pub(crate) mod read_artifact;
pub mod routing_tools;
pub mod submit_result;

pub use get_conversation_context::GetConversationContextTool;
pub use inspect_tool_params::InspectToolParamsTool;
pub use list_tools::ListToolsTool;
pub use read_artifact::ReadArtifactTool;
pub use routing_tools::{
    CreatePlanTool, RequestClarificationTool, RespondDirectlyTool, RoutingDecision, RoutingToolSet,
};
pub use submit_result::{Confidence, SubmitResultDecision, SubmitResultOutput, SubmitResultTool};
