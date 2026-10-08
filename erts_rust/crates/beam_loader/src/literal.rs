// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The decoded static-literal area: the `LitT` chunk materialized into Erlang terms.
//!
//! [`crate::chunk::literal`] parses the `LitT` chunk framing into one raw ETF
//! payload per literal (in `Operand::Literal(idx)` index order); this module
//! *decodes* those payloads with [`beam_codec::decode_term`] into one immutable
//! heap and exposes the resulting terms to the engine. A literal is referenced
//! from the code by [`crate::operand::Operand::Literal`]`(idx)`, which the
//! interpreter resolves through [`crate::ir::Module::literal`].
//!
//! The area is built **once** at load time and never mutated afterwards, so
//! [`crate::ir::Module`] shares it through an [`Arc`] and a `Module` clone is
//! cheap. The backing [`beam_alloc::Heap`] holds a raw pointer (its `unsafe` is
//! confined to `beam_alloc`), so a `LiteralArea` is **neither `Send` nor `Sync`**
//! — and therefore neither is a loaded [`Module`](crate::ir::Module): a module
//! stays on the thread that loaded it.
//!
//! Heap exhaustion while decoding is handled by *growing the area*: the heap
//! allocated here is a private, fixed-capacity arena, so an exhausted decode is
//! retried on a larger arena (a bounded number of attempts) before the allocation
//! failure is propagated. A genuinely malformed literal is never swallowed — it
//! surfaces as a typed [`BeamError`] from [`LiteralArea::decode`].
use std::sync::Arc;

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError, Result};
use beam_term::{AtomTable, Eterm};

/// Conservative words-per-payload-byte estimate for the first decode attempt.
///
/// The densest literal encoding is a list whose elements are one-byte `NIL_EXT`
/// values (roughly one payload byte per cons cell, and a cons cell is two heap
/// words), i.e. about two words per byte; the factor is deliberately twice that so
/// the common case never needs a retry.
const WORDS_PER_LITERAL_BYTE: usize = 4;

/// Extra words reserved per literal (term headers, tuple arity words, …).
const LITERAL_HEADROOM_WORDS: usize = 64;

/// Extra words reserved for the area itself.
const AREA_HEADROOM_WORDS: usize = 16;

/// Lower bound applied to the arena before doubling, so a tiny estimate can still
/// grow out of its first (failed) attempt.
const MIN_RETRY_WORDS: usize = 64;

/// How many times an exhausted decode is retried on a larger heap.
const MAX_DECODE_ATTEMPTS: u32 = 6;

/// The static literals of a module, decoded into a private immutable heap.
///
/// The struct owns the heap the terms point into; dropping the area frees it. The
/// terms are handed out by value ([`Eterm`] is a machine word) and stay valid for
/// as long as the area — and therefore the loaded
/// [`Module`](crate::ir::Module) — is alive.
pub struct LiteralArea {
    /// The arena the decoded terms live in; `None` exactly when `terms` is empty
    /// (a module without literals allocates nothing).
    heap: Option<Heap>,
    /// The decoded literals, in `Operand::Literal(idx)` index order.
    terms: Vec<Eterm>,
}

impl LiteralArea {
    /// An area with no literals.
    pub fn empty() -> LiteralArea {
        LiteralArea {
            heap: None,
            terms: Vec::new(),
        }
    }

    /// Decodes the raw `LitT` payloads into an immutable literal area.
    ///
    /// `raw` holds the ETF payload of each literal in `Operand::Literal(idx)` index
    /// order; `atoms` is the module's atom table (a literal may name atoms, which
    /// are interned there — interning is idempotent, so passing the table the
    /// module itself uses keeps atom identity consistent).
    ///
    /// A malformed payload, an unsupported ETF tag or a decode that still does not
    /// fit after the bounded retries yields the corresponding typed [`BeamError`];
    /// nothing is ever silently defaulted.
    pub fn decode(raw: &[Vec<u8>], atoms: &mut AtomTable) -> Result<LiteralArea> {
        if raw.is_empty() {
            return Ok(LiteralArea::empty());
        }
        let (heap, terms) = decode_terms(raw, atoms)?;
        Ok(LiteralArea {
            heap: Some(heap),
            terms,
        })
    }

    /// The literal at `index` (an `Operand::Literal(index)` reference).
    ///
    /// Returns `None` for an index outside the table.
    pub fn get(&self, index: u32) -> Option<Eterm> {
        self.terms.get(index as usize).copied()
    }

    /// The number of literals in the area.
    pub fn len(&self) -> usize {
        self.terms.len()
    }

