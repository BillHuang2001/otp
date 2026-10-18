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

//! Copy-boundary **completeness**: `copy_struct` is correct across every term class.
//!
//! Each test builds a term of one class on a source heap, copies it with
//! [`copy_struct_simple`](beam_message::copy_struct_simple) into a destination heap sized by
//! [`size_of_copy`](beam_message::size_of_copy), and asserts the copy is structurally `=:=` the
//! original (`beam_term::compare::eq`), is a genuine copy (a different object address), and that the
//! destination consumed a word count consistent with the documented sizing.
//!
//! The destination heap is always owned by the *caller* of the copy helper (never by the helper):
//! a [`Heap`](beam_alloc::Heap) frees its region when it is dropped, so a helper that owned it
//! would hand back a dangling term.
//!
//! # Classes and how they are checked
//!
//! * `immediates` - small ints, atoms, `NIL`, internal pid/port/catch: shared by identity, no word.
//! * `cons lists` - proper **and** improper (dotted) pairs.
//! * `tuples` - nested, zero-arity (the shared literal), and a native `RECORD_SUBTAG` record.
//! * `maps` - a flat map (`<= 32` keys), a HAMT/hash map (`> 32` keys, head-bitmap/inner-node) and
//!   a HAMT **collision node** (the tuple of `[Key, Value]` cells two keys share when their full
//!   64-bit hashes agree).
//! * `funs` - `Local` (`make_fun3`), `External` (`EXPORT_EXT`), `Imported` (`NEW_FUN_EXT`).
//! * `bitstrings` - an on-heap small binary (`ErlHeapBits`), byte-aligned and not.
//! * `numbers` - a boxed float (`HEADER_FLONUM`) and a positive/negative bignum (`write_big`).
//!
//! `beam_term::compare` has a layout-aware arm for every class here - including external
//! pid/port/reference (`cmp_external_*`, see `tests/external.rs`) and `RECORD_SUBTAG`
//! (`cmp_records`) - so a copy is `=:=` its original for all of them. Each test asserts that
//! structural equality through `compare::eq` and, where the copy itself is not enough to pin the
//! shape, additionally through the layout accessors (`record_*`, `external_*_parts`). Refc
//! (off-heap) binaries share their payload rather than deep-copying - see `tests/binary.rs`.

use beam_alloc::Heap;
use beam_message::{copy_struct_simple, size_of_copy};
use beam_term::prelude::*;
use beam_term::{compare, size_object};

/// A heap big enough for any single test's source graph.
fn src_heap() -> Heap {
    Heap::new(4096).expect("source heap")
}

/// A destination heap sized for the documented `size_of_copy` count plus slack.
///
/// Because the capacity comes from `size_of_copy`, a test that copies successfully also proves the
/// preflight size is sufficient for that class.
fn dest_for(term: Eterm) -> Heap {
    Heap::new(size_of_copy(term) + 8).expect("destination heap")
}

/// Copies `term` into the caller-owned `dst` and returns the copy.
fn copy_into(term: Eterm, dst: &mut Heap) -> Eterm {
    copy_struct_simple(term, dst).expect("the copy succeeds")
}

/// Asserts `copy` is structurally equal to `original` and is a distinct object (a real copy).
fn assert_faithful_copy(original: Eterm, copy: Eterm, atoms: &AtomTable) {
    assert!(
        compare::eq(original, copy, atoms),
        "the copy must be =:= the original"
    );
    assert!(!is_immediate(original), "only compound terms are copied");
    assert_ne!(
        object_addr(original),
        object_addr(copy),
        "the copy must be a distinct object"
    );
}

/// The address of the object a non-immediate compound term points at.
fn object_addr(term: Eterm) -> usize {
    if is_list(term) {
        list_val(term) as usize
    } else {
        boxed_val(term) as usize
    }
}

// ---------------------------------------------------------------------------
// Immediates
// ---------------------------------------------------------------------------

