#![doc = "Typed block IR, direct-threaded interpreter and the BeamAsm/Cranelift JIT tiers."]

//! The execution engine: IR seam plus interpreter and JIT backends.
//!
//! Implements the Stage-1 engine half of Part II §H with the IR contract of §E and
//! the interpreter/JIT trade-offs of §F. Backends consume only the typed,
//! block-structured IR produced by `beam_loader`; the root-set contract is
//! `x_reg_array[0..Live)` plus the whole Erlang stack, with **no frame liveness
//! maps**. `jit_beamasm` is a Rust port of BeamAsm with block register allocation;
//! `jit_cranelift` is the optional hot tier with bounded, off-critical-path
//! compilation and cheap deoptimization (compile latency is a correctness
//! constraint, Part I §C).

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_engine() -> &'static str {
    "beam_engine"
}
