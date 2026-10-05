#![doc = "ETS tables (set/ordered_set/hash family) with B-tree and CHAMP interiors."]

//! ETS tables and their interior data structures.
//!
//! Implements the observable table semantics of Part I §A.1/§A.8 (`ets:*` behaviour
//! and introspection) plus the Stage-3 row of Part II §H: `set`, `ordered_set`,
//! `bag`, `duplicate_bag` and the `new`/`select` implementations over B-tree and
//! CHAMP interiors. Per workspace `CONTEXT.md` §2 the interior shapes (tree
//! fan-out, CHAMP thresholds) are *not* externally pinned; only the semantics are.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_ets() -> &'static str {
    "beam_ets"
}
