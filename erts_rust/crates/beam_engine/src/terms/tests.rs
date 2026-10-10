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

//! Round-trips through the [`MinimalContext`] arena, plus raw-word layout
//! assertions against stack buffers (allowed here — this is the SAFETY
//! module) and the wrong-type / limit error cases.
use super::*;
// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so they use
// the `test_support` wrappers that keep the old ergonomic arity. An explicit import
// shadows the glob one above, which still supplies the readers and the constants.
use super::test_support::{
    badmatch_reason, make_cons, make_flatmap, make_float, make_fun, make_heap_binary,
    make_integer, make_list, make_tuple,
};
use crate::context::{ExecutionContext, MinimalContext};
use crate::machine::Machine;
use beam_common::{BeamError, Result};
use beam_loader::Module;
use beam_term::{make_atom, make_small, Eterm, MAX_SMALL, MIN_SMALL, NIL};
fn ctx() -> MinimalContext {
    MinimalContext::new()
}

/// Asserts that a reader rejected a mismatched term with `BadArg`.
fn assert_bad_arg<T: core::fmt::Debug>(r: Result<T>) {
    match r {
        Err(BeamError::BadArg) => {}
        other => panic!("expected BadArg, got {other:?}"),
    }
}

// -- tuples ---------------------------------------------------------------

#[test]
fn tuple_round_trips_through_the_ctx() {
    let mut c = ctx();
    let (a, b, d) = (make_small(1), make_small(2), make_small(3));
    let t = make_tuple(&mut c, &[a, b, d]).unwrap();
    assert_eq!(tuple_arity(t).unwrap(), 3);
    assert_eq!(tuple_element(t, 0).unwrap(), a);
    assert_eq!(tuple_element(t, 1).unwrap(), b);
    assert_eq!(tuple_element(t, 2).unwrap(), d);
    assert_bad_arg(tuple_element(t, 3));
    assert_bad_arg(tuple_element(t, usize::MAX));
    assert_bad_arg(tuple_arity(a));
    assert_bad_arg(tuple_arity(NIL));
}

#[test]
fn empty_tuple_is_the_shared_zero_arity_literal() {
    let mut c = ctx();
    let t = make_tuple(&mut c, &[]).unwrap();
    assert_eq!(t, beam_term::tuple0());
    assert_eq!(tuple_arity(t).unwrap(), 0);
}

#[test]
fn tuple_layout_is_arityval_then_elements() {
    // Raw-word proof of the exact header/element offsets.
    let mut words = [make_small(0); 3];
    words[0] = beam_term::make_arityval(2);
    words[1] = make_small(10);
    words[2] = make_small(20);
    let t = beam_term::make_tuple(words.as_mut_ptr());
    assert_eq!(tuple_arity(t).unwrap(), 2);
    assert_eq!(tuple_element(t, 0).unwrap(), make_small(10));
    assert_eq!(tuple_element(t, 1).unwrap(), make_small(20));
    // SAFETY: `words` is the live 3-word tuple block backing `t`, so its header
    // word is a valid term word.
    assert_eq!(
        unsafe { *beam_term::boxed_val(t) },
        beam_term::make_arityval(2)
    );
}

// -- lists ----------------------------------------------------------------

#[test]
fn list_round_trips_through_the_ctx() {
    let mut c = ctx();
    let elems = [make_small(1), make_small(2), make_small(3)];
    let l = make_list(&mut c, &elems).unwrap();
    assert_eq!(list_elements(l).unwrap(), elems);
    assert_eq!(cons_head(l).unwrap(), elems[0]);
    let tail = cons_tail(l).unwrap();
    assert_eq!(list_elements(tail).unwrap(), elems[1..]);
}

#[test]
fn empty_list_is_nil_and_nil_reads_as_empty() {
    let mut c = ctx();
    assert_eq!(make_list(&mut c, &[]).unwrap(), NIL);
    assert_eq!(list_elements(NIL).unwrap(), Vec::<Eterm>::new());
    assert_bad_arg(cons_head(NIL));
    assert_bad_arg(cons_tail(NIL));
}

