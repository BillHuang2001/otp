#![doc = "Shared error/result types, configuration, logging and version constants for the Rust ERTS rewrite."]

//! Common foundation shared by every crate in the Rust ERTS workspace.
//!
//! Implements the error strategy of Part I §A (return-based BIF/NIF failure, no
//! `setjmp`/`longjmp`) and the Stage-0 foundations listed in Part II §H: the
//! `BeamError` / `Result` pair, the BEAM exception-reason encoding, configuration,
//! logging and version constants. This crate is a dependency sink: it depends on
//! nothing but `libc`/std.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_common() -> &'static str {
    "beam_common"
}
