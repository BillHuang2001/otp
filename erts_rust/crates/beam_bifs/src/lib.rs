#![doc = "BIF registry (bif.tab surface) and the built-in function implementations."]

//! Built-in functions, the `bif.tab` surface.
//!
//! Implements compatibility contract Part I §A.7 (the OTP app contracts exposed to
//! `erlang`/`erts_internal`/`prim_*`) and the Stage-5 row of Part II §H. The BIF
//! table is source data — regenerate the registry from it, never patch generated
//! output. Failures are return-based `Result`s that the engine turns into BEAM
//! exceptions (Part I §A / workspace `CONTEXT.md` §3): no `setjmp`/`longjmp`.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_bifs() -> &'static str {
    "beam_bifs"
}
