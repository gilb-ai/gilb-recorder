//! Shannon — the local, auditable analyzer.
//!
//! Two jobs:
//! - `slice` (Layer 1) — reduce + redact recorded activity into a de-identified
//!   slice (see `docs/redaction-spec.md`); raw activity never leaves the machine.
//! - `find` (Phase 1) — run a job's prompt as a `claude -p` subprocess over
//!   `gilb-mcp`, parse the emitted findings (opaque JSON), and POST each to the
//!   job's gilb-web endpoint. Only the derived findings cross the vendor
//!   boundary, and that egress lives in deterministic Rust (`web`/`pipeline`).

pub mod claude;
pub mod config;
pub mod db;
pub mod findings;
pub mod pii;
pub mod pipeline;
pub mod redact;
pub mod run;
pub mod slice;
mod util;
pub mod web;

pub use findings::parse_findings;
pub use redact::{redact, ActionRow};
pub use slice::{Segment, Slice, Step};
