#![doc = "Frozen 64-bit tagged Erlang term representation: immediates, boxed terms, atoms and literal areas."]

//! The frozen term representation owned by this crate (workspace `CONTEXT.md` §2).
//!
//! Implements Part I §A.1–A.2 (language semantics and the `.beam` data model) and
//! the term-layout discussion of Part II §B: one machine word `Eterm` with a 2-bit
//! primary tag (`HEADER`/`LIST`/`BOXED`/`IMMED1`), a 3-bit pointer tag on 64-bit
//! (literal-area bit), 60-bit small integers, 6-bit-shifted atom immediates and
//! header-less 2-word cons cells tagged `TAG_PRIMARY_LIST`. Every other crate
//! depends downward on it for term operations.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_term() -> &'static str {
    "beam_term"
}