#[test]
fn improper_list_is_rejected_by_list_elements() {
    let cell = [make_small(1), make_small(2)];
    let improper = beam_term::make_list(cell.as_ptr());
    assert_eq!(cons_head(improper).unwrap(), make_small(1));
    assert_eq!(cons_tail(improper).unwrap(), make_small(2));
    assert_bad_arg(list_elements(improper));
    assert_bad_arg(cons_tail(make_small(3)));
}

#[test]
fn cons_cell_layout_is_car_then_cdr() {
    let mut c = ctx();
    let l = make_list(&mut c, &[make_small(7)]).unwrap();
    // SAFETY: `l` is the live 2-word cons cell the context arena owns.
    unsafe {
        assert_eq!(*beam_term::list_val(l), make_small(7));
        assert_eq!(*beam_term::list_val(l).add(1), NIL);
    }
}

// -- cons (`put_list`) ----------------------------------------------------

#[test]
fn cons_round_trips_through_the_ctx() {
    let mut c = ctx();
    let (h, t) = (make_small(1), make_small(2));
    let cell = make_cons(&mut c, h, t).unwrap();
    assert!(beam_term::is_list(cell));
    assert_eq!(cons_head(cell).unwrap(), h);
    assert_eq!(cons_tail(cell).unwrap(), t);
    // SAFETY: `cell` is the live 2-word cons cell the arena owns.
    unsafe {
        assert_eq!(*beam_term::list_val(cell), h);
        assert_eq!(*beam_term::list_val(cell).add(1), t);
    }
}

#[test]
fn cons_accepts_a_proper_nil_tail() {
    let mut c = ctx();
    let cell = make_cons(&mut c, make_small(1), NIL).unwrap();
    assert_eq!(cons_tail(cell).unwrap(), NIL);
    assert_eq!(list_elements(cell).unwrap(), vec![make_small(1)]);
}

#[test]
fn cons_allows_an_improper_tail() {
    // `[1 | 2]` is a legal Erlang term; `list_elements` rejects it, but the
    // cell itself reads fine (this is what `put_list` must build).
    let mut c = ctx();
    let cell = make_cons(&mut c, make_small(1), make_small(2)).unwrap();
    assert_eq!(cons_head(cell).unwrap(), make_small(1));
    assert_eq!(cons_tail(cell).unwrap(), make_small(2));
    assert_bad_arg(list_elements(cell));
}

#[test]
fn make_list_builds_from_cons_cells() {
    // `make_list` is now a fold over `make_cons`; the proper-list shape and
    // element order must be unchanged.
    let mut c = ctx();
    let elems = [make_small(10), make_small(20), make_small(30)];
    let l = make_list(&mut c, &elems).unwrap();
    let mut cursor = l;
    for elem in elems {
        assert_eq!(cons_head(cursor).unwrap(), elem);
        cursor = cons_tail(cursor).unwrap();
    }
    assert_eq!(cursor, NIL);
}

// -- badmatch reason ------------------------------------------------------

#[test]
fn badmatch_reason_is_a_tagged_tuple_when_the_atom_is_interned() {
    let mut c = ctx();
    let mut module = Module::default();
    let tag = module.atom_table.intern("badmatch");
    let value = make_small(42);
    let reason = badmatch_reason(&mut c, &module, value).unwrap();
    assert_eq!(tuple_arity(reason).unwrap(), 2);
    assert_eq!(tuple_element(reason, 0).unwrap(), make_atom(tag));
    assert_eq!(tuple_element(reason, 1).unwrap(), value);
}

#[test]
fn badmatch_reason_is_a_typed_error_without_the_atom() {
    // `Module::default()` interns nothing, so `badmatch` cannot be
    // materialized: the canonical typed error, never a wrong atom.
    let mut c = ctx();
    let module = Module::default();
    assert!(matches!(
        badmatch_reason(&mut c, &module, make_small(1)),
        Err(BeamError::BadMatch)
    ));
}

// -- floats ---------------------------------------------------------------

#[test]
fn float_round_trips_through_the_ctx() {
    let mut c = ctx();
    let f = make_float(&mut c, 1.5).unwrap();
    assert_eq!(float_value(f).unwrap(), 1.5);
    // A negative zero survives the bit-exact write/read.
    let z = make_float(&mut c, -0.0).unwrap();
    assert!(float_value(z).unwrap().is_sign_negative());
}

