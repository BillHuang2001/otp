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

//! Sizing consistency: the `size_of_copy` preflight against what the copier actually writes.
//!
//! The sized-then-copied contract the crate documents in `copy.rs` is **exact**: for every shape
//! this test builds, a destination heap of exactly `size_of_copy(term)` words accepts the copy and
//! the copy writes exactly that many words (`Heap::used`). Both directions are checked:
//!
//! 1. `size_object(term) == to.used()` and `to.used() <= to.capacity()` - the preflight is neither
//!    an under- nor an over-reservation, for a corpus spanning immediates-free compounds, deep and
//!    improper lists, flat maps, HAMT maps, funs, native records, on-heap bitstrings, off-heap MSO
//!    containers and refc binaries;
//! 2. a destination one word short is **refused before a single word is written** - the error is the
//!    typed message-heap allocation failure and `to.used() == 0` (no partial write), and no off-heap
//!    artifact is registered either.
//!
//! `beam_term::size_object` is deliberately **not** modified here (it lives in `beam_term`); this
//! file pins the agreement between it and `layout::object_words` - the per-object counter the
//! incremental allocator reserves with - and would fail loudly if either drifted.
//!
//! # One documented disagreeing shape (excluded from the corpus)
//!
//! A **standalone `ErlSubBits` match context** (a `SUB_BITS` whose `orig` is an on-heap bitstring
//! rather than a `BinRef`) is the one shape where the two sizings disagree by design:
//! `beam_term::size_object` reports the size of the fresh heap-bits window the *C* copier
//! materialises (`copy.c:232-239`), while this crate's copier treats the match context as an opaque
//! leaf and copies its `ERL_SUB_BITS_SIZE` words verbatim (a documented M1 gap - no match-context
//! constructor exists yet). The copier's preflight uses its **own** count, so the reservation is
//! still exactly what is written; only the `size_object` cross-check does not hold for that shape.

use beam_alloc::{Heap, RefcBinary};
use beam_common::{AllocKind, BeamError};
use beam_gc::Mso;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, size_of_copy, CopyContext};
use beam_term::prelude::*;
use beam_term::size_object;

/// A source heap with the atom table and off-heap table the shapes may need.
struct Src {
    heap: Heap,
    atoms: AtomTable,
    off: OffHeapTable,
}

impl Src {
    fn new() -> Src {
        Src {
            heap: Heap::new(1 << 16).expect("source heap"),
            atoms: AtomTable::new(),
            off: OffHeapTable::new(),
        }
    }
}

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first.
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the cons fits");
    }
    acc
}

/// Writes a flatmap or HAMT map on `heap`.
fn write_map_term(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    // SAFETY: `map_words_capacity` is a sound upper bound on the words `write_map` writes.
    unsafe {
        let hp = heap
            .alloc_words(map_words_capacity(pairs.len()))
            .expect("the map fits");
        write_map(hp, pairs, atoms).expect("the map builds")
    }
}

/// Builds an off-heap MSO reference container `[HEADER_BIN_REF, slot, 0]` on `heap`.
fn bin_ref(heap: &mut Heap, slot: usize) -> Eterm {
    // SAFETY: the heap owns these three words.
    unsafe {
        let p = heap.alloc_words(ERL_BIN_REF_SIZE).expect("the container fits");
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
        make_boxed(p as *const Eterm)
    }
}

// --- the corpus -----------------------------------------------------------

fn small_tuple(s: &mut Src) -> Eterm {
    s.heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the tuple fits")
}

fn deep_list(s: &mut Src) -> Eterm {
    let mut acc = NIL;
    for i in 0..500 {
        acc = s.heap.cons(make_small(i % 97), acc).expect("the cons fits");
    }
    acc
}

fn improper_list(s: &mut Src) -> Eterm {
    let tail = s.heap.tuple(&[make_small(9)]).expect("the tail fits");
    let inner = s.heap.cons(make_small(2), tail).expect("the inner pair fits");
    s.heap.cons(make_small(1), inner).expect("the outer pair fits")
}

fn flat_map(s: &mut Src) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (0..8).map(|i| (make_small(i), make_small(i * 3 + 1))).collect();
    write_map_term(&mut s.heap, &pairs, &s.atoms)
}

