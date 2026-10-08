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

//! Cross-interior correctness parity for both selectable interior families.
//!
//! Test-only module (declared `#[cfg(test)]` in `lib.rs`).
//!
//! # `set`: chained vs CHAMP
//!
//! Two `set` tables — one on the chained [`HashInterior`](crate::hash_interior),
//! one on the CHAMP [`ChampInterior`](crate::champ_interior) — are driven through
//! the *same* operation script and every observable result is compared: `size()` /
//! `ets:info(T, size)`, `lookup`/`member`/`lookup_element`, `insert_new`,
//! `update_counter`/`update_element`, `take`, `delete`/`delete_object`, the
//! `match_object` family and whole-table iteration. The interiors share the
//! `Interior` seam, so this is the guarantee that swapping the `set` index changes
//! no observable behaviour. The same script runs for `keypos = 1` and `keypos = 2`
//! so key-position handling is covered too.
//!
//! # `ordered_set`: B+-tree vs sorted `Vec`
//!
//! The same idea for the two ordered interiors
//! ([`OrderedSetInterior`](crate::OrderedSetInterior) and
//! [`SortedOrderedInterior`](crate::SortedOrderedInterior)), but the tail of the
//! trace is **order-sensitive**: `tab2list`, the whole-object walk and
//! `match_object` must agree not just on *content* but on the exact ascending
//! **arithmetic** key order (`beam_term::compare::cmp_arith`, the order ERTS
//! `ordered_set` uses), along with `first`/`last`/`next`/`prev`.

use crate::kind::{OrderedInterior, SetInterior, TableKind, TableOptions};
use crate::table::{Table, TableId};
use beam_common::BeamError;
use beam_term::heap::Heap;
use beam_term::prelude::*;

/// A `{Key, Value}` object tuple on `heap` (keypos 1).
fn obj(heap: &mut Heap, key: i64, value: i64) -> Eterm {
    heap.tuple(&[make_small(key), make_small(value)]).unwrap()
}

/// A `{Value, Key}` object tuple on `heap` (keypos 2).
fn obj_kp2(heap: &mut Heap, key: i64, value: i64) -> Eterm {
    heap.tuple(&[make_small(value), make_small(key)]).unwrap()
}

/// A canonical, comparable rendering of a term: small ints, nil and (nested)
/// tuples thereof. The scripts only ever build these, so any other shape is a
/// test bug and rendered as a `?raw` marker (which would then differ between the
/// two traces).
fn render(term: Eterm) -> String {
    if term == NIL {
        return "nil".to_string();
    }
    if is_small(term) {
        return signed_value(term).to_string();
    }
    // SAFETY: the scripts only ever build tuples of small ints; a non-tuple falls
    // through to the `?raw` marker below.
    if unsafe { is_tuple(term) } {
        // SAFETY: `is_tuple` just held, so the header/arity read is well defined.
        let n = unsafe { tuple_size(term) };
        let mut parts = Vec::with_capacity(n);
        for i in 0..n {
            // SAFETY: `i < n == tuple_size(term)`.
            parts.push(render(unsafe { tuple_element(term, i) }));
        }
        return format!("{{{}}}", parts.join(","));
    }
    format!("?raw:{:x}", term.raw())
}

/// A rendering of a list of terms as an order-insensitive set.
fn render_set(terms: &[Eterm]) -> String {
    let mut rendered: Vec<String> = terms.iter().map(|t| render(*t)).collect();
    rendered.sort();
    rendered.dedup();
    rendered.join("|")
}

/// A rendering of a fallible scalar result.
fn render_result(result: Result<Eterm, BeamError>) -> String {
    match result {
        Ok(term) => format!("ok:{}", render(term)),
        Err(err) => format!("err:{err:?}"),
    }
}

/// A `[HEADER_FLONUM, bits]` float term, leaked for the test's life (neither the
/// `beam_term::heap::Heap` arena nor this module has a boxed-float constructor).
fn float_term(value: f64) -> Eterm {
    let words: Box<[Eterm; 2]> =
        Box::new([HEADER_FLONUM, Eterm::from_raw(value.to_bits() as usize)]);
    let leaked: &'static [Eterm; 2] = Box::leak(words);
    make_boxed(leaked.as_ptr())
}

