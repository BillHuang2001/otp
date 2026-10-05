#![doc = "Erlang distribution protocol v6: handshake, DFLAG_*/DOP_* control messages and epmd."]

//! Distribution protocol v6 (`DFLAG_*` / `DOP_*`).
//!
//! Implements compatibility contract Part I §A.5 (the distribution wire protocol)
//! and the Stage-0 foundations of Part II §H: the connect/challenge handshake,
//! capability negotiation, control-message framing, fragmented payloads and the
//! `epmd` registration client.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_dist() -> &'static str {
    "beam_dist"
}