fn hamt_map(s: &mut Src) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (0..64)
        .map(|i| (make_small(i * 3), make_small(i * 1000 + 7)))
        .collect();
    write_map_term(&mut s.heap, &pairs, &s.atoms)
}

fn nested_graph(s: &mut Src) -> Eterm {
    let inner = s.heap.tuple(&[make_small(1), make_small(2)]).expect("fits");
    let list = build_list(&mut s.heap, &[inner, make_small(3), inner]);
    s.heap
        .tuple(&[inner, list, make_small(4), inner])
        .expect("fits")
}

fn local_fun(s: &mut Src) -> Eterm {
    let env = s.heap.tuple(&[make_small(1), make_small(2)]).expect("the env fits");
    // SAFETY: `fun_words(Local, 1)` words are reserved and `env` is a live term.
    unsafe {
        let hp = s.heap.alloc_words(fun_words(FunKind::Local, 1)).expect("fits");
        write_fun_local(hp, 2, 0x0212_0304_0506_0708, 11, &[env]).expect("a valid local fun")
    }
}

fn external_fun(s: &mut Src) -> Eterm {
    let module = s.atoms.intern("lists");
    let function = s.atoms.intern("reverse");
    // SAFETY: `fun_words(External, 0)` words are reserved.
    unsafe {
        let hp = s.heap.alloc_words(fun_words(FunKind::External, 0)).expect("fits");
        write_fun_external(hp, 1, module, function).expect("a valid external fun")
    }
}

fn native_record(s: &mut Src) -> Eterm {
    let module = s.atoms.intern("m");
    let name = s.atoms.intern("r");
    let k1 = make_atom(s.atoms.intern("alpha"));
    let k2 = make_atom(s.atoms.intern("beta"));
    // SAFETY: `record_block_words(2)` words are reserved and every value is a live term.
    unsafe {
        let hp = s.heap.alloc_words(record_block_words(2)).expect("the record fits");
        write_record(
            hp,
            module,
            name,
            false,
            &[(k1, make_small(11)), (k2, make_small(22))],
            make_small(0),
        )
        .expect("a valid native record")
    }
}

fn on_heap_bitstring(s: &mut Src) -> Eterm {
    let bytes = [0xA5u8; 9];
    let bits = bytes.len() * 8;
    // SAFETY: `heap_bits_size_words(bits)` words are reserved and the payload fits.
    unsafe {
        let hp = s.heap.alloc_words(heap_bits_size_words(bits)).expect("fits");
        write_heap_bits(hp, &bytes, bits).expect("the payload fits")
    }
}

fn off_heap_container(s: &mut Src) -> Eterm {
    let slot = s.off.push(Mso::new(0x0BAD_F00D_u32, 20));
    bin_ref(&mut s.heap, slot)
}

fn refc_binary(s: &mut Src) -> Eterm {
    let bytes: Vec<u8> = (0..200usize).map(|i| ((i * 7) % 251) as u8).collect();
    s.off.push_binary(RefcBinary::new(&bytes).expect("the payload allocates"));
    let data_ptr = s.off.binaries()[0].data_ptr();
    // SAFETY: 8 words are reserved for the pair; `s.off` owns the payload, which outlives it.
    unsafe {
        let hp = s.heap.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            data_ptr as *mut Eterm,
            core::ptr::null_mut(),
            data_ptr,
            0,
            bytes.len() * 8,
        )
    }
}

fn graph_with_refc_binary(s: &mut Src) -> Eterm {
    let bytes: Vec<u8> = (0..100usize).map(|i| (i % 97) as u8).collect();
    s.off.push_binary(RefcBinary::new(&bytes).expect("the payload allocates"));
    let data_ptr = s.off.binaries()[0].data_ptr();
    // SAFETY: 8 words are reserved for the pair; `s.off` owns the payload, which outlives it.
    let pair = unsafe {
        let hp = s.heap.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            data_ptr as *mut Eterm,
            core::ptr::null_mut(),
            data_ptr,
            0,
            bytes.len() * 8,
        )
    };
    let list = build_list(&mut s.heap, &[pair, make_small(1)]);
    s.heap.tuple(&[list, pair]).expect("the root fits")
}

