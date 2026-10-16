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

//! End-to-end tests of the integrated path: a term graph built through
//! [`GenImmixCollector::young_mut`], driven through several minors and a major, must
//! stay structurally intact with every root relocated into the mature Immix space.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or
//! that a collection just wrote); every block carries a `// SAFETY:` rationale.

use beam_common::BeamError;
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, is_atom, is_list, list_val, make_atom, make_small,
    signed_value, Atom, Eterm, NIL,
};

use super::*;

/// Walks a proper list of small integers, returning them in order.
fn list_to_smalls(mut term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    while is_list(term) {
        // SAFETY: `term` is a live cons cell (`is_list`), so both words are readable.
        let cell = list_val(term);
        values.push(signed_value(unsafe { *cell }));
        term = unsafe { *cell.add(1) };
    }
    assert_eq!(term, NIL, "the list must be proper");
    values
}

/// Builds `{atom, [1,2,3], {4,5}}` in the nursery; returns the outer term and the atom.
fn build_graph(c: &mut GenImmixCollector) -> (Eterm, Eterm) {
    let heap = c.young_mut().heap_mut();
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

/// Asserts the graph reachable from `root` — resident in the mature space — has the
/// shape and leaf values of `{atom, [1,2,3], {4,5}}`.
fn assert_graph_preserved(c: &GenImmixCollector, root: Eterm, atom: Eterm) {
    assert!(
        c.mature().contains_term(root),
        "the root must be resident in the mature space"
    );
    assert!(
        !c.young().contains_term(root),
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

    assert!(is_atom(e0));
    assert_eq!(e0, atom, "the atom leaf is unchanged");
    assert!(is_list(e1));
    assert!(c.mature().contains_term(e1), "the list is in mature");
    assert_eq!(list_to_smalls(e1), vec![1, 2, 3]);

    assert!(c.mature().contains_term(e2), "the inner tuple is in mature");
    // SAFETY: `e2` is a live 2-element tuple in the mature space.
    let inner_header = unsafe { *boxed_val(e2) };
    assert!(is_arity_value(inner_header));
    assert_eq!(arityval(inner_header), 2);
    // SAFETY: elements 0..2 lie within that tuple.
    assert_eq!(signed_value(unsafe { *boxed_val(e2).add(1) }), 4);
    assert_eq!(signed_value(unsafe { *boxed_val(e2).add(2) }), 5);
}

#[test]
fn minor_promotes_into_immix_and_empties_the_nursery() {
    let mut c = GenImmixCollector::from_policy(HeapPolicy::default()).expect("collector");
    let (outer, atom) = build_graph(&mut c);

    let available = c.young().available_words();
    assert!(!c.should_collect(available), "an exact fit still fits");
    assert!(c.should_collect(available + 1));

    let mut roots = [outer];
    let collection = c.collect(&mut roots).expect("collect");

    assert_eq!(collection.kind, CollectionKind::Minor);
    assert_eq!(c.young().used_words(), 0, "the nursery is emptied");
    assert_eq!(
        collection.stats.words_copied, 13,
        "only the live words move"
    );
    assert_eq!(
        collection.stats.objects_copied, 5,
        "outer + inner + 3 cells"
    );
    assert_eq!(c.stats().minor_collections, 1);
    assert_eq!(c.stats().major_collections, 0);

    assert_graph_preserved(&c, roots[0], atom);
}

#[test]
fn fullsweep_after_forces_a_major_which_reclaims_in_place_then_promotes() {
    let policy = HeapPolicy {
        fullsweep_after: 1,
        ..HeapPolicy::default()
    };
    let mut c = GenImmixCollector::from_policy(policy).expect("collector");

    let (first, atom) = build_graph(&mut c);
    let mut roots = [first];
    let one = c.collect(&mut roots).expect("first collect");
    assert_eq!(one.kind, CollectionKind::Minor);
    assert_eq!(c.stats().minor_collections, 1);

    // Allocate again; keep the promoted root live too.
    let extra = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(9)])
        .expect("extra tuple");
    let mut roots = [roots[0], extra];
    let two = c.collect(&mut roots).expect("second collect");

    assert_eq!(two.kind, CollectionKind::Major, "fullsweep_after = 1");
    assert_eq!(c.stats().major_collections, 1);
    assert_eq!(c.stats().minor_collections, 1, "no extra minor ran");
    assert_eq!(c.young().used_words(), 0);
    assert!(
        c.last_immix_stats().is_some(),
        "the mature reclaim is observable"
    );

    for root in roots {
        assert!(c.mature().contains_term(root), "live data is in mature");
    }
    assert_graph_preserved(&c, roots[0], atom);

    let extra_root = roots[1];
    // SAFETY: `extra_root` is a live 1-element tuple in the mature space.
    let extra_header = unsafe { *boxed_val(extra_root) };
    assert!(is_arity_value(extra_header));
    assert_eq!(arityval(extra_header), 1);
    // SAFETY: element 0 lies within that tuple.
    assert_eq!(signed_value(unsafe { *boxed_val(extra_root).add(1) }), 9);
}

