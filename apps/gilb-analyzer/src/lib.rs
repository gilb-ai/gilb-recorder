//! Shannon — the local, auditable analyzer.
//!
//! Two jobs:
//! - `slice` (Layer 1) — reduce + redact recorded activity into a de-identified
//!   slice (see `docs/redaction-spec.md`); raw activity never leaves the machine.
//! - `find` (Phase 1) — run the therblig-finder prompt as a `claude -p`
//!   subprocess over `gilb-mcp`, parse the emitted Therbligs, dedup, and push
//!   them to gilb-web. Only the derived Therbligs cross the vendor boundary, and
//!   that egress lives in deterministic Rust (`web`/`pipeline`).

pub mod claude;
pub mod config;
pub mod db;
pub mod pii;
pub mod pipeline;
pub mod redact;
pub mod run;
pub mod slice;
pub mod therblig;
mod util;
pub mod web;

pub use redact::{redact, ActionRow};
pub use slice::{Segment, Slice, Step};
pub use therblig::{parse_therbligs, Delegation, Evidence, Therblig, TherbligStep};