/// The corpus: a name and a builder for each shape.
const CORPUS: &[(&str, fn(&mut Src) -> Eterm)] = &[
    ("small tuple", small_tuple),
    ("deep list (500 cells)", deep_list),
    ("improper list", improper_list),
    ("flat map (8 keys)", flat_map),
    ("HAMT map (64 keys)", hamt_map),
    ("nested graph", nested_graph),
    ("local fun", local_fun),
    ("external fun", external_fun),
    ("native record", native_record),
    ("on-heap bitstring", on_heap_bitstring),
    ("off-heap MSO container", off_heap_container),
    ("refc binary", refc_binary),
    ("graph with a shared refc binary", graph_with_refc_binary),
];

#[test]
fn the_documented_sizing_is_exact_for_every_shape() {
    for (name, build) in CORPUS {
        let mut src = Src::new();
        let term = build(&mut src);

        let sized = size_of_copy(term);
        assert!(sized > 0, "{name}: every corpus shape is a compound term");
        assert_eq!(
            sized,
            size_object(term),
            "{name}: `size_of_copy` is the `size_object` forwarder"
        );

        // A destination of EXACTLY `sized` words (no slack) must accept the whole copy.
        let mut dst = Heap::new(sized).expect("destination heap");
        let mut dst_off = OffHeapTable::new();
        let copy = {
            let mut ctx = CopyContext::new(&src.off, &mut dst_off);
            copy_struct(term, &mut dst, &mut ctx)
                .unwrap_or_else(|e| panic!("{name}: sized copy must succeed, got {e:?}"))
        };

        assert!(!is_immediate(copy), "{name}: a compound term is copied");
        assert_eq!(
            dst.used(),
            sized,
            "{name}: the copy writes exactly the preflighted word count"
        );
        assert!(
            dst.used() <= dst.capacity(),
            "{name}: the copy never overflows the destination"
        );
        // Only a shape containing a refc binary registers a destination artifact (its shared
        // handle); the MSO-container shape's object is registered too, so allow up to two.
        assert!(
            dst_off.binaries().len() + dst_off.len() <= 2,
            "{name}: no unexpected off-heap artifact"
        );
    }
}

#[test]
fn a_one_word_short_destination_is_refused_before_any_write() {
    for (name, build) in CORPUS {
        let mut src = Src::new();
        let term = build(&mut src);
        let sized = size_of_copy(term);
        assert!(sized > 1, "{name}: needs more than one word");

        let mut dst = Heap::new(sized - 1).expect("destination heap");
        let mut dst_off = OffHeapTable::new();
        match {
            let mut ctx = CopyContext::new(&src.off, &mut dst_off);
            copy_struct(term, &mut dst, &mut ctx)
        } {
            Err(BeamError::Alloc(failure)) => {
                assert_eq!(
                    failure.kind,
                    AllocKind::Message,
                    "{name}: exhaustion is attributed to the message heap"
                );
            }
            other => panic!("{name}: expected a typed allocation refusal, got {other:?}"),
        }
        assert_eq!(dst.used(), 0, "{name}: a refused copy writes no destination word");
        assert_eq!(dst.peak_used(), 0, "{name}: nothing was even temporarily written");
        assert!(
            dst_off.binaries().is_empty() && dst_off.is_empty(),
            "{name}: a refused copy registers no off-heap artifact"
        );
    }
}

#[test]
fn a_zero_sized_destination_still_refuses_a_payload() {
    // The degenerate boundary: no capacity at all. The refusal must still be the typed message
    // allocation failure rather than a panic or a partial write.
    let mut src = Src::new();
    let term = small_tuple(&mut src);
    let mut dst = Heap::new(0).expect("empty destination heap");
    match beam_message::copy_struct_simple(term, &mut dst) {
        Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Message),
        other => panic!("expected a typed refusal, got {other:?}"),
    }
    assert_eq!(dst.used(), 0);
}

#[test]
fn an_immediate_still_needs_no_destination_capacity() {
    // Immediates are shared, so a zero-capacity destination is enough for them.
    let mut dst = Heap::new(0).expect("empty destination heap");
    for term in [make_small(7), NIL, tuple0()] {
        let copy = beam_message::copy_struct_simple(term, &mut dst).expect("shared as-is");
        assert_eq!(copy.raw(), term.raw());
    }
    assert_eq!(dst.used(), 0);
}