#[test]
fn a_major_reclaims_dead_mature_objects() {
    let policy = HeapPolicy {
        fullsweep_after: 1,
        ..HeapPolicy::default()
    };
    let mut c = GenImmixCollector::from_policy(policy).expect("collector");

    // Promote a 5-object graph, then drop it and promote a different one; the first
    // graph must be reclaimed by the next major even though it sits in the mature space.
    let (first, _atom) = build_graph(&mut c);
    let mut roots = [first];
    let one = c.collect(&mut roots).expect("promote the first graph");
    assert_eq!(one.kind, CollectionKind::Minor);
    assert!(c.mature().contains_term(roots[0]));
    assert_eq!(c.mature().used_lines(), 5);

    // A fresh, unrelated live leaf in the nursery; the first graph is now unreachable.
    let leaf = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(1)])
        .expect("leaf");
    let mut roots = [leaf];
    let collection = c.collect(&mut roots).expect("major");

    assert_eq!(
        collection.kind,
        CollectionKind::Major,
        "fullsweep_after = 1"
    );
    // The old 5-object graph is gone; only the new leaf survives.
    assert!(c.mature().contains_term(roots[0]));
    assert_eq!(
        c.mature().used_lines(),
        1,
        "the dead mature graph was reclaimed in place"
    );
    assert_eq!(
        c.mature().free_lines(),
        15,
        "the reclaimed lines are reusable"
    );
}

#[test]
fn a_minor_that_would_not_fit_escalates_to_a_major() {
    // A mature space far smaller than the nursery cannot receive a whole promotion.
    let policy = HeapPolicy {
        initial_mature_words: 1, // one block: 16 lines
        initial_young_words: 233,
        ..HeapPolicy::default()
    };
    let mut c = GenImmixCollector::from_policy(policy).expect("collector");

    // Fill most of the mature space's 16 lines with garbage promoted once, then make
    // the next nursery promotion bigger than what is left.
    let heap = c.young_mut().heap_mut();
    let mut first_roots: Vec<Eterm> = Vec::new();
    for i in 0..12i64 {
        first_roots.push(heap.cons(make_small(i), NIL).expect("cell"));
    }
    let mut roots = first_roots.clone();
    let first = c.collect(&mut roots).expect("first promote");
    assert_eq!(first.kind, CollectionKind::Minor);
    assert_eq!(c.mature().used_lines(), 12);

    // A new 8-object nursery graph was never reachable from the first roots, so it can
    // only fit after the old 12 lines are reclaimed in place.
    let heap = c.young_mut().heap_mut();
    let mut live: Vec<Eterm> = Vec::new();
    for i in 100..108i64 {
        live.push(heap.cons(make_small(i), NIL).expect("cell"));
    }
    let mut roots = live.clone();
    let second = c.collect(&mut roots).expect("escalated major");

    assert_eq!(
        second.kind,
        CollectionKind::Major,
        "would-not-fit minor escalates"
    );
    assert_eq!(c.stats().major_collections, 1);
    for root in roots {
        assert!(c.mature().contains_term(root));
    }
    assert_eq!(c.mature().used_lines(), 8, "the dead graph was reclaimed");
}