/// A rendering of one **number** term: `i<v>` for a small integer and `f<v>` for
/// a float, so an integer/float mixed run can be compared without exposing a
/// float's pointer identity (which [`render`] would, via its `?raw` fallback).
fn render_number(term: Eterm) -> String {
    if is_small(term) {
        return format!("i{}", signed_value(term));
    }
    // SAFETY: the numeric mixed-order script only ever builds small ints and
    // floats, so every non-small term here is a live boxed flonum.
    let bits = unsafe { float_bits(term) };
    format!("f{}", f64::from_bits(bits))
}
/// One step of the operation script.
#[derive(Clone, Copy)]
enum Op {
    Insert(i64, i64),
    InsertNew(i64, i64),
    Delete(i64),
    DeleteObject(i64, i64),
    UpdateCounter(i64, i64),
    UpdateElement(i64, i64),
    Take(i64),
    Lookup(i64),
    LookupElement(i64, usize),
    Member(i64),
}

/// A deterministic operation script (an LCG, so the two tables see the identical
/// sequence without any RNG dependency). Keys/values come from a small pool, so
/// the script exercises replaces, deletes of absent keys and repeated counters.
fn script(len: usize) -> Vec<Op> {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as i64
    };
    let mut ops = Vec::with_capacity(len);
    for _ in 0..len {
        let key = next().rem_euclid(64);
        let value = next().rem_euclid(1000);
        match next().rem_euclid(10) {
            0 | 1 | 2 => ops.push(Op::Insert(key, value)),
            3 => ops.push(Op::InsertNew(key, value)),
            4 => ops.push(Op::Delete(key)),
            5 => ops.push(Op::DeleteObject(key, value.rem_euclid(4))),
            6 => ops.push(Op::UpdateCounter(key, value.rem_euclid(7) - 3)),
            7 => ops.push(Op::UpdateElement(key, value)),
            8 => ops.push(Op::Take(key)),
            _ => match next().rem_euclid(3) {
                0 => ops.push(Op::Lookup(key)),
                1 => ops.push(Op::LookupElement(key, 1 + (value.rem_euclid(2)) as usize)),
                _ => ops.push(Op::Member(key)),
            },
        }
    }
    ops
}

