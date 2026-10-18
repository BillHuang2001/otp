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

//! The **retired** destination-pricing walk of the copy engine, kept as the equivalence oracle for
//! [`crate::copy::preflight_words`].
//!
//! This is `src/copy.rs`'s former self-contained sizing walk — the bounded DFS
//! ([`bounded_words_reference`]), its visit predicate ([`needs_visit_reference`]), the acyclicity
//! check it fell back to ([`is_acyclic_reference`]) and the preflight built on them
//! ([`preflight_words_reference`]) — moved here verbatim. The shipped copier no longer walks
//! anything itself: it supplies a [`beam_term::size::SizingPolicy`]
//! ([`crate::copy::CopyPricing`](crate::copy)) to `beam_term::size`'s policy-parameterised walk, so
//! the DFS, the per-object interior, the bound and the cycle verdict are one shared implementation.
//!
//! The module is `#[cfg(test)]`-gated (see `lib.rs`), so none of it ships. It exists because a
//! refactor of a load-bearing cost model needs *evidence*, and the only evidence that the delegated
//! walk prices exactly as the retired one did is the retired one still being here to disagree: the
//! tests below assert, for a corpus of every term class the copier prices and for every limit up to
//! and past each graph's exact count, that both walks return the same `Ok(n)` and refuse the same
//! graphs with the same error.
//!
//! Two capabilities it never had are therefore *not* equivalence properties, and the tests state so
//! explicitly rather than pretending otherwise:
//!
//! * **No cycle detection.** The retired walk relied on a caller-supplied finite bound: on a cyclic
//!   graph every object contributes at least 2 words, so a finite `limit` stops it, but at
//!   `usize::MAX` its total saturates and it never terminates. `beam_term::size`'s walk detects the
//!   back-edge itself, so the shipped copier is prompt at *every* limit — a strict improvement the
//!   cycle tests pin on the shipped side, with the retired side exercised at finite limits only.
//! * **The empty-tuple guard is the literal-tagged shape in both walks.** `needs_visit_reference`
//!   skips only a zero-arity `ARITYVAL` (the shared empty tuple), and `beam_term::size::classify`
//!   applies the *same* predicate: it prices an arity-0 `ARITYVAL` header at nothing only when the
//!   term carries the literal-area bit — the C's `arity == 0` rule, which recognises
//!   `ERTS_GLOBAL_LIT_EMPTY_TUPLE` by identity and whose `ASSERT` explicitly excludes a record
//!   header (`copy.c:124-140`). A forged zero-arity `RECORD` header — which no constructor produces
//!   (`write_record` writes `make_record_header(n + 1)`, arity >= 1) and only the public
//!   `make_record_header(0)` constant builder can lay down — therefore prices its structural
//!   `arity + 1 == 1` word in **both** walks.
//!   `a_forged_zero_arity_record_header_prices_the_structural_one_word` pins that agreement.

use std::collections::HashSet;

use beam_common::Result;
use beam_term::layout::for_each_sizing_child_slot as sizing_child_slots;
use beam_term::prelude::*;

use crate::copy::{is_shared, refc_binary_words, CopyContext};
use crate::layout;

/// Whether a word contributes a destination object the copier must descend into: a compound term
/// that is **not** shared as-is. Immediates, literal-area terms (shared verbatim) and non-term
/// (bare header) words are all skipped — they write no destination word and expose no child.
///
/// This is the **destination-pricing** visit predicate — the `litopt != NULL` policy of the C
/// `size_object_x` (`copy.c:105,119`), where a literal-area subterm is skipped. It is deliberately
/// narrower than `beam_term::size::needs_visit`, which descends *into* literals because it counts
/// the **source** graph's occupancy. It is the predicate the shipped policy
/// ([`crate::copy::CopyPricing`]) expresses as `is_shared`.
#[inline]
fn needs_visit_reference(term: Eterm) -> bool {
    !is_shared(term) && is_term_word(term)
}

