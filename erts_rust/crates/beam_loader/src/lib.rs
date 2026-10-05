#![doc = ".beam file parsing and lowering into the typed block-structured IR of the execution engine."]

//! `.beam` reader and lowering front-end.
//!
//! Implements compatibility contract Part I §A.2 (the `.beam` binary/assembly
//! surface) and the Stage-1 loader half of Part II §H/§E: chunk parsing (`AtU8`,
//! `Code`, `LitT`, `ExpT`, `FunT`, `Line`, …), disassembly of the BEAM opcode
//! stream, and lowering into the *typed, block-structured IR* with safepoints,
//! failure/branch semantics, catch info and line/range tables. Backends (the
//! interpreter and the JIT tiers) consume only that IR.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_loader() -> &'static str {
    "beam_loader"
}