    /// Whether the area holds no literals.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The decoded literals, in `Operand::Literal(idx)` index order.
    pub fn terms(&self) -> &[Eterm] {
        &self.terms
    }

    /// The heap the terms live in (introspection and tests), if any.
    pub fn heap(&self) -> Option<&Heap> {
        self.heap.as_ref()
    }
}

impl Default for LiteralArea {
    /// An empty area (`Module::default()` must not allocate).
    fn default() -> LiteralArea {
        LiteralArea::empty()
    }
}

impl core::fmt::Debug for LiteralArea {
    /// A compact, meaningful rendering: neither [`Eterm`] nor [`Heap`] has a useful
    /// `Debug` (the heap is a raw pointer).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LiteralArea")
            .field("terms", &self.terms.len())
            .field("heap_words", &self.heap.as_ref().map_or(0, Heap::used))
            .finish_non_exhaustive()
    }
}

/// A shared, immutable literal area — the type of [`crate::ir::Module::literals`].
///
/// The `Arc` is sound because the area is write-once at load time and only ever
/// read afterwards; it is *not* `Send`/`Sync` (the heap holds a raw pointer), so a
/// loaded module is confined to its loading thread.
pub type SharedLiteralArea = Arc<LiteralArea>;

/// The first-attempt heap capacity for `count` literals totalling `total_bytes`.
///
/// Saturating, never wrapping: an absurd payload yields `usize::MAX` and the
/// `Heap::new` allocation failure is then reported as a typed error.
fn estimate_capacity(count: usize, total_bytes: usize) -> usize {
    total_bytes
        .saturating_mul(WORDS_PER_LITERAL_BYTE)
        .saturating_add(count.saturating_mul(LITERAL_HEADROOM_WORDS))
        .saturating_add(AREA_HEADROOM_WORDS)
}

/// Decodes `raw` into a fresh arena sized from the payloads' total length.
///
/// Returns the arena and the decoded terms; shared with
/// [`crate::attributes::AttributeTable`] so the arena-sizing and heap-growth retry
/// live in one place.
pub(crate) fn decode_terms(raw: &[Vec<u8>], atoms: &mut AtomTable) -> Result<(Heap, Vec<Eterm>)> {
    let total_bytes: usize = raw.iter().map(Vec::len).sum();
    decode_terms_at_capacity(raw, atoms, estimate_capacity(raw.len(), total_bytes))
}

/// Decodes `raw` into a fresh arena of at least `initial_capacity` words, growing
/// the arena (bounded) while the decode is limited by heap exhaustion.
///
/// The heap never grows, so a failed attempt is retried on a *new, larger* arena;
/// the discarded terms go away with it (the atoms they interned are idempotent).
pub(crate) fn decode_terms_at_capacity(
    raw: &[Vec<u8>],
    atoms: &mut AtomTable,
    initial_capacity: usize,
) -> Result<(Heap, Vec<Eterm>)> {
    let mut capacity = initial_capacity;
    let mut attempt = 0;
    loop {
        let mut heap = Heap::new(capacity)?;
        match decode_into(raw, &mut heap, atoms) {
            Ok(terms) => return Ok((heap, terms)),
            Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Heap => {
                attempt += 1;
                if attempt >= MAX_DECODE_ATTEMPTS {
                    return Err(BeamError::Alloc(failure));
                }
                capacity = capacity.max(MIN_RETRY_WORDS).saturating_mul(2);
            }
            Err(other) => return Err(other),
        }
    }
}

/// Decodes every payload into `heap`, in order.
fn decode_into(raw: &[Vec<u8>], heap: &mut Heap, atoms: &mut AtomTable) -> Result<Vec<Eterm>> {
    let mut terms = Vec::with_capacity(raw.len());
    for payload in raw {
        terms.push(beam_codec::decode_term(payload, heap, atoms)?);
    }
    Ok(terms)
}

#[cfg(test)]
mod tests {
    //! Literal decoding: a hand-built ETF payload, the heap-growth retry, the
    //! bounded-retry failure, and the malformed-payload error. The committed
    //! `coverage.beam` fixture (whose `LitT` holds `{error,division_by_zero}`,
    //! `{error,badarith}` and `#{a=>1,b=>2}`) is covered by the in-module tests of
    //! [`crate::lower`].
    //!
    //! Tests are allowed to `unwrap` (a panic there is a test failure).

    use super::*;
    use beam_codec::term::{cons_head, cons_tail, tuple_arity, tuple_element};
    use beam_common::BeamError;
    use beam_term::prelude::{atom_val, is_nil, is_small, make_atom, signed_value};