/// Drive the shared operation script against one table, returning the per-step
/// trace common to both interior families. The heap backing the freshly-built
/// objects is owned here, so the caller only supplies the table.
fn drive_script(table: &mut Table, keypos: usize, atoms: &AtomTable) -> Vec<String> {
    let mut heap = Heap::new(1 << 20);
    let build = |heap: &mut Heap, key: i64, value: i64| {
        if keypos == 1 {
            obj(heap, key, value)
        } else {
            obj_kp2(heap, key, value)
        }
    };
    // The element update always targets the *value* slot, never the key.
    let value_pos = if keypos == 1 { 2 } else { 1 };
    // `{Pos, Incr}` counter operand positions likewise.
    let counter_pos = value_pos as i64;

    let mut trace = Vec::new();
    for op in script(400) {
        match op {
            Op::Insert(key, value) => {
                let object = build(&mut heap, key, value);
                trace.push(format!("insert {key} {value} {:?}", table.insert(object, atoms)));
            }
            Op::InsertNew(key, value) => {
                let object = build(&mut heap, key, value);
                trace.push(format!(
                    "insert_new {key} {value} {:?}",
                    table.insert_new(object, atoms)
                ));
            }
            Op::Delete(key) => trace.push(format!(
                "delete {key} {:?}",
                table.delete(make_small(key), atoms)
            )),
            Op::DeleteObject(key, value) => {
                let object = build(&mut heap, key, value);
                trace.push(format!(
                    "delete_object {key} {value} {:?}",
                    table.delete_object(object, atoms)
                ));
            }
            Op::UpdateCounter(key, incr) => {
                let mut out = beam_alloc::Heap::new(256).expect("a test heap is allocatable");
                let op_term = heap
                    .tuple(&[make_small(counter_pos), make_small(incr)])
                    .unwrap();
                trace.push(format!(
                    "update_counter {key} {incr} {}",
                    render_result(table.update_counter(make_small(key), op_term, atoms, &mut out))
                ));
            }
            Op::UpdateElement(key, value) => {
                let mut out = beam_alloc::Heap::new(256).expect("a test heap is allocatable");
                let updates = [(value_pos, make_small(value))];
                trace.push(format!(
                    "update_element {key} {value} {:?}",
                    table.update_element(make_small(key), &updates, atoms, &mut out)
                ));
            }
            Op::Take(key) => {
                let taken = table.take(make_small(key), atoms).unwrap_or_default();
                trace.push(format!("take {key} {}", render_set(&taken)));
            }
            Op::Lookup(key) => {
                let found = table.lookup(make_small(key), atoms);
                trace.push(format!("lookup {key} {}", render_set(&found)));
            }
            Op::LookupElement(key, pos) => trace.push(format!(
                "lookup_element {key} {pos} {}",
                render_result(table.lookup_element(make_small(key), pos, atoms))
            )),
            Op::Member(key) => trace.push(format!(
                "member {key} {}",
                table.member(make_small(key), atoms)
            )),
        }
        // The size is compared after every step, both spellings.
        trace.push(format!("size {}", table.size()));
        trace.push(format!("info_size {}", table.info().size));
    }
    trace
}

/// Apply the script to one `set` table and return its observable trace
/// (whole-table iteration compared order-insensitively).
fn run_script(set_interior: SetInterior, keypos: usize, atoms: &AtomTable, wildcard: Eterm) -> Vec<String> {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::Set);
    opts.set_keypos(keypos).unwrap();
    let mut table =
        Table::with_set_interior(TableId::next(), opts, set_interior).expect("table is buildable");
    assert_eq!(table.set_interior(), set_interior);

    let mut trace = drive_script(&mut table, keypos, atoms);

    // Whole-table iteration content (order is unspecified for a hash kind).
    trace.push(format!("tab2list {}", render_set(&table.tab2list())));

    // The `match_object` family over the final state: every stored object matches
    // the `{'_','_'}` pattern.
    trace.push(format!("match_object {}", render_set(&match_all(&table, atoms, wildcard))));

    trace
}

/// Apply the script to one `ordered_set` table and return its observable trace.
///
/// Unlike the `set` trace the tail is **order-sensitive**: `tab2list`, the
/// whole-object walk, the `next`/`prev` chains and `match_object` must all
/// reproduce the exact ascending `beam_term::compare` order, so any ordering
/// divergence between the two ordered interiors is caught.
fn run_ordered_script(
    ordered_interior: OrderedInterior,
    keypos: usize,
    atoms: &AtomTable,
    wildcard: Eterm,
) -> Vec<String> {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    opts.set_keypos(keypos).unwrap();
    let mut table = Table::with_ordered_interior(TableId::next(), opts, ordered_interior)
        .expect("table is buildable");
    assert_eq!(table.ordered_interior(), ordered_interior);

    let mut trace = drive_script(&mut table, keypos, atoms);

    // Ordered content *and order*: `tab2list` is the ascending object run.
    trace.push(format!("tab2list {}", render_ordered(&table.tab2list())));
    trace.push(format!("first {}", render_opt(table.first(atoms))));
    trace.push(format!("last {}", render_opt(table.last(atoms))));

    // The `next`-chain (present keys) reproduces the ascending run …
    let mut walk = Vec::new();
    let mut cursor = table.first(atoms);
    while let Some(key) = cursor {
        walk.push(key);
        cursor = table.next(key, atoms).unwrap();
    }
    trace.push(format!("walk {}", render_ordered(&walk)));

    // … and the `prev`-chain is its reverse.
    let mut back = Vec::new();
    let mut cursor = table.last(atoms);
    while let Some(key) = cursor {
        back.push(key);
        cursor = table.prev(key, atoms).unwrap();
    }
    back.reverse();
    trace.push(format!("back_walk {}", render_ordered(&back)));

    // The `match_object` family over the final state, in the same ordered order.
    trace.push(format!("match_object {}", render_ordered(&match_all(&table, atoms, wildcard))));

    trace
}

