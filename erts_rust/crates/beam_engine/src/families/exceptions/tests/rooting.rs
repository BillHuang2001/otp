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

//! GC-rooting regression pin for [`crate::families::exceptions::stacktrace::symbolic_stacktrace`].
//!
//! The cooked stacktrace item's `A` slot (the failed-clause argument list) and its
//! `{error_info, Map}` location extension were once embedded **by raw pointer** —
//! shared with the machine's `raise_function_clause_args`/`raise_error_info` latches,
//! whose cells live on a backing store (production: `beam_rt`'s pooled BIF heap) the
//! host may rewind after the carve. The fix deep-copies both payloads into the same
//! rooted reservation via [`crate::terms::bounded_copy`], so the cooked item owns its
//! graph. This test builds the two payloads on a **separate** [`MinimalContext`] arena,
//! cooks the trace on a second arena, and asserts the cooked heads are different words
//! (a deep copy) that still render correctly after the foreign arena is dropped.

use super::*;

// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity. (An explicit
// import shadows the glob-imported `terms` above.)
use crate::terms::test_support as terms;

use beam_term::{
    arityval, is_arity_value, is_flatmap_header, is_map_header, is_nil, is_small, list_val,
    make_atom, primary_tag, signed_value, tuple_val, Eterm, PrimaryTag, HEAP_BITS_SUBTAG,
    TAG_HEADER_MASK,
};

use crate::families::exceptions::stacktrace::symbolic_stacktrace;
use crate::terms::{list_elements, tuple_arity, tuple_element};

use crate::context::{ExecutionContext, MinimalContext, RootSet};
use beam_common::Result;

/// A structural description of `term` with **no addresses in it**, enough for this
/// test to tell a copied list/binary/flat map from a dangling or garbage word.
///
/// The renderer intentionally does not descend into flat maps, hash maps or binary
/// payloads — the assertions only need the *kind* markers, never a byte-exact
/// serialization.
#[allow(unsafe_code)]
fn shape(term: Eterm) -> String {
    // The primary tag, never the loose `is_list`/`is_boxed` bit tricks: those are
    // also true for a bare HEADER word.
    match primary_tag(term) {
        PrimaryTag::Immed1 if is_nil(term) => "[]".to_string(),
        PrimaryTag::Immed1 if is_small(term) => format!("#{}", signed_value(term)),
        PrimaryTag::Immed1 => format!("immed:{:#x}", term.raw()),
        PrimaryTag::List => {
            // SAFETY: a LIST word built by the term writers addresses a live
            // header-less `[CAR, CDR]` cell.
            let (car, cdr) = unsafe { (*list_val(term), *list_val(term).add(1)) };
            format!("[{}|{}]", shape(car), shape(cdr))
        }
        PrimaryTag::Boxed => {
            // SAFETY: a BOXED word addresses a live header word, which the
            // tag/map/arity predicates below read but never follow.
            let hp = tuple_val(term);
            let header = unsafe { *hp };
            if is_arity_value(header) {
                // SAFETY: an `arityval` header fixes exactly `arity` element words
                // after the header, each of which is a live term.
                let arity = arityval(header);
                let mut out = String::from("{");
                for i in 0..arity {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&shape(unsafe { *hp.add(1 + i) }));
                }
                out.push('}');
                out
            } else if is_flatmap_header(header) {
                "flatmap".to_string()
            } else if is_map_header(header) {
                "hashmap".to_string()
            } else if header.raw() & TAG_HEADER_MASK == HEAP_BITS_SUBTAG {
                "binary".to_string()
            } else {
                format!("boxed:{:#x}", header.raw())
            }
        }
        PrimaryTag::Header => format!("header:{:#x}", term.raw()),
    }
}

#[test]
fn the_cooked_saved_args_and_error_info_are_copied_not_shared_and_survive_the_foreign_store_drop() {
    // The entry module exists only so `item_shape` can resolve `func = 0`; it is
    // never executed. No `Line` chunk means the item has no source location, so its
    // location is exactly the `[{error_info, Map}]` shape.
    let mut fixture = Fixture::new(&["error_info", "module", "erl_stdlib_errors"]);
    fixture.add_function("run", 0, vec![Instr::Label(Label(0)), Instr::Return]);
    let module = fixture.finish();

    // The two payloads live on their own arena — the "foreign" backing store that a
    // production host would rewind after the carve.
    let mut foreign = MinimalContext::new();
    let bits = terms::make_heap_binary(&mut foreign, &[1]).expect("the heap binary builds");
    let list = terms::make_list(&mut foreign, &[bits]).expect("the saved args list builds");
    let module_atom = module.atom_table.lookup("module").unwrap();
    let erl_stdlib_errors_atom = module.atom_table.lookup("erl_stdlib_errors").unwrap();
    let map = terms::make_flatmap(
        &mut foreign,
        &[(make_atom(module_atom), make_atom(erl_stdlib_errors_atom))],
    )
    .expect("the error_info map builds");

    // Arm the machine exactly as a landed `error/3`-style raise would have left it.
    let mut machine = Machine::new();
    machine.raise_snapshot = vec![(0, 0, 0)];
    machine.raise_function_clause_args = Some(list);
    machine.raise_error_info = Some(map);

    // Cook on a SECOND arena, distinct from `foreign`.
    let mut ctx = MinimalContext::new();
    let cooked = symbolic_stacktrace(&mut machine, &module, &mut ctx, &Operand::Nil, false)
        .expect("the trace cooks");

    let items = list_elements(cooked).expect("the cooked trace is a proper list");
    let item = items[0];
    let a_slot = tuple_element(item, 2).expect("the A slot");
    let location = tuple_element(item, 3).expect("the location");
    let location_elements = list_elements(location).expect("the location is a proper list");
    let error_info_pair = location_elements[0];
    let map_copy = tuple_element(error_info_pair, 1).expect("the error_info map");

    // THE regression pin: both payload heads are a deep copy, not the foreign words.
    // Pre-fix `write_item` embedded the latches' own words verbatim, so these
    // comparisons were equal and the assertion failed.
    assert_ne!(
        a_slot.raw(),
        list.raw(),
        "the cooked A-slot argument list must be a deep copy, not the foreign list head"
    );
    assert_ne!(
        map_copy.raw(),
        map.raw(),
        "the cooked error_info map must be a deep copy, not the foreign map head"
    );

    // The copy (not the source) must back the cooked trace: after the foreign arena
    // is dropped the copied graph still renders its structure.
    drop(foreign);
    let a_shape = shape(a_slot);
    let map_shape = shape(map_copy);
    assert!(
        a_shape.starts_with("[binary|"),
        "the cooked A slot is [<<1>>]; got: {a_shape}"
    );
    assert_eq!(map_shape, "flatmap", "the cooked error_info map is a flat map");
}