#[test]
fn immediates_are_shared_by_identity() {
    let mut atoms = AtomTable::new();
    let atom = make_atom(atoms.intern("shared_atom"));
    let pid = make_internal_pid(make_pid_data(0x0000_7, 0x0000_1234));
    let port = make_internal_port(make_port_data(0xCAFE));

    for term in [make_small(0), make_small(-7), atom, NIL, pid, port, make_catch(9)] {
        let mut dst = Heap::new(4).expect("destination heap");
        let copy = copy_into(term, &mut dst);
        assert_eq!(copy.raw(), term.raw(), "an immediate is returned by identity");
        assert_eq!(dst.used(), 0, "an immediate touches no destination word");
    }
}

#[test]
fn a_tuple_of_immediates_copies_the_object_only() {
    let mut src = src_heap();
    let t = src
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the tuple fits");
    let mut dst = dest_for(t);
    let copy = copy_into(t, &mut dst);
    assert_faithful_copy(t, copy, &AtomTable::new());
    assert_eq!(size_object(t), 4, "header + three immediate elements");
    assert_eq!(dst.used(), 4, "exactly the one object is written");
}

// ---------------------------------------------------------------------------
// Cons lists: proper and improper
// ---------------------------------------------------------------------------

fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the list fits");
    }
    acc
}

#[test]
fn a_proper_list_of_boxed_elements_copies_recursively() {
    let mut src = src_heap();
    let a = src.tuple(&[make_small(1), make_small(2)]).expect("fits");
    let b = src.tuple(&[make_small(3)]).expect("fits");
    let list = build_list(&mut src, &[a, b, make_small(9)]);

    let mut dst = dest_for(list);
    let copy = copy_into(list, &mut dst);
    assert_faithful_copy(list, copy, &AtomTable::new());
    // 3 cons cells (2 words each) + the two nested tuples (3 + 2 words).
    assert_eq!(dst.used(), 3 * 2 + 3 + 2);
    assert_eq!(dst.used(), size_object(list));
}

#[test]
fn an_improper_dotted_pair_copies_its_non_nil_tail() {
    let mut src = src_heap();
    let tail = src.tuple(&[make_small(42)]).expect("the tail tuple fits");
    // `[1, 2 | {42}]` - a dotted pair whose cdr is not `NIL` and not a cons cell.
    let inner = src.cons(make_small(2), tail).expect("the inner pair fits");
    let dotted = src.cons(make_small(1), inner).expect("the outer pair fits");

    // SAFETY: `dotted` is a live, non-NIL cons cell.
    assert!(!is_nil(unsafe { cdr(dotted) }), "the last cdr is not NIL");
    let mut dst = dest_for(dotted);
    let copy = copy_into(dotted, &mut dst);
    assert_faithful_copy(dotted, copy, &AtomTable::new());

    // The tail tuple was copied (a distinct object), not shared from the source.
    // SAFETY: `copy` is a live, non-NIL cons chain of length 2 with a boxed tail.
    let copied_tail = unsafe { cdr(cdr(copy)) };
    assert!(is_arity_value(unsafe { *boxed_val(copied_tail) }));
    assert_ne!(boxed_val(copied_tail), boxed_val(tail), "the tail is a real copy");
}

// ---------------------------------------------------------------------------
// Tuples, zero-arity, and native records
// ---------------------------------------------------------------------------

#[test]
fn nested_tuples_sized_exactly() {
    let mut src = src_heap();
    let inner = src.tuple(&[make_small(1), make_small(2)]).expect("fits");
    let outer = src.tuple(&[inner, make_small(3), inner]).expect("fits");
    let mut dst = dest_for(outer);
    let copy = copy_into(outer, &mut dst);
    assert_faithful_copy(outer, copy, &AtomTable::new());
    // The flat copy duplicates `inner`: 4 (outer) + 3 (inner) + 3 (inner again).
    assert_eq!(size_object(outer), 10);
    assert_eq!(dst.used(), 10);
}

#[test]
fn the_zero_arity_tuple_is_the_shared_literal() {
    let mut src = src_heap();
    let t0 = src.tuple(&[]).expect("the empty tuple fits");
    let outer = src.tuple(&[t0, make_small(1)]).expect("fits");

    let mut dst = dest_for(outer);
    let copy = copy_into(outer, &mut dst);
    assert_faithful_copy(outer, copy, &AtomTable::new());
    // SAFETY: `copy` is a live 2-tuple.
    let copied_t0 = unsafe { tuple_element(copy, 0) };
    assert_eq!(
        copied_t0.raw(),
        tuple0().raw(),
        "a zero-arity tuple copies to the shared global literal"
    );
    assert_eq!(dst.used(), 3, "only the outer object is written");
}

