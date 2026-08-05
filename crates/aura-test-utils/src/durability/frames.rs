//! Frame transcript and red-frame reporting for the durability harness.

use std::collections::BTreeMap;

use serde_json::Value;

/// A named acceptance frame with its captured events and store state.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub name: String,
    pub events: Vec<Value>,
    pub store_state: BTreeMap<String, Value>,
}

impl Frame {
    /// Create a new frame with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Append a normalized SSE event to the frame.
    pub fn push_event(&mut self, event: Value) {
        self.events.push(event);
    }

    /// Record a store-state snapshot under a stable key.
    pub fn record_state(&mut self, key: impl Into<String>, value: Value) {
        self.store_state.insert(key.into(), value);
    }
}

/// The full transcript of a durability run, partitioned into frames.
#[derive(Clone, Debug, Default)]
pub struct FrameTranscript {
    frames: Vec<Frame>,
}

impl FrameTranscript {
    /// Start a new empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a frame to the transcript.
    pub fn push(&mut self, frame: Frame) {
        self.frames.push(frame);
    }

    /// Return an iterator over all frames.
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// Render the transcript as a stable JSON value for snapshotting.
    pub fn to_snapshot(&self) -> Value {
        serde_json::json!({
            "frames": self.frames.iter().map(|f| {
                serde_json::json!({
                    "name": f.name,
                    "events": f.events,
                    "store_state": f.store_state,
                })
            }).collect::<Vec<_>>(),
        })
    }
}

/// Which frames are red (failed) in the current run.
#[derive(Clone, Debug, Default)]
pub struct RedFrames {
    red: Vec<String>,
}

impl RedFrames {
    /// Mark a frame as red.
    pub fn push(&mut self, name: impl Into<String>) {
        self.red.push(name.into());
    }

    /// True if any frame is red.
    pub fn is_empty(&self) -> bool {
        self.red.is_empty()
    }

    /// Return the red frame names.
    pub fn names(&self) -> &[String] {
        &self.red
    }
}

/// Render the red-frames list as a test failure message.
pub fn render_red_frames(red: &RedFrames) -> String {
    if red.is_empty() {
        return "all frames green".to_string();
    }
    let mut msg = String::from("red frames:\n");
    for name in red.names() {
        msg.push_str(&format!("  - {name}\n"));
    }
    msg
}