/// A [`MinimalContext`] wrapper whose **rooted** allocation first runs a real
/// relocating collection (Tier-B [`MinimalContext::reclaim_heap`]) over the root set,
/// so the reservation [`symbolic_stacktrace`] takes actually **moves** the raise
/// latches' terms before the carve reads them back.
struct RelocatingContext {
    inner: MinimalContext,
    /// Whether the last rooted allocation really performed a copy (`Some` report), so
    /// a test can fail loudly instead of silently passing a no-op arena.
    moved: bool,
}

impl RelocatingContext {
    fn new() -> RelocatingContext {
        RelocatingContext {
            inner: MinimalContext::new(),
            moved: false,
        }
    }
}

impl ExecutionContext for RelocatingContext {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        // Move every term the root set reaches — including the loose raise latches
        // `Machine::with_roots` parked on the stack for this hand-off — into a fresh
        // survivor region before the inner arena serves the reservation.
        let report = self.inner.reclaim_heap(|reloc| {
            reloc.forward_roots(roots.x)?;
            reloc.forward_roots(roots.stack)?;
            reloc.forward_roots(roots.fr)?;
            Ok(())
        })?;
        self.moved = report.is_some();
        self.inner.alloc_words_rooted(n, roots)
    }
}

/// A reference-bearing `saved_args` payload survives the reservation's forced
/// relocating collection and cooks with the reference's minted id preserved — the
/// latch is re-read post-move and `bounded_copy` copies the M1 reference verbatim,
/// never a stale pointer or a typed `unsupported`.
#[allow(unsafe_code)]
#[test]
fn a_reference_bearing_saved_args_survives_a_forced_relocating_collection() {
    let mut fixture = Fixture::new(&[]);
    fixture.add_function("run", 0, vec![Instr::Label(Label(0)), Instr::Return]);
    let module = fixture.finish();

    let mut host = RelocatingContext::new();
    let mut atoms = beam_term::AtomTable::new();
    let bm = atoms.intern("bm");

    // Mint an M1 internal reference whose block lives outside the arena (a foreign
    // pointer, like a runtime-minted reference the host would keep elsewhere).
    let mut ref_words = [Eterm::default(); beam_term::INTERNAL_REF_WORDS];
    // SAFETY: `ref_words` is a live, aligned, exclusively-owned block of exactly
    // `INTERNAL_REF_WORDS` words — the writer's documented contract.
    let reference =
        unsafe { beam_term::write_internal_ref(ref_words.as_mut_ptr(), 42) }.unwrap();

    // Build the payload on the *inner* (non-relocating) arena so the intermediate
    // terms are not reclaimed while only held in Rust locals; the relocating host is
    // used only for the final cook, whose reservation roots and moves the graph.
    let binary = terms::make_heap_binary(&mut host.inner, &[1, 2, 3]).unwrap();
    let pair = terms::make_tuple(&mut host.inner, &[make_atom(bm), reference]).unwrap();
    let nil = terms::make_list(&mut host.inner, &[]).unwrap();
    let saved_args = terms::make_list(&mut host.inner, &[binary, pair, nil]).unwrap();

    let mut machine = Machine::new();
    machine.raise_snapshot = vec![(0, 0, 0)];
    machine.raise_function_clause_args = Some(saved_args);

    let cooked = symbolic_stacktrace(&mut machine, &module, &mut host, &Operand::Nil, false)
        .expect("the trace cooks");
    assert!(
        host.moved,
        "the reservation must have performed a real relocating collection"
    );

    let items = list_elements(cooked).expect("the cooked trace is a proper list");
    let item = items[0];
    let a_slot = tuple_element(item, 2).expect("the A slot");
    let a_elements = list_elements(a_slot).expect("the A slot is a proper list");
    assert_eq!(a_elements.len(), 3);
    let copied_pair = a_elements[1];
    assert_eq!(tuple_arity(copied_pair).expect("a tuple"), 2);
    let copied_ref = tuple_element(copied_pair, 1).expect("the pair's reference");

    // SAFETY: `copied_ref` is the M1 block `bounded_copy` just carved into the
    // reservation; the reference id word is the block's own immediate payload.
    assert!(unsafe { beam_term::is_internal_ref(copied_ref) });
    assert_eq!(
        unsafe { beam_term::internal_ref_id(copied_ref) }.expect("an M1 reference"),
        42
    );
    assert_ne!(copied_ref, reference, "the reference must be copied, not shared");
}

