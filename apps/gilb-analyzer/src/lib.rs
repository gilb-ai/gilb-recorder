//! Shannon — the local, auditable analyzer.
//!
//! `find` runs a job's prompt as a `claude -p` subprocess over
//! `gilb-mcp`, parses the emitted findings (opaque JSON), and POSTs each to the
//! job's gilb-web endpoint. Only the derived findings cross the vendor boundary,
//! and that egress lives in deterministic Rust (`web`/`pipeline`).

pub mod claude;
pub mod config;
pub mod db;
pub mod findings;
pub mod pipeline;
pub mod run;
mod util;
pub mod web;

pub use findings::parse_findings;
