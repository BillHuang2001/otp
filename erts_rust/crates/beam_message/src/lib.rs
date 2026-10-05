#![doc = "Copy engine (copy_struct) and messaging: send/receive, fragments and message queues."]

//! The process memory model: term copying across process boundaries.
//!
//! Implements the messaging/copy-boundary half of Part II §C (§A lists the
//! process model) and the Stage-3 row of Part II §H: `copy_struct` term movement
//! between heaps, send/receive, signal delivery, fragmented messages and message
//! queue storage. Immutability bounds what may be shared; everything else is
//! copied.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_message() -> &'static str {
    "beam_message"
}
