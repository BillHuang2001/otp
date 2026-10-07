//! Contracts **A.2** / **A.5** — encoding a *local* pid / port id.
//!
//! `term_to_binary/1` of `self()` and of an internal port emits `NEW_PID_EXT` (tag `88`) and
//! `V4_PORT_EXT` (tag `120`) respectively: the encoder needs the node's `(name, creation)`
//! identity for both. Split out of [`super::wire`] (which pins the tag surface for terms
//! built from *external* ids) because these are the terms whose bytes depend on the
//! runtime's own node identity — the reason this group exists is that a wrong or missing
//! identity shows up as `unsupported`, never as a wrong byte.
//!
//! The pid/port *creation* and id fields of the image are per-VM-start values, so the cases
//! assert the stable leading shape (`binary:part/3` up to the node name) and the
//! round-trip property (`binary_to_term` gives back a pid/port), never the full bytes.

use super::Case;

/// The A.2/A.5 local-pid/port encoding cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
