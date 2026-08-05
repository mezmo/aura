//! Durability-harness infrastructure for the #271 HITL park/reify
//! acceptance frames.
//!
//! The harness drives a real `aura-web-server` process against a stub Ollama
//! LLM and the file-backed session store. It captures SSE and store transitions
//! as labeled frames, then asserts them against committed expect-test
//! snapshots. Production holes are left as `todo!()`; the harness fails at the
//! first one and reports the red frames.

pub mod frames;
pub mod normalize;
pub mod server;
pub mod stub_llm;

pub use frames::{render_red_frames, Frame, FrameTranscript, RedFrames};
pub use normalize::scrub_nondeterminism;
pub use server::{AuraServerProcess, ServerConfig, SessionStoreBackend};
pub use stub_llm::StubLlm;