#[test]
fn float_value_rejects_a_non_float() {
    assert_bad_arg(float_value(make_small(1)));
    assert_bad_arg(float_value(NIL));
}

#[test]
fn float_layout_is_header_then_bits() {
    let mut c = ctx();
    let f = make_float(&mut c, 2.0).unwrap();
    // SAFETY: `f` is the live 2-word float block the arena owns.
    unsafe {
        assert_eq!(*beam_term::boxed_val(f), beam_term::HEADER_FLONUM);
        assert_eq!(
            (*beam_term::boxed_val(f).add(1)).raw() as u64,
            2.0f64.to_bits()
        );
    }
}

// -- maps -----------------------------------------------------------------

#[test]
fn flatmap_round_trips_through_the_ctx() {
    let mut c = ctx();
    let pairs = [
        (make_small(1), make_small(10)),
        (make_small(2), make_small(20)),
    ];
    let m = make_flatmap(&mut c, &pairs).unwrap();
    assert_eq!(map_entries(m).unwrap(), pairs.to_vec());
}

#[test]
fn empty_flatmap_round_trips() {
    let mut c = ctx();
    let m = make_flatmap(&mut c, &[]).unwrap();
    assert!(map_entries(m).unwrap().is_empty());
}

#[test]
fn flatmap_over_the_small_limit_is_unsupported() {
    let mut c = ctx();
    let pairs: Vec<(Eterm, Eterm)> = (0..MAP_SMALL_MAP_LIMIT + 1)
        .map(|i| (make_small(i as i64), NIL))
        .collect();
    assert!(make_flatmap(&mut c, &pairs).is_err());
}

#[test]
fn map_entries_rejects_a_non_map() {
    assert_bad_arg(map_entries(make_small(1)));
    assert_bad_arg(map_entries(NIL));
}

// -- hashmaps (HAMT read path) --------------------------------------------

/// `MAP_HEADER_TAG_SZ` (`erl_map.h:181-183`) as a shift count.
const MAP_HEADER_TAG_SZ: usize = beam_term::MAP_HEADER_TAG_SZ as usize;
/// `MAP_HEADER_ARITY_SZ` (`erl_map.h:181-183`) as a shift count.
const MAP_HEADER_ARITY_SZ: usize = beam_term::MAP_HEADER_ARITY_SZ as usize;

/// `MAKE_MAP_HEADER(Type, Arity, Val)` (`erl_map.h:181-183`) — the frozen
/// map-node header. Test-only: the engine *reads* a HAMT but never builds one
/// (see [`map_entries`]).
fn map_header(mtype: usize, arity: usize, val: usize) -> Eterm {
    let size = (((val << MAP_HEADER_ARITY_SZ) | arity) << MAP_HEADER_TAG_SZ) | mtype;
    beam_term::make_header(size, beam_term::MAP_SUBTAG)
}

/// A tiny in-test HAMT fixture.
///
/// The rewrite has no HAMT *builder*, so these read-path tests hand-write the
/// exact `erl_map.h` node blocks. Every block lives in a `Box<[Eterm]>` the
/// fixture keeps alive, so a sub-node pointer stored in a parent node stays
/// valid (moving the `Box` moves the pointer, never the pointed-to block).
struct Hamt {
    blocks: Vec<Box<[Eterm]>>,
}

impl Hamt {
    fn new() -> Hamt {
        Hamt { blocks: Vec::new() }
    }

    /// Store `words` and tag the block as a **boxed** term (a map node).
    fn node(&mut self, words: Vec<Eterm>) -> Eterm {
        let block = words.into_boxed_slice();
        let term = beam_term::make_boxed(block.as_ptr());
        self.blocks.push(block);
        term
    }

    /// Store `[K, V]` and tag the block as a **list** term (a leaf cell).
    fn leaf(&mut self, key: Eterm, value: Eterm) -> Eterm {
        let cell = vec![key, value].into_boxed_slice();
        let term = beam_term::make_list(cell.as_ptr());
        self.blocks.push(cell);
        term
    }