/// Every stored object, matched (and returned in the table's own iteration order)
/// through the `{'_','_'}` pattern.
fn match_all(table: &Table, atoms: &AtomTable, wildcard: Eterm) -> Vec<Eterm> {
    let mut heap = Heap::new(1 << 20);
    let mut out = beam_alloc::Heap::new(1 << 16).expect("a test heap is allocatable");
    let pattern = heap.tuple(&[wildcard, wildcard]).unwrap();
    table.match_object(pattern, atoms, &mut out).unwrap()
}

/// A rendering of a list of terms that preserves order (the ordered trace needs
/// the exact ascending run, not just the content).
fn render_ordered(terms: &[Eterm]) -> String {
    terms
        .iter()
        .map(|term| render(*term))
        .collect::<Vec<_>>()
        .join("|")
}

/// A rendering of an optional term (`end` for an empty order).
fn render_opt(term: Option<Eterm>) -> String {
    match term {
        Some(term) => render(term),
        None => "end".to_string(),
    }
}

#[test]
fn champ_is_the_documented_default_set_interior() {
    // The docs (crate `CONTEXT.md`, `SetInterior`, `Table::new`) claim CHAMP is the
    // `set` default; pin it so a regression in the default is caught here.
    assert_eq!(SetInterior::default(), SetInterior::Champ);
    assert_eq!(SetInterior::default().as_str(), "champ");

    let table = Table::new(TableId::next(), TableOptions::new()).expect("table is buildable");
    assert_eq!(table.set_interior(), SetInterior::Champ);
    // An explicit selection is honoured.
    let chained =
        Table::with_set_interior(TableId::next(), TableOptions::new(), SetInterior::Chained)
            .expect("table is buildable");
    assert_eq!(chained.set_interior(), SetInterior::Chained);
    assert_ne!(SetInterior::Chained, SetInterior::Champ);
}

#[test]
fn chained_and_champ_agree_on_the_operation_script_keypos1() {
    let mut atoms = AtomTable::new();
    let wildcard = make_atom(atoms.intern("_"));
    let chained = run_script(SetInterior::Chained, 1, &atoms, wildcard);
    let champ = run_script(SetInterior::Champ, 1, &atoms, wildcard);
    compare(&chained, &champ);
}

#[test]
fn chained_and_champ_agree_on_the_operation_script_keypos2() {
    let mut atoms = AtomTable::new();
    let wildcard = make_atom(atoms.intern("_"));
    let chained = run_script(SetInterior::Chained, 2, &atoms, wildcard);
    let champ = run_script(SetInterior::Champ, 2, &atoms, wildcard);
    compare(&chained, &champ);
}

#[test]
fn bplus_tree_is_the_documented_default_ordered_interior() {
    // The docs (crate `CONTEXT.md`, `OrderedInterior`, `Table::new`) claim the
    // B+-tree is the `ordered_set` default; pin it so a regression in the default
    // is caught here.
    assert_eq!(OrderedInterior::default(), OrderedInterior::BPlusTree);
    assert_eq!(OrderedInterior::default().as_str(), "bplus_tree");
    assert_eq!(OrderedInterior::Sorted.as_str(), "sorted");
    assert_ne!(OrderedInterior::BPlusTree, OrderedInterior::Sorted);

    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);

    let table = Table::new(TableId::next(), opts.clone()).expect("table is buildable");
    assert_eq!(table.ordered_interior(), OrderedInterior::BPlusTree);

    // An explicit selection is honoured.
    let sorted = Table::with_ordered_interior(TableId::next(), opts, OrderedInterior::Sorted)
        .expect("table is buildable");
    assert_eq!(sorted.ordered_interior(), OrderedInterior::Sorted);
}

