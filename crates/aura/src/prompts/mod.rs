// Prompt templates are stored as .md files in this directory and loaded
// via `include_str!()` from the modules that use them. Most files are
// referenced from their consuming module (e.g. orchestration/config.rs
// loads orchestrator_preamble.md / worker_preamble.md), so this mod.rs
// historically only existed to satisfy the `pub mod prompts;` line in
// lib.rs.
//
// Workflow preambles are the exception: they are bundled here and
// exposed via this module so `aura-config`'s builder can resolve a
// `[agent].workflow = "<id>"` TOML field to its bundled content.

pub mod workflows;