/// The number of **destination** words the copier writes for `term` — the copy engine's own
/// destination-pricing cost model (the C `size_object_litopt` analogue), exact and therefore a sound
/// preflight — or `None` as soon as the running total exceeds `limit`.
///
/// It is **not** a second copy of `beam_term`'s source sizing and must not be delegated to it: for
/// a literal-area subterm this walk counts **0** words (the copy shares it by identity) while
/// `beam_term::size_object` counts its interior, and for a refc binary no source handle owns it
/// counts the flattened window while `beam_term` counts the 8-word pair. Both deltas, their C
/// ground truth and the `beam_term` API that would be needed to unify them are in the module docs.
///
/// It mirrors `shallow`/`copy_refc_binary`'s allocation exactly: `2` words per cons cell,
/// [`ERL_BIN_REF_SIZE`] for an off-heap MSO container, [`refc_binary_words`] for a refc binary
/// (the 8-word `[BinRef][ErlSubBits]` pair when the source holds its payload handle, the flattened
/// window's `heap_bits_size_words` when it does not — see that function), nothing for the shared
/// zero-arity-tuple literal, nothing for a shared literal-area subterm, and the canonical
/// `beam_term::layout::object_words` for every other boxed object (walking its child slots with
/// `beam_term::layout::for_each_sizing_child_slot`). A `SUB_BITS` **match context** is one of those
/// "other" objects: `beam_term::layout::object_words` returns the `heap_bits_size_words(size)` block
/// `copy_match_context` writes and the sizing enumerator yields no child, so this walk sizes it
/// exactly as the copy writes it.
///
/// The `limit` is what makes it cycle-safe: a forged cyclic graph revisits its own objects, so its
/// count grows without bound and the walk stops at `limit` instead of looping forever. Every
/// compound object contributes at least 2 words, so a graph that terminates this way always
/// exceeds `limit` — i.e. `Some(n)` is returned only for a graph that is both acyclic and `<=
/// limit` words.
///
/// `ctx` supplies the source off-heap table the sizing reads to tell a handled refc binary from one
/// the copier will flatten.
fn bounded_words_reference(term: Eterm, limit: usize, ctx: &CopyContext<'_>) -> Option<usize> {
    let mut total = 0usize;
    let mut work: Vec<Eterm> = Vec::new();
    let mut current = term;
    loop {
        if needs_visit_reference(current) {
            match primary_tag(current) {
                PrimaryTag::List => {
                    total = total.saturating_add(2);
                    if total > limit {
                        return None;
                    }
                    // SAFETY: `current` is a live, non-`NIL` cons cell, so both words are readable.
                    let (head, tail) = unsafe { (car(current), cdr(current)) };
                    if needs_visit_reference(head) {
                        work.push(head);
                    }
                    if needs_visit_reference(tail) {
                        work.push(tail);
                    }
                }
                PrimaryTag::Boxed => {
                    if layout::is_off_heap_term(current) {
                        total = total.saturating_add(ERL_BIN_REF_SIZE);
                    } else if layout::is_refc_binary_term(current) {
                        total = total.saturating_add(refc_binary_words(current, ctx));
                    } else {
                        // Everything else — including a `SUB_BITS` match context, whose
                        // `object_words` is the flattened heap-bits block's size — is sized by
                        // the canonical `beam_term::layout::object_words` and walked by the
                        // sizing child-slot enumerator.
                        let p = boxed_val(current);
                        // SAFETY: `current` is a live boxed term, so `p` addresses its header word.
                        let header = unsafe { *p };
                        // The zero-arity tuple is the shared global literal: no destination word.
                        if !(is_arity_value(header) && header_arity(header) == 0) {
                            // SAFETY: `header`/`p` head a live, initialized object of `header`'s
                            // shape, so its per-object word count is well-defined.
                            total = total.saturating_add(unsafe { layout::object_words(header, p) });
                            if total > limit {
                                return None;
                            }
                            // SAFETY: `header`/`p` head a live object whose payload words are
                            // live, so its sizing child slots are readable; the closure only
                            // reads them.
                            unsafe {
                                sizing_child_slots(header, p, |offset| {
                                    // SAFETY: `offset` is an in-bounds slot of the object at `p`.
                                    let child = *p.add(offset);
                                    if needs_visit_reference(child) {
                                        work.push(child);
                                    }
                                });
                            }
                        }
                    }
                    if total > limit {
                        return None;
                    }
                }
                // Unreachable: `needs_visit_reference` already excluded non-terms and immediates.
                // Kept for exhaustiveness — such a word contributes no destination word.
                PrimaryTag::Header | PrimaryTag::Immed1 => {}
            }
        }
        match work.pop() {
            Some(next) => current = next,
            None => return Some(total),
        }
    }
}

/// Whether the term graph rooted at `term` is **acyclic**.
///
/// A depth-first walk (`car`/`cdr` for cons cells,
/// `beam_term::layout::for_each_sizing_child_slot` for boxed objects — the same children the
/// copier descends into) that tracks, in `on_path`, exactly the compound objects on the current
/// DFS path. A revisit of a word still on the path is a back-edge, i.e. a cycle. Shared objects
/// (`immediates`/literals), off-heap references and an `ErlSubBits` (a refc binary, or a match
/// context that `copy_struct` flattens) are leaves here, matching the copier: they have no
/// navigable child.
///
/// This is only ever called on the *error* path ([`preflight_words_reference`] falls back to it
/// after the bounded walk has already shown the graph does not fit), so the `HashSet` it needs is
/// never paid on a normal copy.
fn is_acyclic_reference(term: Eterm) -> bool {
    enum Step {
        Enter(Eterm),
        Exit(usize),
    }

    let mut on_path: HashSet<usize> = HashSet::new();
    let mut stack: Vec<Step> = Vec::new();
    stack.push(Step::Enter(term));

    while let Some(step) = stack.pop() {
        match step {
            Step::Exit(addr) => {
                on_path.remove(&addr);
            }
            Step::Enter(t) => {
                if !needs_visit_reference(t) {
                    continue;
                }
                match primary_tag(t) {
                    PrimaryTag::List => {
                        let addr = list_val(t) as usize;
                        if !on_path.insert(addr) {
                            return false;
                        }
                        // SAFETY: `t` is a live, non-`NIL` cons cell.
                        let (head, tail) = unsafe { (car(t), cdr(t)) };
                        stack.push(Step::Exit(addr));
                        if needs_visit_reference(head) {
                            stack.push(Step::Enter(head));
                        }
                        if needs_visit_reference(tail) {
                            stack.push(Step::Enter(tail));
                        }
                    }
                    PrimaryTag::Boxed => {
                        if layout::is_off_heap_term(t) || layout::is_refc_binary_term(t) {
                            continue;
                        }
                        let p = boxed_val(t);
                        // SAFETY: `t` is a live boxed term, so `p` addresses its header word.
                        let header = unsafe { *p };
                        if is_arity_value(header) && header_arity(header) == 0 {
                            continue;
                        }
                        let addr = p as usize;
                        if !on_path.insert(addr) {
                            return false;
                        }
                        stack.push(Step::Exit(addr));
                        // SAFETY: `header`/`p` head a live object whose payload words are live,
                        // so its sizing child slots are readable; the closure only reads them.
                        unsafe {
                            sizing_child_slots(header, p, |offset| {
                                // SAFETY: `offset` is an in-bounds slot of the object at `p`.
                                let child = *p.add(offset);
                                if needs_visit_reference(child) {
                                    stack.push(Step::Enter(child));
                                }
                            });
                        }
                    }
                    // Unreachable (see `bounded_words_reference`); a non-term has no children.
                    PrimaryTag::Header | PrimaryTag::Immed1 => {}
                }
            }
        }
    }
    true
}