    /// `[MAP_HEADER_HAMT_HEAD_BITMAP(bitmap), size, item…]` — a bitmap head.
    fn head_bitmap(&mut self, bitmap: u16, size: usize, items: &[Eterm]) -> Eterm {
        assert_eq!(bitmap.count_ones() as usize, items.len());
        let mut words = vec![
            map_header(
                beam_term::MAP_HEADER_TAG_HAMT_HEAD_BITMAP,
                1,
                usize::from(bitmap),
            ),
            Eterm::from_raw(size),
        ];
        words.extend_from_slice(items);
        self.node(words)
    }

    /// `[MAP_HEADER_HAMT_HEAD_ARRAY, size, item × 16]` — an array head.
    fn head_array(&mut self, size: usize, items: &[Eterm]) -> Eterm {
        assert_eq!(items.len(), HAMT_ARRAY_SLOTS);
        let mut words = vec![
            map_header(beam_term::MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, 0xffff),
            Eterm::from_raw(size),
        ];
        words.extend_from_slice(items);
        self.node(words)
    }

    /// `[MAP_HEADER_HAMT_NODE_BITMAP(bitmap), item…]` — an interior node.
    fn node_bitmap(&mut self, bitmap: u16, items: &[Eterm]) -> Eterm {
        assert_eq!(bitmap.count_ones() as usize, items.len());
        let mut words = vec![map_header(
            beam_term::MAP_HEADER_TAG_HAMT_NODE_BITMAP,
            0,
            usize::from(bitmap),
        )];
        words.extend_from_slice(items);
        self.node(words)
    }

    /// `[make_arityval(n), item…]` — a collision node.
    fn collision(&mut self, items: &[Eterm]) -> Eterm {
        let mut words = vec![beam_term::make_arityval(items.len())];
        words.extend_from_slice(items);
        self.node(words)
    }
}

#[test]
fn a_hand_built_head_node_is_recognised_as_a_hashmap() {
    let mut hamt = Hamt::new();
    let leaf = hamt.leaf(make_small(1), make_small(10));
    let root = hamt.head_bitmap(0b0001, 1, &[leaf]);
    // SAFETY: `root` is the live head node the fixture owns.
    unsafe {
        assert!(beam_term::is_map(root));
        assert!(beam_term::is_hashmap(root));
        assert!(!beam_term::is_flatmap(root));
    }
}

#[test]
fn map_entries_reads_a_bitmap_head() {
    let (k1, k2) = (make_small(1), make_small(2));
    let mut hamt = Hamt::new();
    let first = hamt.leaf(k1, make_small(10));
    let second = hamt.leaf(k2, make_small(20));
    let root = hamt.head_bitmap(0b0011, 2, &[first, second]);
    assert_eq!(
        map_entries(root).unwrap(),
        vec![(k1, make_small(10)), (k2, make_small(20))]
    );
}

#[test]
fn map_entries_descends_into_sub_nodes() {
    // Head slot 0 → a leaf; head slot 1 → a bitmap node holding two leaves.
    let mut hamt = Hamt::new();
    let top = hamt.leaf(make_small(1), make_small(10));
    let inner_first = hamt.leaf(make_small(2), make_small(20));
    let inner_second = hamt.leaf(make_small(3), make_small(30));
    let inner = hamt.node_bitmap(0b0110, &[inner_first, inner_second]);
    let root = hamt.head_bitmap(0b0011, 3, &[top, inner]);
    assert_eq!(
        map_entries(root).unwrap(),
        vec![
            (make_small(1), make_small(10)),
            (make_small(2), make_small(20)),
            (make_small(3), make_small(30)),
        ]
    );
}

#[test]
fn map_entries_reads_an_array_head() {
    let mut hamt = Hamt::new();
    let mut items = Vec::new();
    for i in 0..HAMT_ARRAY_SLOTS as i64 {
        items.push(hamt.leaf(make_small(i), make_small(i + 100)));
    }
    let root = hamt.head_array(HAMT_ARRAY_SLOTS, &items);
    let entries = map_entries(root).unwrap();
    assert_eq!(entries.len(), HAMT_ARRAY_SLOTS);
    for (i, (key, value)) in entries.iter().enumerate() {
        assert_eq!(*key, make_small(i as i64));
        assert_eq!(*value, make_small(i as i64 + 100));
    }
}

