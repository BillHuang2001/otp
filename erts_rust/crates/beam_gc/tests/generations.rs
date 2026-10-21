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

//! Generation-cycle evidence for the [`beam_gc`] [`Collector`].
//!
//! These tests drive a real `Collector` through the public API (`new`/`collect`/
//! `collect_minor`/`collect_major`, the accessors and the accumulated `stats`) the way
//! `beam_process` would, and assert **observable** behaviour: the chosen
//! [`CollectionKind`], the counters, `contains_term`, nursery emptiness and structural
//! preservation of the copied graph. The Cheney copy engine itself is covered by
//! `tests/copy_correctness.rs`; here we exercise the two-generation driving logic.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or
//! that the copier just wrote); every block carries a `// SAFETY:` rationale.

use beam_gc::prelude::{CollectionKind, Collector, HeapPolicy};
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, is_atom, is_list, list_val, make_atom, make_small,
    signed_value, Atom, Eterm, NIL,
};

/// Walks a proper list of small integers, returning them in order.
fn list_to_smalls(mut term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    while is_list(term) {
        // SAFETY: `term` is a live cons cell (established by `is_list`), so both its car
        // and cdr words are in-bounds and initialised.
        let cell = list_val(term);
        values.push(signed_value(unsafe { *cell }));
        term = unsafe { *cell.add(1) };
    }
    assert_eq!(term, NIL, "the list must be proper");
    values
}

/// Builds the live graph `{atom, [1,2,3], {4,5}}` in the nursery, returning the outer
/// tuple term and the atom leaf (an immediate, copied verbatim) for later comparison.
fn build_graph(collector: &mut Collector) -> (Eterm, Eterm) {
    let heap = collector.young_mut().heap_mut();
    let atom = make_atom(Atom::from_raw(7));
    let cell3 = heap.cons(make_small(3), NIL).expect("cons cell");
    let cell2 = heap.cons(make_small(2), cell3).expect("cons cell");
    let list = heap.cons(make_small(1), cell2).expect("cons cell");
    let inner = heap
        .tuple(&[make_small(4), make_small(5)])
        .expect("inner tuple");
    let outer = heap.tuple(&[atom, list, inner]).expect("outer tuple");
    (outer, atom)
}

/// Allocates a one-element `{value}` tuple in the nursery.
fn build_leaf_tuple(collector: &mut Collector, value: Eterm) -> Eterm {
    collector
        .young_mut()
        .heap_mut()
        .tuple(&[value])
        .expect("leaf tuple")
}

/// Asserts the graph reachable from `root` — now resident in the mature space — has the
/// shape and leaf values of the original `{atom, [1,2,3], {4,5}}`.
fn assert_graph_preserved(collector: &Collector, root: Eterm, atom: Eterm) {
    assert!(
        collector.mature().contains_term(root),
        "the root must be resident in the mature space"
    );
    assert!(
        !collector.young().contains_term(root),
        "the root must no longer be in the nursery"
    );

    // SAFETY: `root` is a live 3-element tuple in the mature space.
    let header = unsafe { *boxed_val(root) };
    assert!(is_arity_value(header));
    assert_eq!(arityval(header), 3);

    // SAFETY: elements 0..3 lie within the 3-element tuple just validated.
    let e0 = unsafe { *boxed_val(root).add(1) };
    let e1 = unsafe { *boxed_val(root).add(2) };
    let e2 = unsafe { *boxed_val(root).add(3) };

    // element 0: the atom leaf, copied verbatim (an immediate is never moved).
    assert!(is_atom(e0));
    assert_eq!(e0, atom, "the atom leaf is unchanged");

    // element 1: the [1,2,3] list, intact in the mature space.
    assert!(is_list(e1));
    assert!(
        collector.mature().contains_term(e1),
        "the list must be resident in the mature space"
    );
    assert_eq!(list_to_smalls(e1), vec![1, 2, 3]);

    // element 2: the inner {4,5} tuple, copied as a distinct mature object.
    assert!(
        collector.mature().contains_term(e2),
        "the inner tuple must be resident in the mature space"
    );
    // SAFETY: `e2` is a live 2-element tuple in the mature space.
    let inner_header = unsafe { *boxed_val(e2) };
    assert!(is_arity_value(inner_header));
    assert_eq!(arityval(inner_header), 2);
    // SAFETY: elements 0..2 lie within that tuple.
    assert_eq!(signed_value(unsafe { *boxed_val(e2).add(1) }), 4);
    assert_eq!(signed_value(unsafe { *boxed_val(e2).add(2) }), 5);
}