    /// `{error,division_by_zero}` — a real literal of `coverage.beam`
    /// (`term_to_binary({error,division_by_zero})`).
    const ERROR_DIVISION_BY_ZERO: [u8; 28] = [
        131, 104, 2, 119, 5, 101, 114, 114, 111, 114, 119, 16, 100, 105, 118, 105, 115, 105, 111,
        110, 95, 98, 121, 95, 122, 101, 114, 111,
    ];

    /// `[i || i <- lists:seq(0, n-1)]` as an external term; each element is two
    /// bytes (`SMALL_INTEGER_EXT`) but two heap words (one cons cell).
    fn small_int_list(n: u32) -> Vec<u8> {
        let mut bytes = vec![131, 108];
        bytes.extend_from_slice(&n.to_be_bytes());
        for i in 0..n {
            bytes.extend_from_slice(&[97, (i % 251) as u8]);
        }
        bytes.push(106); // NIL_EXT
        bytes
    }

    #[test]
    fn decodes_a_hand_built_payload() {
        let mut atoms = AtomTable::new();
        let area = LiteralArea::decode(&[ERROR_DIVISION_BY_ZERO.to_vec()], &mut atoms).unwrap();

        assert_eq!(area.len(), 1);
        assert!(!area.is_empty());
        assert_eq!(area.terms().len(), 1);
        assert!(area.heap().is_some());

        let term = area.get(0).expect("the literal exists");
        assert_eq!(tuple_arity(term).unwrap(), 2);
        let tag = tuple_element(term, 0).unwrap();
        assert_eq!(tag, make_atom(atoms.intern("error")));
        let reason = tuple_element(term, 1).unwrap();
        assert_eq!(atoms.name(atom_val(reason)), Some("division_by_zero"));

        // An index past the end is `None`, never a panic.
        assert_eq!(area.get(1), None);
        assert_eq!(area.get(u32::MAX), None);
    }

    #[test]
    fn decodes_several_literals_in_index_order() {
        let mut atoms = AtomTable::new();
        // `1` then `{ok,17}`.
        let one = vec![131, 97, 1];
        let ok_17 = vec![131, 104, 2, 119, 2, 111, 107, 97, 17];
        let area = LiteralArea::decode(&[one, ok_17], &mut atoms).unwrap();

        assert_eq!(area.len(), 2);
        let first = area.get(0).unwrap();
        assert!(is_small(first));
        assert_eq!(signed_value(first), 1);
        let second = area.get(1).unwrap();
        assert_eq!(tuple_arity(second).unwrap(), 2);
        assert_eq!(tuple_element(second, 0).unwrap(), make_atom(atoms.intern("ok")));
        assert_eq!(signed_value(tuple_element(second, 1).unwrap()), 17);
    }

    #[test]
    fn an_empty_literal_table_yields_an_empty_area() {
        let mut atoms = AtomTable::new();
        let area = LiteralArea::decode(&[], &mut atoms).unwrap();
        assert!(area.is_empty());
        assert_eq!(area.len(), 0);
        assert_eq!(area.get(0), None);
        assert!(area.heap().is_none(), "no literals means no heap");

        let default = LiteralArea::default();
        assert!(default.is_empty());
        assert_eq!(
            format!("{default:?}"),
            "LiteralArea { terms: 0, heap_words: 0, .. }"
        );
    }