#[test]
fn map_entries_reads_a_collision_node() {
    let mut hamt = Hamt::new();
    let first = hamt.leaf(make_small(1), make_small(10));
    let second = hamt.leaf(make_small(2), make_small(20));
    let child = hamt.collision(&[first, second]);
    // A bitmap head whose only occupied slot holds the collision node.
    let root = hamt.head_bitmap(0b0001, 2, &[child]);
    assert_eq!(
        map_entries(root).unwrap(),
        vec![
            (make_small(1), make_small(10)),
            (make_small(2), make_small(20))
        ]
    );
}

#[test]
fn map_entries_rejects_malformed_hamt_nodes() {
    // An array head with `THE_NON_VALUE` in an unoccupied slot: a typed error,
    // never a bogus pair (and never a dereference of the non-value, which
    // `is_boxed` would otherwise accept).
    let mut hamt = Hamt::new();
    let mut items = vec![beam_term::THE_NON_VALUE; HAMT_ARRAY_SLOTS];
    items[0] = hamt.leaf(make_small(1), make_small(10));
    let root = hamt.head_array(1, &items);
    assert_bad_arg(map_entries(root));

    // An item slot holding an immediate is not a HAMT node shape.
    let mut hamt = Hamt::new();
    let root = hamt.head_bitmap(0b0001, 1, &[make_small(7)]);
    assert_bad_arg(map_entries(root));

    // A boxed term whose header is not a map node at all.
    let mut words = [make_small(0)];
    let not_a_node = beam_term::make_boxed(words.as_mut_ptr());
    assert_bad_arg(map_entries(not_a_node));
}
// -- funs -----------------------------------------------------------------

#[test]
fn fun_round_trips_through_the_ctx() {
    let mut c = ctx();
    let free = [make_small(11), make_small(22)];
    let f = make_fun(&mut c, 3, 0xDEAD_BEEF_CAFE, 7, &free).unwrap();
    assert!(is_fun(f));
    assert_eq!(fun_arity(f).unwrap(), 3);
    assert_eq!(fun_num_free(f).unwrap(), 2);
    assert_eq!(fun_uniq(f).unwrap(), 0xDEAD_BEEF_CAFE);
    assert_eq!(fun_index(f).unwrap(), 7);
    assert_eq!(fun_free(f).unwrap(), &free[..]);
}

#[test]
fn fun_without_free_variables_reads_an_empty_env() {
    let mut c = ctx();
    let f = make_fun(&mut c, 0, 1, 2, &[]).unwrap();
    assert!(is_fun(f));
    assert_eq!(fun_arity(f).unwrap(), 0);
    assert_eq!(fun_num_free(f).unwrap(), 0);
    assert_eq!(fun_free(f).unwrap(), &[] as &[Eterm]);
}

#[test]
fn fun_layout_is_header_uniq_index_env() {
    // Raw-word proof of the exact block the widened interpreter relies on.
    let mut words = [Eterm::default(); 3 + 1];
    words[0] = beam_term::fun::make_fun_header(2, 1, beam_term::fun::FunKind::Local).unwrap();
    words[1] = Eterm::from_raw(0x1234_5678_9ABC);
    words[2] = Eterm::from_raw(9);
    words[3] = make_small(42);
    let f = beam_term::make_boxed(words.as_ptr());
    assert!(is_fun(f));
    assert_eq!(fun_arity(f).unwrap(), 2);
    assert_eq!(fun_num_free(f).unwrap(), 1);
    assert_eq!(fun_uniq(f).unwrap(), 0x1234_5678_9ABC);
    assert_eq!(fun_index(f).unwrap(), 9);
    assert_eq!(fun_free(f).unwrap(), &[make_small(42)]);
}