/// Sizes the **destination** for a copy of `term` against a destination holding `limit` free words,
/// returning the flat word count to reserve — the copy engine's destination-capacity gate.
///
/// It is built on [`bounded_words_reference`], the copier's own destination-pricing cost model (the
/// C `size_object_litopt` analogue), **not** on `beam_term::size_object`, which sizes the *source*
/// graph and would mis-price a shared literal-area subterm and a no-handle refc binary in opposite
/// directions. That walk is also bounded and cycle-safe:
///
/// * a graph whose copy fits `limit` is sized by [`bounded_words_reference`] — an exact, bounded,
///   allocation-free walk that cannot hang;
/// * a graph that does not fit is either **cyclic** (a typed
///   [`BeamError::BadArg`](beam_common::BeamError::BadArg) — never a hang) or simply too big, in
///   which case the copier's own walk is re-run **unbounded** (safe now that
///   [`is_acyclic_reference`] has just proven the graph terminates) for the exact count, with the
///   canonical `beam_term::size_object` as the fallback if that walk saturates.
///
/// Either way the caller compares the result against `limit`; a graph that
/// `bounded_words_reference` accepted is guaranteed `<= limit`. A graph it rejected yields a count
/// **above** `limit` — even when `size_object` would under-report it (a refc binary the copier
/// flattens is sized as its 8-word pair there) — so the caller's refusal is total and never
/// leaves a partial write.
pub(crate) fn preflight_words_reference(
    term: Eterm,
    limit: usize,
    ctx: &CopyContext<'_>,
) -> Result<usize> {
    match bounded_words_reference(term, limit, ctx) {
        Some(n) => Ok(n),
        None => {
            if !is_acyclic_reference(term) {
                return Err(crate::error::bad_term());
            }
            // Acyclic, so an unlimited re-run of the copier's own sizing terminates. It returns
            // `Some` for every acyclic graph (the bound is `usize::MAX`, which a saturating total
            // cannot exceed); `size_object` is the defensive fallback.
            Ok(bounded_words_reference(term, usize::MAX, ctx)
                .unwrap_or_else(|| beam_term::size_object(term)))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use beam_alloc::{Heap, RefcBinary};
    use beam_gc::Mso;
    use beam_term::prelude::*;

    use crate::copy::{preflight_words, CopyContext};
    use crate::copy_reference::preflight_words_reference;
    use crate::off_heap::OffHeapTable;

    /// Asserts that the shipped policy walk ([`preflight_words`]) and the retired walk
    /// ([`preflight_words_reference`]) agree **exactly** on `term` at every limit `0..=(count + 2)`
    /// and at `usize::MAX`: the same `Ok(n)` value, or both `Err` with the same rendered error
    /// (`BeamError` has no `PartialEq`, and the rendered form is what a caller observes).
    ///
    /// `src_off` is the source off-heap table the copy boundary reads — empty for a graph with no
    /// off-heap reference, and holding the payload handle for a **handled** refc binary (the case
    /// where the price depends on the context at all).
    ///
    /// The `count` comes from the retired walk itself at `usize::MAX`, which is exactly the call
    /// that also proves the term is acyclic: the `0..=(count + 2)` sweep only makes sense for a graph
    /// that has a flat count at all. Cycles are the other test's subject.
    fn assert_pricing_agrees(name: &str, term: Eterm, src_off: &OffHeapTable) {
        let mut dst_off = OffHeapTable::new();
        let ctx = CopyContext::new(src_off, &mut dst_off);

        let count = match preflight_words_reference(term, usize::MAX, &ctx) {
            Ok(n) => n,
            Err(e) => panic!("{name}: a corpus term must have a flat count, got {e:?}"),
        };
        assert_eq!(
            preflight_words(term, usize::MAX, &ctx).ok(),
            Some(count),
            "{name}: the unbounded count agrees"
        );

        for limit in 0..=(count + 2) {
            let want = preflight_words_reference(term, limit, &ctx);
            let got = preflight_words(term, limit, &ctx);
            match (want, got) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "{name}: the count must agree at limit {limit}"),
                (Err(a), Err(b)) => assert_eq!(
                    format!("{a:?}"),
                    format!("{b:?}"),
                    "{name}: the refusal must agree at limit {limit}"
                ),
                (a, b) => panic!("{name}: divergent verdicts at limit {limit}: {a:?} vs {b:?}"),
            }
        }
    }

    /// Asserts both walks refuse the **forged cyclic** graph `term` with the same error at every
    /// finite limit in `limits`.
    ///
    /// `usize::MAX` is deliberately not among them: the retired walk has no cycle detection and
    /// relies on a finite bound (its saturating total stops growing at `usize::MAX`, so it would
    /// never see `total > limit` and would not terminate). The shipped walk detects the back-edge
    /// itself; [`the_policy_walk_refuses_a_forged_cycle_promptly_at_every_limit`] pins that
    /// separately.
    fn assert_both_refuse_cycle(name: &str, term: Eterm, limits: impl IntoIterator<Item = usize>) {
        let src_off = OffHeapTable::new();
        let mut dst_off = OffHeapTable::new();
        let ctx = CopyContext::new(&src_off, &mut dst_off);
        for limit in limits {
            let want = preflight_words_reference(term, limit, &ctx);
            let got = preflight_words(term, limit, &ctx);
            match (want, got) {
                (Err(a), Err(b)) => assert_eq!(
                    format!("{a:?}"),
                    format!("{b:?}"),
                    "{name}: the refusal must agree at limit {limit}"
                ),
                (a, b) => panic!("{name}: expected a refusal at limit {limit}: {a:?} vs {b:?}"),
            }
        }
    }

    /// The finite limits every forged cycle is checked at: below, around and far past the number of
    /// words any of the fixtures actually contains.
    const CYCLE_LIMITS: [usize; 14] = [
        0, 1, 2, 3, 4, 5, 6, 8, 16, 32, 64, 128, 4096, 100_000,
    ];

    // -----------------------------------------------------------------------
    // Fixture builders (mirroring `tests/classes.rs`, `tests/guards.rs`,
    // `tests/binary.rs` and `tests/external.rs`)
    // -----------------------------------------------------------------------

    /// A fresh source heap big enough for every fixture in one test.
    fn src_heap() -> Heap {
        Heap::new(1 << 16).expect("source heap")
    }

    /// A flat map / HAMT built by the canonical `write_map` (the shape `beam_term` asserts on).
    fn map_term(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
        // SAFETY: `map_words_capacity` is a sound upper bound on the words `write_map` writes.
        unsafe {
            let hp = heap
                .alloc_words(map_words_capacity(pairs.len()))
                .expect("the map fits");
            write_map(hp, pairs, atoms).expect("the map builds")
        }
    }

    /// A `Local` (`make_fun3`) closure with a non-empty environment.
    fn local_fun(heap: &mut Heap, env: &[Eterm]) -> Eterm {
        // SAFETY: `fun_words(Local, env.len())` is the block's exact size.
        unsafe {
            let hp = heap
                .alloc_words(fun_words(FunKind::Local, env.len()))
                .expect("the fun fits");
            write_fun_local(hp, 2, 0x1122_3344_5566_7788, 5, env).expect("a valid local fun")
        }
    }

    /// An `External` (`EXPORT_EXT`) fun: the 4-word block.
    fn external_fun(heap: &mut Heap, module: Atom, function: Atom) -> Eterm {
        // SAFETY: `fun_words(External, 0)` words (the 4-word block) are reserved.
        unsafe {
            let hp = heap
                .alloc_words(fun_words(FunKind::External, 0))
                .expect("the fun fits");
            write_fun_external(hp, 2, module, function).expect("a valid external fun")
        }
    }

    /// An `Imported` (`NEW_FUN_EXT`) fun with a non-empty environment.
    fn imported_fun(heap: &mut Heap, module: Atom, generator: Atom, env: &[Eterm]) -> Eterm {
        let mut uniq = [0u8; 16];
        for (i, b) in uniq.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(1);
        }
        let creator = FunPid {
            node: generator,
            number: 0xAAAA_BBBB,
            serial: 0xCCCC_DDDD,
            creation: 0x1122_3344,
        };
        // SAFETY: `fun_words(Imported, env.len())` is the block's exact size.
        unsafe {
            let hp = heap
                .alloc_words(fun_words(FunKind::Imported, env.len()))
                .expect("the fun fits");
            write_fun_imported(hp, 2, module, &uniq, 0x0102_0304, 7, 3, creator, env)
                .expect("a valid imported fun")
        }
    }

    /// A byte-aligned on-heap small binary (`ErlHeapBits`).
    fn heap_bits(heap: &mut Heap, bytes: &[u8]) -> Eterm {
        // SAFETY: `heap_bits_size_words(bits)` words hold the `ErlHeapBits`; the payload fits.
        unsafe {
            let hp = heap
                .alloc_words(heap_bits_size_words(bytes.len() * 8))
                .expect("the heap binary fits");
            write_heap_bits(hp, bytes, bytes.len() * 8).expect("the payload fits")
        }
    }

    /// A **non-byte-aligned** on-heap small binary: 13 live bits of a 2-byte payload.
    fn non_aligned_heap_bits(heap: &mut Heap) -> Eterm {
        let bits = 13usize;
        let bytes = [0b1011_0110u8, 0b0000_0101u8];
        // SAFETY: `heap_bits_size_words(13)` words are reserved; the payload fits.
        unsafe {
            let hp = heap
                .alloc_words(heap_bits_size_words(bits))
                .expect("the heap binary fits");
            write_heap_bits(hp, &bytes, bits).expect("the payload fits")
        }
    }

    /// A match context (a `SUB_BITS` over an on-heap bitstring) with a non-byte-aligned window.
    fn match_context(heap: &mut Heap) -> Eterm {
        let bytes = [0xA5u8, 0x3C, 0xDE, 0x7E, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let original = heap_bits(heap, &bytes);
        // SAFETY: the on-heap bitstring's data base is word-aligned; the payload covers the window.
        unsafe {
            let base = bitstring_parts(original).0;
            let hp = heap.alloc_words(5).expect("the match context fits");
            write_sub_bits(
                hp,
                Eterm::from_raw(base as usize),
                Eterm::from_raw(3),
                Eterm::from_raw(24),
                original,
            )
        }
    }

    /// The 8-word `[BinRef(3)][ErlSubBits(5)]` pair for `data_ptr`/`len`.
    ///
    /// # Safety
    ///
    /// `heap` must have 8 spare words and `data_ptr` must be a live, aligned payload of `len` bytes
    /// that outlives the returned term.
    unsafe fn refc_binary_pair(heap: &mut Heap, data_ptr: *mut u8, len: usize) -> Eterm {
        // SAFETY: the caller keeps the payload alive and aligned.
        unsafe {
            let hp = heap
                .alloc_words(ERL_REFC_BITS_SIZE)
                .expect("the pair fits");
            write_refc_binary(
                hp,
                data_ptr as *mut Eterm,
                core::ptr::null_mut(),
                data_ptr,
                0,
                len * 8,
            )
        }
    }

    /// The M1 off-heap MSO container `[HEADER_BIN_REF, slot, 0]`.
    fn off_heap_container(heap: &mut Heap, slot: usize) -> Eterm {
        // SAFETY: the heap owns these three words.
        unsafe {
            let p = heap.alloc_words(ERL_BIN_REF_SIZE).expect("the container fits");
            *p = HEADER_BIN_REF;
            *p.add(1) = Eterm::from_raw(slot);
            *p.add(2) = Eterm::from_raw(0);
            make_boxed(p as *const Eterm)
        }
    }

    /// A `1000`-cell proper list.
    fn deep_list(heap: &mut Heap, cells: usize) -> Eterm {
        let mut acc = NIL;
        for i in 0..cells {
            acc = heap.cons(make_small((i % 97) as i64), acc).expect("the cons fits");
        }
        acc
    }

    // -----------------------------------------------------------------------
    // The equivalence contract
    // -----------------------------------------------------------------------

    /// Every shape the copier prices, at every limit around its exact count. One test, because the
    /// source heap, atom table and off-heap tables are shared by the fixtures — the per-shape
    /// assertion is [`assert_pricing_agrees`].
    #[test]
    fn the_policy_walk_agrees_with_the_retired_walk_on_every_shape_at_every_limit() {
        let mut atoms = AtomTable::new();
        let module = atoms.intern("m");
        let function = atoms.intern("reverse");
        let node = atoms.intern("n1@host");
        let generation = atoms.intern("g1@host");
        let atom_term = make_atom(atoms.intern("some_atom"));

        let mut src = src_heap();
        let empty_off = OffHeapTable::new();

        // --- immediates and the shared empty tuple -------------------------
        for (name, term) in [
            ("small_int", make_small(1234)),
            ("atom", atom_term),
            ("nil", NIL),
            ("empty_tuple", tuple0()),
            ("non_term", THE_NON_VALUE),
        ] {
            assert_pricing_agrees(name, term, &empty_off);
        }

        // --- tuples, records, lists ----------------------------------------
        let flat = src
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("the tuple fits");
        assert_pricing_agrees("flat_tuple", flat, &empty_off);

        let nested = src
            .tuple(&[flat, make_small(0), NIL])
            .expect("the tuple fits");
        assert_pricing_agrees("nested_tuple", nested, &empty_off);

        // SAFETY: `record_block_words(2)` words are reserved and every field value is live.
        let record = unsafe {
            let hp = src
                .alloc_words(record_block_words(2))
                .expect("the record fits");
            write_record(
                hp,
                module,
                atoms.intern("r"),
                false,
                &[
                    (make_atom(atoms.intern("alpha")), make_small(11)),
                    (make_atom(atoms.intern("beta")), make_small(22)),
                ],
                make_small(0),
            )
            .expect("a valid native record")
        };
        assert_pricing_agrees("native_record", record, &empty_off);

        let tail = src.cons(make_small(2), NIL).expect("the cons fits");
        let list = src.cons(make_small(1), tail).expect("the cons fits");
        assert_pricing_agrees("cons_list", list, &empty_off);
        assert_pricing_agrees("deep_list_1000", deep_list(&mut src, 1000), &empty_off);
        let short_tail = src.cons(make_small(3), NIL).expect("the cons fits");
        let tuple_head_list = src.cons(flat, short_tail).expect("the cons fits");
        assert_pricing_agrees("list_of_tuples", tuple_head_list, &empty_off);
        // The same subterm twice: counted per occurrence by both walks.
        let shared_dag = src.tuple(&[flat, flat]).expect("the tuple fits");
        assert_pricing_agrees("shared_subterm_twice", shared_dag, &empty_off);

        // --- literal-area terms (the sharing axis) -------------------------
        assert_pricing_agrees("literal_root", make_literal(flat), &empty_off);
        let literal_child = src
            .tuple(&[make_literal(flat), make_small(1)])
            .expect("the tuple fits");
        assert_pricing_agrees("literal_child", literal_child, &empty_off);
        assert_pricing_agrees("literal_list", make_literal(list), &empty_off);

        // --- maps: flat and HAMT -------------------------------------------
        let flat_pairs: Vec<(Eterm, Eterm)> = (0..8)
            .map(|i| (make_small(i), make_small(i * 10 + 3)))
            .collect();
        let flat_map = map_term(&mut src, &flat_pairs, &atoms);
        assert_pricing_agrees("flat_map", flat_map, &empty_off);
        // 64 keys exceed `MAP_SMALL_MAP_LIMIT` (32) -> a genuine HAMT trie.
        let hamt_pairs: Vec<(Eterm, Eterm)> = (0..64)
            .map(|i| (make_small(i * 3), make_small(i * 1000 + 7)))
            .collect();
        let hamt_map = map_term(&mut src, &hamt_pairs, &atoms);
        assert_pricing_agrees("hamt_map", hamt_map, &empty_off);

        // --- funs: all three kinds, each with a non-empty env ---------------
        let env = src
            .tuple(&[make_small(7), atom_term])
            .expect("the free var fits");
        let local = local_fun(&mut src, &[env]);
        assert_pricing_agrees("local_fun", local, &empty_off);
        let external = external_fun(&mut src, module, function);
        assert_pricing_agrees("external_fun", external, &empty_off);
        let imported = imported_fun(&mut src, module, generation, &[env]);
        assert_pricing_agrees("imported_fun", imported, &empty_off);

        // --- numbers and bitstrings ----------------------------------------
        // SAFETY: `write_float` fills exactly `FLOAT_SIZE_WORDS` words.
        let float = unsafe {
            let hp = src.alloc_words(FLOAT_SIZE_WORDS).expect("the float fits");
            write_float(hp, core::f64::consts::PI)
        };
        assert_pricing_agrees("boxed_float", float, &empty_off);

        let digits = [5u64, 1u64];
        // SAFETY: `big_size_words(2)` words are reserved; `digits` is non-empty.
        let bignum = unsafe {
            let hp = src
                .alloc_words(big_size_words(digits.len()))
                .expect("the bignum fits");
            write_big(hp, true, &digits)
        };
        assert_pricing_agrees("bignum", bignum, &empty_off);

        let aligned = heap_bits(&mut src, &[0x01u8, 0x23, 0x45, 0x67, 0x89]);
        assert_pricing_agrees("aligned_heap_bits", aligned, &empty_off);
        let non_aligned = non_aligned_heap_bits(&mut src);
        assert_pricing_agrees("non_aligned_heap_bits", non_aligned, &empty_off);
        // A match context is flattened, and the canonical `object_words` already is that window.
        let context = match_context(&mut src);
        assert_pricing_agrees("match_context", context, &empty_off);

        // --- external pid / port / reference -------------------------------
        // SAFETY: each writer fills exactly the words its size function reserves.
        let (pid, port, reference) = unsafe {
            let pid = write_external_pid(
                src.alloc_words(external_pid_size_words()).expect("the pid fits"),
                make_atom(node),
                0x1234,
                0x0007,
                5,
            );
            let port = write_external_port(
                src.alloc_words(external_port_size_words()).expect("the port fits"),
                make_atom(node),
                0x9999,
                3,
            );
            let reference = write_external_ref(
                src.alloc_words(external_ref_size_words(3))
                    .expect("the ref fits"),
                make_atom(node),
                &[10, 20, 30],
                4,
            );
            (pid, port, reference)
        };
        assert_pricing_agrees("external_pid", pid, &empty_off);
        assert_pricing_agrees("external_port", port, &empty_off);
        assert_pricing_agrees("external_ref", reference, &empty_off);
        let external_tuple = src
            .tuple(&[pid, port, reference])
            .expect("the tuple fits");
        assert_pricing_agrees("external_things", external_tuple, &empty_off);
    }

    /// The refc-binary and off-heap-reference shapes, where the price depends on the **copy
    /// context**'s source off-heap table — the reason the policy exists at all.
    #[test]
    fn the_policy_walk_prices_refc_binaries_and_off_heap_references_from_the_context() {
        let bytes: Vec<u8> = (0..200usize).map(|i| ((i * 7) % 251) as u8).collect();
        let mut src = src_heap();

        // A refc binary whose payload the SOURCE table owns: shared, 8 words.
        let mut handled_off = OffHeapTable::new();
        handled_off.push_binary(RefcBinary::new(&bytes).expect("the payload allocates"));
        let handled_ptr = handled_off.binaries()[0].data_ptr();
        // SAFETY: `handled_off` owns the payload, which outlives the pair.
        let handled = unsafe { refc_binary_pair(&mut src, handled_ptr, bytes.len()) };

        // A refc binary whose payload NO source handle owns: flattened to its window.
        let loose_binary = RefcBinary::new(&bytes).expect("the payload allocates");
        let loose_ptr = loose_binary.data_ptr();
        // SAFETY: `loose_binary` is alive for the whole test, so the pair is live.
        let loose = unsafe { refc_binary_pair(&mut src, loose_ptr, bytes.len()) };
        let empty_off = OffHeapTable::new();

        let mut dst_off = OffHeapTable::new();
        let handled_price = preflight_words(
            handled,
            usize::MAX,
            &CopyContext::new(&handled_off, &mut dst_off),
        )
        .expect("a handled refc binary has a price");
        let loose_price =
            preflight_words(loose, usize::MAX, &CopyContext::new(&empty_off, &mut dst_off))
                .expect("a no-handle refc binary has a price");
        assert_eq!(handled_price, ERL_REFC_BITS_SIZE, "the shared payload's 8-word pair");
        assert_eq!(
            loose_price,
            heap_bits_size_words(bytes.len() * 8),
            "the flattened window"
        );
        assert_ne!(
            handled_price, loose_price,
            "the price really depends on the copy context's source table"
        );

        // Each matches its own reference answer (the same `refc_binary_words` feeds both).
        assert_pricing_agrees("refc_binary_handled", handled, &handled_off);
        assert_pricing_agrees("refc_binary_flattened", loose, &empty_off);
        // ... and a sub-binary (a `write_sub_bits` window over the refc pair) does too.
        // SAFETY: `handled` is a live refc binary and the window is inside its payload.
        let sub = unsafe {
            let hp = src.alloc_words(5).expect("the sub-binary fits");
            write_sub_bits(
                hp,
                Eterm::from_raw(handled_ptr as usize),
                Eterm::from_raw(16),
                Eterm::from_raw(96),
                handled,
            )
        };
        assert_pricing_agrees("refc_sub_binary", sub, &handled_off);

        // An off-heap MSO container: `ERL_BIN_REF_SIZE` words, priced as the canonical container.
        let mut mso_off = OffHeapTable::new();
        let slot = mso_off.push(Mso::new(0x5A5A_u32, 12));
        let container = off_heap_container(&mut src, slot);
        assert_pricing_agrees("mso_container", container, &mso_off);
        let container_tuple = src
            .tuple(&[container, make_small(1)])
            .expect("the tuple fits");
        assert_pricing_agrees("mso_container_in_tuple", container_tuple, &mso_off);
        assert_eq!(
            preflight_words(container, usize::MAX, &CopyContext::new(&mso_off, &mut dst_off))
                .expect("a container has a price"),
            ERL_BIN_REF_SIZE,
            "the M1 container is its 3-word thing"
        );
    }

    /// A forged **zero-arity `RECORD` header** — the one shape the empty-tuple guard could
    /// misclassify — prices its structural `arity + 1 == 1` word in **both** walks.
    ///
    /// `beam_term::size::classify` prices an arity-0 header at nothing only for the literal-tagged
    /// `ERTS_GLOBAL_LIT_EMPTY_TUPLE`, exactly the C's `arity == 0` rule (`copy.c:124-140`), whose
    /// `ASSERT(arity > 0 || !is_record_header(hdr))` states the complement: a zero-arity *record*
    /// header is not the empty tuple. So the retired walk's guard and the shipped policy's guard
    /// agree here, and both charge one word. No term the term layer can build is affected
    /// (`write_record` writes `make_record_header(n + 1)`, arity >= 1), so reaching the shape needs
    /// a raw forge through the public `make_record_header(0)` constant builder — which is what this
    /// test lays down.
    ///
    /// This asserts *agreement*, not a tolerance: if `beam_term` ever widens (or drops) the guard,
    /// this test fails and the crate `CONTEXT.md`'s note must be revisited.
    #[test]
    fn a_forged_zero_arity_record_header_prices_the_structural_one_word() {
        let mut src = src_heap();
        // SAFETY: one word is reserved for the header and the object has no payload.
        let zero_arity_record = unsafe {
            let p = src.alloc_words(1).expect("the header fits");
            *p = make_record_header(0);
            make_boxed(p as *const Eterm)
        };
        let src_off = OffHeapTable::new();
        let mut dst_off = OffHeapTable::new();
        let ctx = CopyContext::new(&src_off, &mut dst_off);

        assert_eq!(
            header_arity(make_record_header(0)),
            0,
            "the forged header really is zero-arity"
        );
        assert_eq!(
            preflight_words_reference(zero_arity_record, usize::MAX, &ctx).ok(),
            Some(1),
            "the retired walk priced it at the canonical `arity + 1`"
        );
        assert_eq!(
            preflight_words(zero_arity_record, usize::MAX, &ctx).ok(),
            Some(1),
            "the policy walk prices it at the structural `arity + 1` too"
        );
        // Reachability, restated as an assertion: the record constructor never produces the shape.
        let mut atoms = AtomTable::new();
        // SAFETY: `record_block_words(1)` words are reserved and every field value is live.
        let real_record = unsafe {
            let hp = src
                .alloc_words(record_block_words(1))
                .expect("the record fits");
            write_record(
                hp,
                atoms.intern("m"),
                atoms.intern("r"),
                false,
                &[(make_atom(atoms.intern("alpha")), make_small(11))],
                make_small(0),
            )
            .expect("a valid native record")
        };
        // SAFETY: `real_record` is a live native record instance.
        assert!(
            header_arity(unsafe { *boxed_val(real_record) }) > 0,
            "`write_record` writes `make_record_header(n + 1)`, so arity >= 1"
        );
        assert_pricing_agrees("native_record_from_the_constructor", real_record, &src_off);
    }

    // -----------------------------------------------------------------------
    // Forged cycles
    // -----------------------------------------------------------------------
    /// A cons cell whose `cdr` points back at itself: the forged term `[1 | <self>]`.
    fn self_referential_cons(heap: &mut Heap) -> Eterm {
        let list = heap.cons(make_small(1), NIL).expect("the cons fits");
        // SAFETY: `heap.cons` just reserved this cell; its `cdr` word is rewritten on purpose, which
        // is exactly the malformed input the cycle guard must survive.
        unsafe { *list_val(list).add(1) = list };
        list
    }

    /// A one-element tuple whose single element is the tuple itself: `{<self>}`.
    fn self_referential_tuple(heap: &mut Heap) -> Eterm {
        // SAFETY: two words are reserved for the 1-tuple and its element is set to itself.
        unsafe {
            let p = heap.alloc_words(2).expect("the tuple fits");
            *p = make_arityval(1);
            *p.add(1) = make_boxed(p as *const Eterm);
            make_boxed(p as *const Eterm)
        }
    }

    /// Two cons cells pointing at each other: `a = [1 | b]`, `b = [2 | a]`.
    fn mutual_cons_cycle(heap: &mut Heap) -> Eterm {
        let a = heap.cons(make_small(1), NIL).expect("cell a fits");
        let b = heap.cons(make_small(2), a).expect("cell b fits");
        // SAFETY: both cells were just reserved; `a`'s `cdr` is pointed at `b`, closing the cycle.
        unsafe { *list_val(a).add(1) = b };
        a
    }

    /// A 100-cell valid list whose outermost `cdr` is forged into a cycle.
    fn cycle_behind_a_valid_prefix(heap: &mut Heap) -> Eterm {
        let mut acc = NIL;
        let mut cells = Vec::new();
        for i in 0..100 {
            acc = heap.cons(make_small(i), acc).expect("the cons fits");
            cells.push(acc);
        }
        let innermost = *cells.last().expect("at least one cell");
        // SAFETY: `acc` is the outermost cell; its `cdr` is rewritten to the innermost cell.
        unsafe { *list_val(acc).add(1) = innermost };
        acc
    }

    /// Both walks refuse all four forged cycles with the same error at every finite limit — the
    /// retired walk's acyclicity fallback and the policy walk's own back-edge detection must agree
    /// that these graphs are `BadArg`, never a `TermGraphTooLarge` count.
    #[test]
    fn both_walks_refuse_a_forged_cycle_at_every_limit() {
        let mut src = src_heap();
        assert_both_refuse_cycle(
            "cons_cycle",
            self_referential_cons(&mut src),
            CYCLE_LIMITS,
        );
        assert_both_refuse_cycle(
            "tuple_cycle",
            self_referential_tuple(&mut src),
            CYCLE_LIMITS,
        );
        assert_both_refuse_cycle(
            "mutual_cons_cycle",
            mutual_cons_cycle(&mut src),
            CYCLE_LIMITS,
        );
        assert_both_refuse_cycle(
            "cycle_behind_a_valid_prefix",
            cycle_behind_a_valid_prefix(&mut src),
            CYCLE_LIMITS,
        );
    }

    /// The shipped walk is **prompt at `usize::MAX`**, which the retired walk cannot be: it detects
    /// the back-edge instead of relying on a finite bound. The assertion completing (well under the
    /// test harness's patience) is the "never hangs" evidence; the retired walk is not run here, for
    /// the reason documented on [`assert_both_refuse_cycle`].
    #[test]
    fn the_policy_walk_refuses_a_forged_cycle_promptly_at_every_limit() {
        let mut src = src_heap();
        for (name, term) in [
            ("cons_cycle", self_referential_cons(&mut src)),
            ("tuple_cycle", self_referential_tuple(&mut src)),
            ("mutual_cons_cycle", mutual_cons_cycle(&mut src)),
            (
                "cycle_behind_a_valid_prefix",
                cycle_behind_a_valid_prefix(&mut src),
            ),
        ] {
            let src_off = OffHeapTable::new();
            let mut dst_off = OffHeapTable::new();
            let ctx = CopyContext::new(&src_off, &mut dst_off);
            for limit in CYCLE_LIMITS.into_iter().chain(core::iter::once(usize::MAX)) {
                let started = Instant::now();
                match preflight_words(term, limit, &ctx) {
                    Err(beam_common::BeamError::BadArg) => {}
                    other => panic!("{name}: expected BadArg at limit {limit}, got {other:?}"),
                }
                assert!(
                    started.elapsed().as_secs() < 5,
                    "{name}: the refusal at limit {limit} must be prompt"
                );
            }
        }
    }
}