#[test]
fn ordered_interiors_agree_on_the_operation_script_keypos1() {
    let mut atoms = AtomTable::new();
    let wildcard = make_atom(atoms.intern("_"));
    let tree = run_ordered_script(OrderedInterior::BPlusTree, 1, &atoms, wildcard);
    let sorted = run_ordered_script(OrderedInterior::Sorted, 1, &atoms, wildcard);
    compare(&tree, &sorted);
}

#[test]
fn ordered_interiors_agree_on_the_operation_script_keypos2() {
    let mut atoms = AtomTable::new();
    let wildcard = make_atom(atoms.intern("_"));
    let tree = run_ordered_script(OrderedInterior::BPlusTree, 2, &atoms, wildcard);
    let sorted = run_ordered_script(OrderedInterior::Sorted, 2, &atoms, wildcard);
    compare(&tree, &sorted);
}

/// The ordered tail really is order-sensitive: a table that stored the same keys
/// in a different order would produce a different `tab2list`/`walk` line, so the
/// parity test above could not pass by content alone. This pins that property.
#[test]
fn the_ordered_trace_pins_the_ascending_order() {
    let mut atoms = AtomTable::new();
    let wildcard = make_atom(atoms.intern("_"));
    let tree = run_ordered_script(OrderedInterior::BPlusTree, 1, &atoms, wildcard);
    let sorted = run_ordered_script(OrderedInterior::Sorted, 1, &atoms, wildcard);

    let tab2list_of = |trace: &[String]| {
        trace
            .iter()
            .find(|line| line.starts_with("tab2list "))
            .cloned()
            .expect("the trace records tab2list")
    };
    // The two ordered interiors produce the *same* ordered rendering ...
    assert_eq!(tab2list_of(&tree), tab2list_of(&sorted));

    // ... and that rendering is not the trivially sorted content: a reversed run
    // would differ, so the line carries order information.
    let ordered = tab2list_of(&tree)
        .strip_prefix("tab2list ")
        .unwrap()
        .to_string();
    let fields: Vec<&str> = ordered.split('|').collect();
    assert!(
        fields.len() >= 2,
        "the script leaves at least two objects to order"
    );
    let mut reversed = fields.clone();
    reversed.reverse();
    let reversed = reversed.join("|");
    assert_ne!(ordered, reversed, "the ordered table has at least two objects");
}