#[test]
fn fun_readers_reject_non_funs() {
    assert!(!is_fun(make_small(1)));
    assert!(!is_fun(NIL));
    assert_bad_arg(fun_arity(make_small(1)));
    assert_bad_arg(fun_num_free(NIL));
    assert_bad_arg(fun_uniq(NIL));
    assert_bad_arg(fun_index(NIL));
    assert_bad_arg(fun_free(NIL));
    // A tuple is boxed but not a fun.
    let mut c = ctx();
    let t = make_tuple(&mut c, &[make_small(1)]).unwrap();
    assert!(!is_fun(t));
    assert_bad_arg(fun_arity(t));
}

#[test]
fn fun_fields_over_255_are_a_system_limit() {
    let mut c = ctx();
    assert!(make_fun(&mut c, 256, 1, 1, &[]).is_err());
    let free = vec![make_small(0); 256];
    assert!(make_fun(&mut c, 1, 1, 1, &free).is_err());
}

#[test]
fn fun_true_arity_reads_the_external_descriptor_over_255() {
    // An external (`EXPORT_EXT`) fun whose true arity exceeds the header's 8-bit
    // field: the header field `fun_arity` (C's `fun_info(F, arity)`) saturates at
    // 255 — oracle-exact, not a bug — while the true arity an encoder must emit
    // lives in descriptor word 3.
    let mut c = ctx();
    let mut atoms = beam_term::AtomTable::new();
    let module = atoms.intern("module");
    let function = atoms.intern("function");
    let hp = c
        .alloc_words(beam_term::fun::fun_words(
            beam_term::fun::FunKind::External,
            0,
        ))
        .unwrap();
    // SAFETY: `hp` addresses exactly `fun_words(External, 0) == 4`
    // exclusively-owned words, which `write_fun_external` writes in full.
    let fun = unsafe { beam_term::fun::write_fun_external(hp, 300, module, function) }.unwrap();
    assert!(is_fun(fun));
    assert_eq!(fun_true_arity(fun).unwrap(), 300);
    assert_eq!(fun_arity(fun).unwrap(), 300 & 0xFF);
    assert_eq!(fun_num_free(fun).unwrap(), 0);
    // Non-funs are rejected, never misread as a fun.
    assert_bad_arg(fun_true_arity(NIL));
    assert_bad_arg(fun_true_arity(make_small(1)));
}

// -- integers ----------------------------------------------------------

#[test]
fn make_integer_keeps_small_values_small() {
    let mut c = ctx();
    for v in [
        0i64,
        1,
        -1,
        MAX_SMALL,
        MAX_SMALL - 1,
        MIN_SMALL,
        MIN_SMALL + 1,
    ] {
        let t = make_integer(&mut c, v).unwrap();
        assert!(beam_term::is_small(t), "{v} must stay a small int");
        assert_eq!(beam_term::signed_value(t), v);
    }
}

#[test]
fn make_integer_builds_an_exact_bignum_outside_the_small_range() {
    let mut c = ctx();
    let cases = [
        (MAX_SMALL + 1, false),
        (MIN_SMALL - 1, true),
        (999_999_999_999_999_999, false),
        (-999_999_999_999_999_999, true),
        (i64::MAX, false),
        (i64::MIN, true),
    ];
    for (v, negative) in cases {
        let t = make_integer(&mut c, v).unwrap();
        assert!(!beam_term::is_small(t), "{v} must not be a small int");
        // SAFETY: `t` is a live bignum just written by `make_integer`.
        let (neg, digits) = unsafe { beam_term::big_parts(t) };
        assert_eq!(neg, negative, "{v}");
        assert_eq!(digits, [v.unsigned_abs()], "{v}");
    }
}

// -- binaries -------------------------------------------------------------

#[test]
fn heap_binary_round_trips_through_the_ctx() {
    let mut c = ctx();
    let bytes = b"hello, beam";
    let b = make_heap_binary(&mut c, bytes).unwrap();
    assert_eq!(binary_bytes(b).unwrap(), &bytes[..]);
}

#[test]
fn empty_heap_binary_round_trips() {
    let mut c = ctx();
    let b = make_heap_binary(&mut c, &[]).unwrap();
    assert_eq!(binary_bytes(b).unwrap(), &[] as &[u8]);
}