#[test]
fn a_native_record_copies_its_definition_and_values() {
    let mut atoms = AtomTable::new();
    let module = atoms.intern("m");
    let name = atoms.intern("r");
    let k1 = make_atom(atoms.intern("alpha"));
    let k2 = make_atom(atoms.intern("beta"));

    let mut src = src_heap();
    // SAFETY: `record_block_words(2)` words are reserved and every field value is a live term.
    let record = unsafe {
        let hp = src.alloc_words(record_block_words(2)).expect("the record fits");
        write_record(
            hp,
            module,
            name,
            false,
            &[(k1, make_small(11)), (k2, make_small(22))],
            make_small(0),
        )
        .expect("a valid native record")
    };

    let mut dst = dest_for(record);
    let copy = copy_into(record, &mut dst);
    // The construction block is `[field_order][definition][instance]` and the copier reproduces
    // exactly those three objects (the instance root, its definition through instance word 1, and
    // the definition's `field_order` tuple through definition word 1), so the flat size equals the
    // whole block.
    assert_eq!(
        size_of_copy(record),
        record_block_words(2),
        "the instance, its definition and its field_order tuple are the whole block"
    );
    assert_eq!(dst.used(), record_block_words(2), "nothing is written twice");

    // `compare`'s `cmp_records` arm compares the definition's `(module, name, is_exported)`,
    // the field count and keys, then the values - never the inlined definition pointer - so
    // the copy is `=:=` its original. The accessors then pin the shape explicitly.
    assert!(compare::eq(record, copy, &atoms), "the copied native record is =:= its original");
    // SAFETY: both terms are live native records.
    unsafe {
        assert!(record_is_native(copy, module, name));
        assert_eq!(record_field_count(copy).expect("a well-formed record"), 2);
        assert_eq!(
            record_get(copy, k1, &atoms).expect("a well-formed record"),
            Some(make_small(11))
        );
        assert_eq!(
            record_get(copy, k2, &atoms).expect("a well-formed record"),
            Some(make_small(22))
        );
    }
    assert_ne!(
        boxed_val(copy),
        boxed_val(record),
        "the record instance is a genuine copy"
    );
    // The copied definition is a distinct object reached from the copy, not the source's.
    // SAFETY: `copy` is a live native record.
    let copied_def = unsafe { record_definition(copy) }.expect("a well-formed record");
    // SAFETY: `record` is a live native record.
    let src_def = unsafe { record_definition(record) }.expect("a well-formed record");
    assert_ne!(boxed_val(copied_def), boxed_val(src_def));
}

// ---------------------------------------------------------------------------
// Maps: flat and HAMT
// ---------------------------------------------------------------------------

fn write_map_term(src: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    // SAFETY: `map_words_capacity` is a sound upper bound on the words `write_map` writes.
    unsafe {
        let hp = src
            .alloc_words(map_words_capacity(pairs.len()))
            .expect("the map fits");
        write_map(hp, pairs, atoms).expect("the map builds")
    }
}

#[test]
fn a_flat_map_copies_as_a_flat_map() {
    let atoms = AtomTable::new();
    let pairs: Vec<(Eterm, Eterm)> = (0..8)
        .map(|i| (make_small(i), make_small(i * 10 + 3)))
        .collect();

    let mut src = src_heap();
    let map = write_map_term(&mut src, &pairs, &atoms);
    // SAFETY: `map` is a live boxed map.
    assert!(is_flatmap_header(unsafe { *boxed_val(map) }), "8 keys => a flat map");

    let mut dst = dest_for(map);
    let copy = copy_into(map, &mut dst);
    assert_faithful_copy(map, copy, &atoms);
    assert_eq!(size_object(map), size_of_copy(map));
    assert_eq!(dst.used(), size_object(map));
    assert_eq!(map_size(copy, &atoms).expect("well-formed"), 8);
    for (k, v) in &pairs {
        assert_eq!(
            map_get(copy, *k, &atoms).expect("well-formed"),
            Some(*v),
            "key {k:?} survives the copy"
        );
    }
}