#[test]
fn minor_collection_promotes_live_terms_and_empties_the_nursery() {
    let mut collector = Collector::new(HeapPolicy::default()).expect("collector");
    let (outer, atom) = build_graph(&mut collector);

    // The policy is observed through `should_collect`: an allocation exactly filling the
    // remaining words fits, one word more does not.
    let available = collector.young().available_words();
    assert!(
        !collector.should_collect(available),
        "an exact fit still fits"
    );
    assert!(
        collector.should_collect(available + 1),
        "one word past the capacity must collect"
    );

    // 4 (outer) + 6 (three cons cells) + 3 (inner) words in the nursery.
    assert_eq!(collector.young().used_words(), 13);

    let mut roots = [outer];
    let collection = collector.collect(&mut roots).expect("collect");

    // A well-fitting minor: the mature space holds the whole nursery, so no full sweep.
    assert_eq!(collection.kind, CollectionKind::Minor);
    assert_eq!(collector.young().used_words(), 0, "the nursery is emptied");
    assert_eq!(
        collection.stats.words_copied, 13,
        "only the live words move"
    );
    assert_eq!(
        collection.stats.objects_copied, 5,
        "outer + inner + 3 cells"
    );
    assert_eq!(
        collection.stats.words_reclaimed, 0,
        "the nursery held no garbage"
    );

    // The accumulated counters record exactly one minor and no major.
    assert_eq!(collector.stats().minor_collections, 1);
    assert_eq!(collector.stats().major_collections, 0);
    assert_eq!(collector.stats().words_copied, 13);

    // The graph survives, now reachable through the updated root in the mature space.
    assert_graph_preserved(&collector, roots[0], atom);
}

#[test]
fn fullsweep_after_forces_a_major_on_the_next_collection() {
    let policy = HeapPolicy {
        fullsweep_after: 1,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");

    let (first_term, atom) = build_graph(&mut collector);
    let mut roots = [first_term];
    let first = collector.collect(&mut roots).expect("first collect");
    assert_eq!(
        first.kind,
        CollectionKind::Minor,
        "the first collection is still a minor"
    );
    assert_eq!(collector.stats().minor_collections, 1);
    assert_eq!(collector.stats().major_collections, 0);

    // The nursery was reset; allocate again and keep the promoted root live too.
    let extra = build_leaf_tuple(&mut collector, make_small(9));
    let mut roots = [roots[0], extra];
    let second = collector.collect(&mut roots).expect("second collect");

    assert_eq!(
        second.kind,
        CollectionKind::Major,
        "fullsweep_after = 1 forces a fullsweep after one minor"
    );
    assert_eq!(collector.stats().major_collections, 1);
    assert_eq!(collector.stats().minor_collections, 1, "no extra minor ran");
    assert_eq!(collector.young().used_words(), 0);

    // Everything live is now in the mature space.
    for root in roots {
        assert!(
            collector.mature().contains_term(root),
            "all live data is resident in the mature space"
        );
    }
    assert_graph_preserved(&collector, roots[0], atom);

    // The freshly-allocated young tuple survived the fullsweep as well.
    let extra_root = roots[1];
    // SAFETY: `extra_root` is a live 1-element tuple in the mature space.
    let extra_header = unsafe { *boxed_val(extra_root) };
    assert!(is_arity_value(extra_header));
    assert_eq!(arityval(extra_header), 1);
    // SAFETY: element 0 lies within that tuple.
    assert_eq!(signed_value(unsafe { *boxed_val(extra_root).add(1) }), 9);
}

#[test]
fn a_minor_that_would_not_fit_escalates_to_a_major() {
    // A mature space far smaller than the nursery cannot receive a whole minor, so a
    // plain `collect` must escalate to a fullsweep (no partial-copy rollback).
    let policy = HeapPolicy {
        initial_mature_words: 12,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");

    let (outer, atom) = build_graph(&mut collector);
    let young_used = collector.young().used_words();
    assert_eq!(young_used, 13);
    assert!(
        collector.mature().available_words() < young_used,
        "the nursery no longer fits in the mature space"
    );

    let mut roots = [outer];
    let collection = collector.collect(&mut roots).expect("collect");

    assert_eq!(
        collection.kind,
        CollectionKind::Major,
        "a would-not-fit minor escalates to a fullsweep"
    );
    assert_eq!(collector.stats().major_collections, 1);
    assert_eq!(collector.stats().minor_collections, 0, "no minor ran");
    assert_eq!(collector.young().used_words(), 0);

    // The fullsweep sized a fresh mature space to hold the whole live set.
    assert!(
        collector.mature().capacity_words() > 12,
        "the mature space was resized up"
    );
    assert!(collector.mature().contains_term(roots[0]));
    assert_graph_preserved(&collector, roots[0], atom);
}