#[test]
fn heap_binary_layout_is_header_size_then_data() {
    let mut c = ctx();
    let b = make_heap_binary(&mut c, b"abc").unwrap();
    // SAFETY: `b` is the live heap-binary block the arena owns.
    unsafe {
        let p = beam_term::boxed_val(b);
        // header: block words (2 + 1) minus the header word, heap-bits subtag.
        assert_eq!(
            *p,
            beam_term::make_header(2 + 1 - 1, beam_term::HEAP_BITS_SUBTAG)
        );
        // size is the *bit* count.
        assert_eq!((*p.add(1)).raw(), 3 * 8);
    }
    assert_eq!(binary_bytes(b).unwrap(), &b"abc"[..]);
}

#[test]
fn heap_binary_over_the_limit_is_unsupported() {
    let mut c = ctx();
    let bytes = vec![0u8; ERL_ONHEAP_BINARY_LIMIT + 1];
    assert!(make_heap_binary(&mut c, &bytes).is_err());
}

#[test]
fn binary_bytes_rejects_non_binaries() {
    assert_bad_arg(binary_bytes(make_small(1)));
    assert_bad_arg(binary_bytes(NIL));
}

// -- the node sink (carving a multi-node graph out of ONE reservation) -----

/// `[{1,11},{2,22}]` carved out of a **single** reserved region: every node (the two
/// pair tuples and the two list cells) must come out of that one block, which is what
/// keeps a multi-node build alive under a moving collector.
#[test]
fn slack_carves_a_multi_node_graph_out_of_one_reservation() {
    let mut c = ctx();
    let mut machine = Machine::new();
    // `{1,11}` + `{2,22}` = 3 + 3 words, the list of two = 4 words.
    let mut slack = reserve_slack(&mut machine, &mut c, 3 + 3 + 4).unwrap();

    let pair_a = slack.tuple(&[make_small(1), make_small(11)]).unwrap();
    let pair_b = slack.tuple(&[make_small(2), make_small(22)]).unwrap();
    let list = slack.list(&[pair_a, pair_b]).unwrap();

    // Every word of the reservation was consumed exactly once.
    assert_eq!(slack.remaining(), 0);
    let elems = list_elements(list).unwrap();
    assert_eq!(elems.len(), 2);
    assert_eq!(tuple_element(elems[0], 0).unwrap(), make_small(1));
    assert_eq!(tuple_element(elems[0], 1).unwrap(), make_small(11));
    assert_eq!(tuple_element(elems[1], 0).unwrap(), make_small(2));
    assert_eq!(tuple_element(elems[1], 1).unwrap(), make_small(22));
}

#[test]
fn a_carve_region_too_small_for_a_node_is_a_typed_error() {
    let mut c = ctx();
    let mut machine = Machine::new();
    // Room for two words only: the 3-word `{1,1}` tuple cannot fit.
    let mut slack = reserve_slack(&mut machine, &mut c, 2).unwrap();
    let err = slack.tuple(&[make_small(1), make_small(1)]).unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
    // The failed carve must not have consumed anything.
    assert_eq!(slack.remaining(), 2);
    // A 2-word cons-shaped list is still buildable (`2 * 1` cells).
    let one = slack.list(&[make_small(9)]).unwrap();
    assert_eq!(list_elements(one).unwrap(), vec![make_small(9)]);
    assert_eq!(slack.remaining(), 0);
}

/// The **unrooted** sink: one `alloc_words` per node, sharing the exact layout of the
/// carve writers (an empty list is [`NIL`](beam_term::NIL) and allocates nothing).
#[test]
fn context_sink_builds_each_node_with_alloc_words() {
    let mut c = ctx();
    let mut sink = ContextSink::new(&mut c);

    let t = sink
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .unwrap();
    assert_eq!(tuple_arity(t).unwrap(), 3);
    assert_eq!(tuple_element(t, 2).unwrap(), make_small(3));

    let l = sink.list(&[make_small(4), make_small(5)]).unwrap();
    assert_eq!(list_elements(l).unwrap(), vec![make_small(4), make_small(5)]);
    assert_eq!(sink.list(&[]).unwrap(), NIL);
}