#[test]
fn nursery_references_to_mature_objects_survive_a_major() {
    // The hard case: a live nursery object points *back* into the mature space, and the
    // major reclaim must both keep that mature object alive and rewrite the nursery's
    // reference to its final (possibly evacuated) address.
    let policy = HeapPolicy {
        fullsweep_after: 1,
        ..HeapPolicy::default()
    };
    let mut c = GenImmixCollector::from_policy(policy).expect("collector");

    // Promote a mature object (a 1-element tuple); `roots[0]` is its mature address.
    let leaf = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(42)])
        .expect("leaf");
    let mut roots = [leaf];
    let one = c.collect(&mut roots).expect("promote the leaf");
    assert_eq!(one.kind, CollectionKind::Minor);
    let leaf_in_mature = roots[0];
    assert!(c.mature().contains_term(leaf_in_mature));

    // A young holder whose element 0 is the mature leaf — the young->mature edge a naive
    // in-place reclaim would either drop or leave dangling.
    let holder = c
        .young_mut()
        .heap_mut()
        .tuple(&[leaf_in_mature])
        .expect("holder");

    let mut roots = [holder];
    let collection = c.collect(&mut roots).expect("major");

    assert_eq!(
        collection.kind,
        CollectionKind::Major,
        "fullsweep_after = 1"
    );
    let holder = roots[0];
    assert!(c.mature().contains_term(holder), "the holder was promoted");

    // SAFETY: `holder` is a live 1-element tuple in the mature space.
    let inner = unsafe { *boxed_val(holder).add(1) };
    assert!(
        c.mature().contains_term(inner),
        "the mature leaf survived the reclaim and the nursery reference was rewritten"
    );
    // SAFETY: `inner` is the live 1-element tuple; its element is the small integer.
    assert_eq!(signed_value(unsafe { *boxed_val(inner).add(1) }), 42);
}

#[test]
fn explicit_collect_minor_and_collect_major_drive_both_paths() {
    let mut c = GenImmixCollector::from_policy(HeapPolicy::default()).expect("collector");
    let (outer, atom) = build_graph(&mut c);

    let mut roots = [outer];
    let minor = c.collect_minor(&mut roots).expect("minor");
    assert_eq!(c.stats().minor_collections, 1);
    assert!(minor.words_copied > 0);
    assert_graph_preserved(&c, roots[0], atom);

    // An explicit major reclaims in place and promotes.
    let extra = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(9)])
        .expect("extra tuple");
    let mut roots = [roots[0], extra];
    let major = c.collect_major(&mut roots).expect("major");
    assert_eq!(c.stats().major_collections, 1);
    assert!(major.words_copied >= 2);
    for root in roots {
        assert!(c.mature().contains_term(root));
    }
    assert_graph_preserved(&c, roots[0], atom);
}

#[test]
fn collect_minor_reports_exhaustion_before_copying_anything() {
    // A single-block mature space holds 16 lines.
    let policy = HeapPolicy {
        initial_mature_words: 1,
        ..HeapPolicy::default()
    };
    let mut c = GenImmixCollector::from_policy(policy).expect("collector");

    // Fill the mature space with 12 live lines.
    let heap = c.young_mut().heap_mut();
    let mut first = Vec::new();
    for i in 0..12i64 {
        first.push(heap.cons(make_small(i), NIL).expect("cell"));
    }
    let mut roots = first.clone();
    c.collect_minor(&mut roots).expect("first minor");
    assert_eq!(c.mature().used_lines(), 12);

    // An 8-line promotion cannot fit the 4 free lines: the typed error is raised before
    // any word is copied, so the mature space is untouched (never half-forwarded).
    let heap = c.young_mut().heap_mut();
    let mut second = Vec::new();
    for i in 0..8i64 {
        second.push(heap.cons(make_small(i), NIL).expect("cell"));
    }
    let mut roots = second.clone();
    let err = c.collect_minor(&mut roots).expect_err("must not fit");
    assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");
    assert_eq!(c.mature().used_lines(), 12, "nothing was copied");
    assert_eq!(
        c.stats().minor_collections,
        1,
        "the failed minor is not counted"
    );
    assert!(roots.iter().all(|r| c.young().contains_term(*r)));
}
