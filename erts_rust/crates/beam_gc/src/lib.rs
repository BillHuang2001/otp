#![doc = "Per-process generational semi-space copying GC, MSO sweep and heap sizing."]

//! Per-process garbage collector (Stage 4).
//!
//! Implements the GC half of Part II §D and the Stage-4 row of Part II §H:
//! generational semi-space Cheney copying, barrier-free by construction
//! (immutability ⇒ no write barrier), off-heap refcounted large objects (MSO) and
//! heap sizing. MMTk/Boehm are rejected; GenImmix is a measurement-gated
//! mature-generation option only.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_gc() -> &'static str {
    "beam_gc"
}