#[test]
fn a_hamt_map_copies_as_a_whole_trie() {
    let atoms = AtomTable::new();
    // 64 distinct keys exceed `MAP_SMALL_MAP_LIMIT` (32) -> a genuine HAMT with sub-nodes.
    let pairs: Vec<(Eterm, Eterm)> = (0..64)
        .map(|i| (make_small(i * 3), make_small(i * 1000 + 7)))
        .collect();

    let mut src = src_heap();
    let map = write_map_term(&mut src, &pairs, &atoms);
    // SAFETY: `map` is a live boxed map.
    let head = unsafe { *boxed_val(map) };
    assert!(is_map_header(head) && !is_flatmap_header(head), "64 keys => a HAMT");

    let mut dst = dest_for(map);
    let copy = copy_into(map, &mut dst);
    assert_faithful_copy(map, copy, &atoms);
    assert_eq!(size_object(map), size_of_copy(map));
    assert_eq!(dst.used(), size_object(map), "every trie node is written once");
    assert_eq!(map_size(copy, &atoms).expect("well-formed"), 64);

    // SAFETY: `copy` is a live boxed map of the same shape.
    let copy_head = unsafe { *boxed_val(copy) };
    assert_eq!(
        map_header_type(copy_head),
        map_header_type(head),
        "the copied head keeps the source's HAMT head kind"
    );
    let copied_inner = count_inner_nodes(copy);
    assert!(copied_inner > 0, "a 64-key trie has sub-nodes");
    assert_eq!(
        copied_inner,
        count_inner_nodes(map),
        "every source sub-node is reproduced"
    );

    for (k, v) in &pairs {
        assert_eq!(map_get(copy, *k, &atoms).expect("well-formed"), Some(*v));
    }
}