/// Insert the numeric mixed-key run into one ordered interior and render the
/// resulting ascending arithmetic key order by walking `tab2list` (the ordered
/// object run) and rendering each object's key element.
fn numeric_order_run(interior: OrderedInterior, atoms: &AtomTable) -> String {
    let mut heap = Heap::new(256);
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    let mut table =
        Table::with_ordered_interior(TableId::next(), opts, interior).expect("table is buildable");
    // `1` then `1.0` collapse to a single key whose stored object is the *last* one,
    // so its key element renders as the float `1.0`.
    for key in [
        make_small(5),
        make_small(-3),
        make_small(0),
        float_term(1.5),
        float_term(-2.5),
        make_small(1),
        float_term(1.0),
    ] {
        table
            .insert(heap.tuple(&[key, key]).unwrap(), atoms)
            .unwrap();
    }
    assert_eq!(table.size(), 6, "the `1`/`1.0` pair is one key");
    table
        .tab2list()
        .iter()
        .map(|obj| {
            // SAFETY: the script only builds 2-tuples.
            render_number(unsafe { tuple_element(*obj, 0) })
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// The `cmp` -> `cmp_arith` change must leave the two ordered interiors in exact
/// agreement on the **arithmetic key order**: `{1, a}` and `{1.0, b}` collapse to
/// one key (the last object wins), and a numeric mixed-key set iterates in value
/// order across integer/float representations (`-3 < -2.5 < 0 < 1.0 < 1.5 < 5`).
/// This pins the two ordered interiors against each other on the new order.
#[test]
fn ordered_interiors_agree_on_arithmetic_key_order() {
    let atoms = AtomTable::new();

    // Both interiors collapse the `1`/`1.0` pair to a single key.
    for interior in [OrderedInterior::BPlusTree, OrderedInterior::Sorted] {
        let mut heap = Heap::new(64);
        let mut opts = TableOptions::new();
        opts.set_kind(TableKind::OrderedSet);
        let mut table =
            Table::with_ordered_interior(TableId::next(), opts, interior).expect("buildable");
        table
            .insert(heap.tuple(&[make_small(1), make_small(10)]).unwrap(), &atoms)
            .unwrap();
        table
            .insert(heap.tuple(&[float_term(1.0), make_small(20)]).unwrap(), &atoms)
            .unwrap();
        assert_eq!(table.size(), 1, "{interior:?}: 1 and 1.0 are the same key");
        assert!(table.member(make_small(1), &atoms));
        assert!(table.member(float_term(1.0), &atoms));
        // Either representation finds the last-inserted object.
        let want = heap.tuple(&[float_term(1.0), make_small(20)]).unwrap();
        let found = table.lookup(make_small(1), &atoms);
        assert!(eq(found[0], want, &atoms), "{interior:?}");
    }

    // The numeric mixed order renders identically, and in value order.
    let tree = numeric_order_run(OrderedInterior::BPlusTree, &atoms);
    let sorted = numeric_order_run(OrderedInterior::Sorted, &atoms);
    assert_eq!(tree, sorted, "both ordered interiors share the key order");
    assert_eq!(tree, "i-3|f-2.5|i0|f1|f1.5|i5");
}

/// Render one **key** term returned by the ordered navigation, as `i<v>`/`f<v>`
/// ([`render_number`]) or `end` for `'$end_of_table'`. Unlike [`render`] this
/// distinguishes an integer `1` from a float `1.0`, which is exactly the
/// divergence the replaced-key-term rule is about.
fn render_key(term: Option<Eterm>) -> String {
    match term {
        Some(term) => render_number(term),
        None => "end".to_string(),
    }
}

/// The collapsed-key scenario the oracle pins on the returned **key terms**:
/// `{1, a}` then `{1.0, b}` (or the reverse order) leaves one key, and
/// `ets:first`/`ets:last`/`ets:next`/`ets:prev` report the *last-inserted* key
/// term — the key follows the stored object. Probed with the system `erl`
/// (OTP 29 / erts-17.1): `{1,a}` then `{1.0,b}` gives `first = 1.0`,
/// `next(first) = '$end_of_table'`; the reverse order gives `first = 1`.
fn collapsed_key_run(interior: OrderedInterior, int_first: bool, atoms: &AtomTable) -> String {
    let mut heap = Heap::new(64);
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    let mut table =
        Table::with_ordered_interior(TableId::next(), opts, interior).expect("buildable");
    let (first, second) = if int_first {
        (make_small(1), float_term(1.0))
    } else {
        (float_term(1.0), make_small(1))
    };
    table
        .insert(heap.tuple(&[first, make_small(10)]).unwrap(), atoms)
        .unwrap();
    table
        .insert(heap.tuple(&[second, make_small(20)]).unwrap(), atoms)
        .unwrap();
    assert_eq!(table.size(), 1, "{interior:?}: 1 and 1.0 are one key");
    let first_key = table.first(atoms);
    let last = table.last(atoms);
    let next = first_key.and_then(|key| table.next(key, atoms).unwrap());
    let prev = last.and_then(|key| table.prev(key, atoms).unwrap());
    format!(
        "size={} first={} last={} next={} prev={}",
        table.size(),
        render_key(first_key),
        render_key(last),
        render_key(next),
        render_key(prev),
    )
}

/// A same-arith-key replace must update the stored **key term**, not just the
/// object: `1` and `1.0` collapse to one key whose term is the *last-inserted* one
/// (the oracle's `ets:first`/`last`/`next`/`prev` all report it). Both ordered
/// interiors must agree on every returned key term — the B+-tree through its
/// replaced leaf entry, the sorted `Vec` through its replaced entry.
#[test]
fn ordered_interiors_agree_on_the_replaced_key_term() {
    let atoms = AtomTable::new();
    for int_first in [true, false] {
        let tree = collapsed_key_run(OrderedInterior::BPlusTree, int_first, &atoms);
        let sorted = collapsed_key_run(OrderedInterior::Sorted, int_first, &atoms);
        assert_eq!(
            tree, sorted,
            "the two ordered interiors share the stored key term"
        );
    }

    // …and the oracle's exact rendering, per insert order.
    assert_eq!(
        collapsed_key_run(OrderedInterior::BPlusTree, true, &atoms),
        "size=1 first=f1 last=f1 next=end prev=end"
    );
    assert_eq!(
        collapsed_key_run(OrderedInterior::Sorted, true, &atoms),
        "size=1 first=f1 last=f1 next=end prev=end"
    );
    assert_eq!(
        collapsed_key_run(OrderedInterior::BPlusTree, false, &atoms),
        "size=1 first=i1 last=i1 next=end prev=end"
    );
    assert_eq!(
        collapsed_key_run(OrderedInterior::Sorted, false, &atoms),
        "size=1 first=i1 last=i1 next=end prev=end"
    );
}

/// Assert two traces are identical, printing the first mismatch.
fn compare(chained: &[String], champ: &[String]) {
    assert_eq!(
        chained.len(),
        champ.len(),
        "the two interiors produced different trace lengths"
    );
    for (i, (a, b)) in chained.iter().zip(champ.iter()).enumerate() {
        assert_eq!(a, b, "trace diverged at step {i}");
    }
}

#[test]
fn interiors_agree_on_membership_and_iteration_content() {
    use crate::champ_interior::ChampInterior;
    use crate::hash_interior::HashInterior;
    use crate::table::Interior;

    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 18);
    let mut chained: Box<dyn Interior> = Box::new(HashInterior::new(TableKind::Set));
    let mut champ: Box<dyn Interior> = Box::new(ChampInterior::new());

    const N: i64 = 1500;
    let objects: Vec<Eterm> = (0..N).map(|i| obj(&mut heap, i, i * 3)).collect();
    for i in 0..N as usize {
        let key = make_small(i as i64);
        chained.insert(key, objects[i], &atoms).unwrap();
        champ.insert(key, objects[i], &atoms).unwrap();
    }
    assert_eq!(chained.size(), champ.size());

    // Every key is found, with the same object, in both.
    for i in 0..N as usize {
        let key = make_small(i as i64);
        assert_eq!(
            chained.lookup(key, &atoms),
            champ.lookup(key, &atoms),
            "key {i}"
        );
        assert!(champ.member(key, &atoms));
    }
    // Absent keys agree.
    for i in N..N + 50 {
        let key = make_small(i);
        assert!(chained.lookup(key, &atoms).is_empty());
        assert!(champ.lookup(key, &atoms).is_empty());
    }

    // Iteration content (order is unspecified) covers the same keys.
    let mut ck = chained.keys();
    let mut hk = champ.keys();
    ck.sort_by(|a, b| cmp(*a, *b, &atoms));
    hk.sort_by(|a, b| cmp(*a, *b, &atoms));
    assert_eq!(ck, hk);
    assert_eq!(chained.iter_objects().count(), champ.iter_objects().count());

    // Remove every other key and re-check.
    for i in (0..N).step_by(2) {
        let key = make_small(i);
        chained.delete(key, &atoms).unwrap();
        champ.delete(key, &atoms).unwrap();
    }
    assert_eq!(chained.size(), champ.size());
    let mut ck = chained.keys();
    let mut hk = champ.keys();
    ck.sort_by(|a, b| cmp(*a, *b, &atoms));
    hk.sort_by(|a, b| cmp(*a, *b, &atoms));
    assert_eq!(ck, hk);
}
