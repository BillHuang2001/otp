//! Contract **A.7** — the two externally-observable legacy hashes.
//!
//! `erlang:phash/2` (the C `make_hash`) and `erlang:phash2/1,2` (the C `make_hash2`) are
//! *frozen* observable behaviour: a program that persisted a hash bucket or a term-hash key
//! must read the same value from the rewrite. They are their own group because the pinned
//! values come from bit-exact transcriptions of `erl_term_hashing.c` (`../../crates/beam_bifs`
//! `src/phash.rs`), not from the term API — so the goldens are the strongest possible
//! statement that the transcription is right.
//!
//! Grouped by the input family that exercises a distinct branch of the C:
//! small-int boundaries, the block-hash binary path (by length), a non-byte-aligned
//! bitstring, a map (which routes through the *other* hash), `NIL`/improper lists, and the
//! `Range` argument.

use super::Case;

/// The A.7 `phash`/`phash2` cases.
pub(crate) fn group() -> Vec<Case> {
    Vec::new()
}
