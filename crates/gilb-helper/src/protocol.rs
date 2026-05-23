//! Wire protocol for the gilb helper daemon.
//!
//! Frames are length-prefixed msgpack: 4-byte big-endian payload length
//! followed by a serialized [`Request`] or [`Response`] (see
//! `research/08-helper-daemon.md` §3). Only the `Ping` / `Pong` handshake
//! is defined today; `StartCapture` / `StopCapture` / `Status` land in
//! follow-up cards.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Ping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Pong,
}
