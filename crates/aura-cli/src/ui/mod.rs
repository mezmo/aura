pub mod markdown;
pub mod pre_launch;
pub mod prompt;
pub mod welcome;

// Sub-modules extracted from the original prompt.rs
pub(crate) mod animation;
pub(crate) mod event_replay;
pub(crate) mod input_frame;
pub(crate) mod input_hint;
pub(crate) mod mid_stream;
pub(crate) mod orchestrator;
pub(crate) mod state;
pub(crate) mod status_bar;
pub(crate) mod stream_panel;
