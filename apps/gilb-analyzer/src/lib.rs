//! Shannon — the local, auditable collector. Reads recorded activity and
//! reduces + redacts it into a de-identified slice (see
//! `docs/redaction-spec.md`). The slice is the only thing that leaves the
//! machine; raw activity never does.

pub mod claude;
pub mod config;
pub mod pii;
pub mod redact;
pub mod slice;
pub mod therblig;
pub mod web;

pub use redact::{redact, ActionRow};
pub use slice::{Segment, Slice, Step};
pub use therblig::{parse_therbligs, Delegation, Evidence, Therblig, TherbligStep};