    #[test]
    fn a_malformed_payload_is_a_typed_error() {
        let mut atoms = AtomTable::new();
        // A truncated `{ok,17}`: the decoder must reject it, not panic.
        let truncated = vec![131, 104, 2, 119, 2, 111, 107];
        let err = LiteralArea::decode(&[truncated], &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");

        // A payload that is not an external term at all (no `VERSION_MAGIC`).
        let err = LiteralArea::decode(&[vec![0, 1, 2, 3]], &mut atoms).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }

    #[test]
    fn every_truncation_of_a_payload_is_a_typed_error() {
        // The record framing may hand over any byte run; a run that stops inside the
        // term must fail closed at the decoder (the last atom's byte count needs its
        // full 16 bytes, so no strict prefix is a complete term).
        let mut atoms = AtomTable::new();
        for len in 0..ERROR_DIVISION_BY_ZERO.len() {
            let err = LiteralArea::decode(&[ERROR_DIVISION_BY_ZERO[..len].to_vec()], &mut atoms)
                .err()
                .unwrap_or_else(|| panic!("a payload truncated to {len} bytes must be rejected"));
            assert!(matches!(err, BeamError::BadArg), "len {len}: {err:?}");
        }
        // The untruncated payload still decodes, so the sweep above is not vacuous.
        let area = LiteralArea::decode(&[ERROR_DIVISION_BY_ZERO.to_vec()], &mut atoms).unwrap();
        assert_eq!(area.len(), 1);
    }

    #[test]
    fn a_zlib_compressed_litt_chunk_round_trips_into_terms() {
        // The compressed `LitT` framing must reach the same decoded terms as the
        // verbatim one: build the record table, compress it, parse it through
        // `chunk::literal` and decode the payloads into a literal area.
        let payload: Vec<u8> = vec![131, 104, 2, 119, 2, 111, 107, 97, 17]; // `{ok,17}`
        let mut table = 1u32.to_be_bytes().to_vec();
        table.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        table.extend_from_slice(&payload);
        let mut chunk = (table.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(&beam_codec::compress::compress(&table, 6).unwrap());

        let raw = crate::chunk::literal::parse(&chunk).unwrap();
        assert_eq!(raw, vec![payload]);

        let mut atoms = AtomTable::new();
        let area = LiteralArea::decode(&raw, &mut atoms).unwrap();
        let term = area.get(0).expect("the single literal decodes");
        assert_eq!(tuple_arity(term).unwrap(), 2);
        assert_eq!(tuple_element(term, 0).unwrap(), make_atom(atoms.intern("ok")));
        assert_eq!(signed_value(tuple_element(term, 1).unwrap()), 17);
    }
    #[test]
    fn an_exhausted_decode_is_retried_on_a_larger_heap() {
        let mut atoms = AtomTable::new();
        // A 100-element list needs ~200 words; a one-word first arena forces the
        // decode to fail and be retried with a bigger one.
        let (heap, terms) = decode_terms_at_capacity(&[small_int_list(100)], &mut atoms, 1).unwrap();

        assert_eq!(terms.len(), 1);
        let list = terms[0];
        assert_eq!(signed_value(cons_head(list).unwrap()), 0);
        assert!(heap.used() >= 200);
    }

    #[test]
    fn a_payload_beyond_the_bounded_retries_reports_the_allocation_failure() {
        let mut atoms = AtomTable::new();
        // 1500 cons cells need ~3000 words — more than the largest retry arena
        // (the retries double from the 64-word floor, six attempts at most).
        // `Heap` is not `Debug`, so match on the `Result` rather than `unwrap_err`.
        match decode_terms_at_capacity(&[small_int_list(1500)], &mut atoms, 1) {
            Ok(_) => panic!("expected the decode to fail on the largest retry arena"),
            Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Heap),
            Err(other) => panic!("expected a heap allocation failure, got {other:?}"),
        }
    }

    #[test]
    fn nested_terms_are_decoded_into_the_one_arena() {
        let mut atoms = AtomTable::new();
        // `[[1,2],{ok}]` — a nested list and a tuple in one literal.
        let payload = vec![
            131, 108, 0, 0, 0, 2, // LIST_EXT, 2 elements
            108, 0, 0, 0, 2, 97, 1, 97, 2, 106, // [1,2]
            104, 1, 119, 2, 111, 107, // {ok}
            106, // NIL
        ];
        let area = LiteralArea::decode(&[payload], &mut atoms).unwrap();
        let list = area.get(0).unwrap();
        assert!(!is_nil(list));

        // Head: the nested list `[1,2]`.
        let head = cons_head(list).unwrap();
        assert_eq!(signed_value(cons_head(head).unwrap()), 1);

        // Second element: the tuple `{ok}`.
        let tail = cons_tail(list).unwrap();
        let second = cons_head(tail).unwrap();
        assert_eq!(tuple_arity(second).unwrap(), 1);
        assert_eq!(
            tuple_element(second, 0).unwrap(),
            make_atom(atoms.intern("ok"))
        );

        // The tail of the tail is NIL; nothing merged or lost across payloads.
        assert!(is_nil(cons_tail(tail).unwrap()));
    }

    #[test]
    fn the_capacity_estimate_grows_with_the_payload() {
        // A pure unit test of the sizing policy, so a regression in the estimate is
        // visible without decoding anything.
        assert_eq!(estimate_capacity(0, 0), AREA_HEADROOM_WORDS);
        assert_eq!(
            estimate_capacity(2, 100),
            100 * WORDS_PER_LITERAL_BYTE + 2 * LITERAL_HEADROOM_WORDS + AREA_HEADROOM_WORDS
        );
        // Saturating, never wrapping (a 32-bit `usize` could otherwise overflow).
        assert_eq!(estimate_capacity(usize::MAX, usize::MAX), usize::MAX);
    }
}