/// Counts the inner (`HAMT_NODE_BITMAP`) trie nodes reachable from the hash-map node `node`
/// iteratively. Leaf cons cells are `LIST` words and collision tuples are `ARITYVAL` terms, so
/// both are skipped (they hold no further sub-node).
fn count_inner_nodes(node: Eterm) -> usize {
    let mut count = 0;
    let mut stack = vec![node];
    while let Some(term) = stack.pop() {
        if !is_boxed(term) {
            continue;
        }
        // SAFETY: `term` is boxed, so its header word is live.
        let head = unsafe { *boxed_val(term) };
        if !is_map_header(head) {
            continue;
        }
        let arity = header_arity(head);
        let nslots = map_header_val(head).count_ones() as usize;
        if map_header_type(head) as usize == MAP_HEADER_TAG_HAMT_NODE_BITMAP {
            count += 1;
        }
        for i in 0..nslots {
            // SAFETY: slot `i` is within the node's payload.
            let slot = unsafe { *boxed_val(term).add(1 + arity + i) };
            stack.push(slot);
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Funs: Local / External / Imported
// ---------------------------------------------------------------------------

#[test]
fn a_local_closure_copies_with_its_environment() {
    let mut atoms = AtomTable::new();
    let tag = make_atom(atoms.intern("tag"));
    let mut src = src_heap();
    let free = src.tuple(&[make_small(7), tag]).expect("the free var fits");
    // SAFETY: `fun_words(Local, 1)` words are reserved; `free` is a live term.
    let fun = unsafe {
        let hp = src.alloc_words(fun_words(FunKind::Local, 1)).expect("fits");
        write_fun_local(hp, 2, 0x1122_3344_5566_7788, 5, &[free]).expect("a valid local fun")
    };

    let mut dst = dest_for(fun);
    let copy = copy_into(fun, &mut dst);
    assert_faithful_copy(fun, copy, &atoms);
    assert_eq!(size_object(fun), fun_words(FunKind::Local, 1) + size_object(free));
    assert_eq!(dst.used(), size_object(fun), "the fun block and its env exactly");
    // SAFETY: `copy` is a live local fun.
    assert_eq!(unsafe { fun_uniq(copy) }.expect("uniq survives"), 0x1122_3344_5566_7788);
    assert_eq!(unsafe { fun_index(copy) }.expect("index survives"), 5);
    // The environment is a distinct copied object, reached at `1 + desc_words(Local)`.
    // SAFETY: `copy` is a live local fun with one free variable.
    let copied_env = unsafe { fun_env(copy) }.expect("one free variable");
    assert_eq!(copied_env.len(), 1);
    assert_ne!(boxed_val(copied_env[0]), boxed_val(free), "the env is a real copy");
}

#[test]
fn an_external_export_fun_copies_the_module_and_function() {
    let mut atoms = AtomTable::new();
    let module = atoms.intern("lists");
    let function = atoms.intern("reverse");
    let arity = 2;

    let mut src = src_heap();
    // SAFETY: `fun_words(External, 0)` words (the 4-word block) are reserved.
    let fun = unsafe {
        let hp = src.alloc_words(fun_words(FunKind::External, 0)).expect("fits");
        write_fun_external(hp, arity, module, function).expect("a valid external fun")
    };

    let mut dst = dest_for(fun);
    let copy = copy_into(fun, &mut dst);
    assert_faithful_copy(fun, copy, &atoms);
    assert_eq!(size_object(fun), 1 + 3, "header + module + function + arity");
    assert_eq!(dst.used(), 4, "the whole 4-word block is copied");
    // The 4-word block is copied verbatim, so the true arity (descriptor word 3, reached through
    // `fun_true_arity` rather than the masked header field) survives the copy.
    // SAFETY: `fun` and `copy` are live external-fun terms.
    let (src_arity, copied_arity) = unsafe {
        (
            fun_true_arity(fun).expect("a valid external fun"),
            fun_true_arity(copy).expect("the copy is a valid external fun"),
        )
    };
    assert_eq!(copied_arity, src_arity, "the copied arity word equals the source's");
    assert_eq!(copied_arity, arity);
}

#[test]
fn an_imported_new_fun_ext_copies_its_signature() {
    let mut atoms = AtomTable::new();
    let module = atoms.intern("m");
    let mut uniq = [0u8; 16];
    for (i, b) in uniq.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(1);
    }
    let creator = FunPid {
        node: atoms.intern("n1@host"),
        number: 0xAAAA_BBBB,
        serial: 0xCCCC_DDDD,
        creation: 0x1122_3344,
    };

    let mut src = src_heap();
    let free = src.tuple(&[make_small(1), make_small(2)]).expect("the free var fits");
    // SAFETY: `fun_words(Imported, 1)` words are reserved; `free` is a live term.
    let fun = unsafe {
        let hp = src.alloc_words(fun_words(FunKind::Imported, 1)).expect("fits");
        write_fun_imported(hp, 2, module, &uniq, 0x0102_0304, 7, 3, creator, &[free])
            .expect("a valid imported fun")
    };

    let mut dst = dest_for(fun);
    let copy = copy_into(fun, &mut dst);
    assert_faithful_copy(fun, copy, &atoms);
    assert_eq!(
        size_object(fun),
        fun_words(FunKind::Imported, 1) + size_object(free)
    );
    assert_eq!(dst.used(), size_object(fun));
}

// ---------------------------------------------------------------------------
// On-heap small bitstrings
// ---------------------------------------------------------------------------

#[test]
fn an_on_heap_small_binary_copies_by_value() {
    let bytes = [0x01u8, 0x23, 0x45, 0x67, 0x89];
    let bits = bytes.len() * 8;
    let mut src = src_heap();
    // SAFETY: `heap_bits_size_words(bits)` words are reserved and the payload fits.
    let binary = unsafe {
        let hp = src.alloc_words(heap_bits_size_words(bits)).expect("fits");
        write_heap_bits(hp, &bytes, bits).expect("the payload fits the words")
    };

    let mut dst = dest_for(binary);
    let copy = copy_into(binary, &mut dst);
    assert_faithful_copy(binary, copy, &AtomTable::new());
    assert_eq!(size_object(binary), heap_bits_size_words(bits));
    assert_eq!(dst.used(), heap_bits_size_words(bits));
    // SAFETY: `copy` is a live `ErlHeapBits` of the same shape.
    let (base, start, size) = unsafe { bitstring_parts(copy) };
    assert_eq!((start, size), (0, bits));
    // SAFETY: `binary` is a live `ErlHeapBits`; only its base pointer is read.
    let src_base = unsafe { bitstring_parts(binary) }.0;
    assert_ne!(
        base as usize, src_base as usize,
        "a small binary is copied by value, not shared"
    );
    // SAFETY: the payload spans bits `0..bits`, i.e. `ceil(bits/8)` live bytes at `base`.
    let copied = unsafe { core::slice::from_raw_parts(base, bits.div_ceil(8)) };
    assert_eq!(copied, &bytes[..]);
}

#[test]
fn a_non_byte_aligned_bitstring_copies_by_value() {
    // 13 bits: a non-byte-aligned window whose last byte has 3 live bits.
    let bits = 13usize;
    let bytes = [0b1011_0110u8, 0b0000_0101u8];
    let mut src = src_heap();
    // SAFETY: `heap_bits_size_words(bits)` words are reserved; the payload fits.
    let binary = unsafe {
        let hp = src.alloc_words(heap_bits_size_words(bits)).expect("fits");
        write_heap_bits(hp, &bytes, bits).expect("the payload fits the words")
    };

    let mut dst = dest_for(binary);
    let copy = copy_into(binary, &mut dst);
    assert_faithful_copy(binary, copy, &AtomTable::new());
    // SAFETY: `copy` is a live `ErlHeapBits` with a 13-bit window.
    let (base, start, size) = unsafe { bitstring_parts(copy) };
    assert_eq!((start, size), (0, bits));
    // Every *live* bit is reproduced (the 3 trailing bits of the last byte are padding and are not
    // part of the string, so they are not compared).
    for i in 0..bits {
        // SAFETY: both terms are live bitstrings of at least `bits` live bits.
        let (want, got) = unsafe { (bit_at(binary, i), bit_at(copy, i)) };
        assert_eq!(got, want, "bit {i} survives the copy");
    }
    // The copy is a real object with its own payload region.
    // SAFETY: `binary` is a live `ErlHeapBits`; only its base pointer is read.
    let src_base = unsafe { bitstring_parts(binary) }.0;
    assert_ne!(base as usize, src_base as usize, "copied by value, not shared");
}

// ---------------------------------------------------------------------------
// Numeric leaves: boxed floats and bignums
// ---------------------------------------------------------------------------

#[test]
fn a_boxed_float_copies_by_value() {
    let value = core::f64::consts::PI;
    let mut src = src_heap();
    // SAFETY: `write_float` fills exactly the `FLOAT_SIZE_WORDS` (2) words reserved here.
    let float = unsafe {
        let hp = src.alloc_words(FLOAT_SIZE_WORDS).expect("the float fits");
        write_float(hp, value)
    };

    let mut dst = dest_for(float);
    let copy = copy_into(float, &mut dst);
    assert_faithful_copy(float, copy, &AtomTable::new());
    assert_eq!(size_object(float), FLOAT_SIZE_WORDS, "header + the f64 bits word");
    assert_eq!(dst.used(), size_object(float), "exactly the 2-word flonum block");
    // A float is copied by value: the copy carries the same `f64` bit pattern at a new address.
    // SAFETY: both terms are live boxed floats.
    let (src_bits, copied_bits) = unsafe { (float_bits(float), float_bits(copy)) };
    assert_eq!(copied_bits, src_bits, "the f64 bit pattern is preserved");
    // SAFETY: `copy` is a live boxed float.
    assert_eq!(unsafe { float_value(copy) }, value, "the value reads back");
}

#[test]
fn a_bignum_copies_by_value_for_both_signs() {
    // A 2-digit magnitude (2^64 + 5): one header word plus two `ErtsDigit`s.
    let digits = [5u64, 1u64];
    for negative in [false, true] {
        let mut src = src_heap();
        // SAFETY: `big_size_words(2)` (3) words are reserved and `digits` is non-empty.
        let big = unsafe {
            let hp = src
                .alloc_words(big_size_words(digits.len()))
                .expect("the bignum fits");
            write_big(hp, negative, &digits)
        };

        let mut dst = dest_for(big);
        let copy = copy_into(big, &mut dst);
        assert_faithful_copy(big, copy, &AtomTable::new());
        assert_eq!(
            size_object(big),
            big_size_words(digits.len()),
            "header + one word per ErtsDigit"
        );
        assert_eq!(dst.used(), size_object(big), "exactly the bignum block");
        // SAFETY: `copy` is a live bignum of the same shape as `big`.
        let (copied_neg, copied_digits) = unsafe { big_parts(copy) };
        assert_eq!(copied_neg, negative, "the header's sign bit is preserved");
        assert_eq!(copied_digits, &digits[..], "every digit is copied verbatim");
    }
}

// ---------------------------------------------------------------------------
// HAMT collision tuples
// ---------------------------------------------------------------------------

/// Builds a two-key hash map whose single head slot is a **collision node** — the ordinary tuple
/// of `[Key, Value]` cons cells two keys share when their full 64-bit hashes agree (`erl_map.h`;
/// `map.rs`'s `TrieChild::Collision`). Forcing a genuine 64-bit hash collision through
/// [`write_map`] is impractical, so the node is laid out with the public builders
/// ([`make_map_header`] + [`make_arityval`] + [`make_list`]), exactly as `beam_term`'s own
/// `size`/`layout` unit tests lay out a HAMT by hand.
fn build_collision_map(src: &mut Heap) -> Eterm {
    // SAFETY: every `alloc_words` call below reserves exactly the words written into it, and every
    // word of the produced map is initialised before the root term escapes.
    unsafe {
        let cell_a = src.alloc_words(2).expect("cell a fits");
        *cell_a = make_small(1);
        *cell_a.add(1) = make_small(11);
        let cell_b = src.alloc_words(2).expect("cell b fits");
        *cell_b = make_small(2);
        *cell_b.add(1) = make_small(22);

        // The collision node: a plain 3-word tuple `[arityval(2), cell_a, cell_b]`.
        let coll = src.alloc_words(3).expect("the collision tuple fits");
        *coll = make_arityval(2);
        *coll.add(1) = make_list(cell_a as *const Eterm);
        *coll.add(2) = make_list(cell_b as *const Eterm);
        let coll_term = make_boxed(coll as *const Eterm);

        // The head node: `[header, size, slot0]`, a one-slot bitmap head pointing at the collision
        // node (`make_map_header(type, arity, val)` = `[tag|type, arity, val]`).
        let head = src.alloc_words(3).expect("the head node fits");
        *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 0b1);
        *head.add(1) = Eterm::from_raw(2); // the head's `size` word: the total key count
        *head.add(2) = coll_term;
        make_boxed(head as *const Eterm)
    }
}

#[test]
fn a_hamt_collision_tuple_copies_with_its_cells() {
    let atoms = AtomTable::new();
    let mut src = src_heap();
    let map = build_collision_map(&mut src);
    // SAFETY: `map` is a live boxed hash-map head node.
    let head = unsafe { *boxed_val(map) };
    assert!(is_map_header(head), "the head is a HAMT node");
    assert!(
        !is_flatmap_header(head),
        "a collision node lives only in a hash map, never a flat map"
    );
    // SAFETY: `map` is a live boxed head node with one bitmap slot.
    let src_collision = unsafe { *boxed_val(map).add(2) };

    let mut dst = dest_for(map);
    let copy = copy_into(map, &mut dst);
    // `compare` navigates the trie (the collision tuple is walked as a tuple of cells), so a
    // faithful copy is `=:=` the original.
    assert_faithful_copy(map, copy, &atoms);
    // Head node (3) + the collision tuple (3) + its two 2-word cons cells.
    assert_eq!(size_object(map), 3 + 3 + 2 + 2);
    assert_eq!(
        dst.used(),
        size_object(map),
        "the collision node and every cell is written once"
    );
    assert_eq!(map_size(copy, &atoms).expect("well-formed"), 2);
    // The pairs survive (read through `map_pairs`, which walks every slot rather than following a
    // hash path — a hand-laid collision node has no meaningful nibble path).
    assert_eq!(
        map_pairs(copy, &atoms).expect("well-formed"),
        vec![
            (make_small(1), make_small(11)),
            (make_small(2), make_small(22)),
        ]
    );
    // The copied collision tuple is a distinct object reached from the copied head.
    // SAFETY: `copy` is a live boxed head node with one bitmap slot.
    let copied_collision = unsafe { *boxed_val(copy).add(2) };
    assert_ne!(
        object_addr(copied_collision),
        object_addr(src_collision),
        "the collision tuple is a real copy"
    );
    assert!(
        is_arity_value(unsafe { *boxed_val(copied_collision) }),
        "the copied slot is still a collision tuple"
    );
}
