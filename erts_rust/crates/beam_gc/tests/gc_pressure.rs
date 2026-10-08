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

//! **Correctness under pressure** — the generational collector driven through a
//! sustained promotion/collection cadence (deliverable 3, scenario 1: *threshold
//! pressure*).
//!
//! A generational collector is only correct if its invariant ("a mature object never
//! refers to a young object") holds after *every* collection, and only useful if the
//! live graph survives every promotion. This file drives `Collector::collect` through
//! many minors and majors with the `fullsweep_after` cadence set to **1**, **2** and the
//! **default** (escalation-forced here), and after *each* collection asserts:
//!
//! * the nursery is empty (`young().used_words() == 0`);
//! * **every** root is reachable and structurally intact — the round-tagged tuple and
//!   its `[round, round+1, round+2]` list are read back from the updated root and their
//!   leaf values checked, so a dropped or mis-forwarded object is caught immediately;
//! * the sequence of [`CollectionKind`]s matches the configured cadence.
//!
//! The same corpus (scenario 1) is then re-driven through `AnyCollector` so it runs
//! under both the copying mature space and the **GenImmix** mark-region mature space,
//! asserting identical collection-kind sequences and an equally intact live graph.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself; every
//! block carries a `// SAFETY:` rationale.

use beam_alloc::Heap;
use beam_gc::prelude::{AnyCollector, CollectionKind, Collector, GcConfig, HeapPolicy, MatureKind};
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, is_list, list_val, make_small, signed_value, Eterm, NIL,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A policy with the given fullsweep cadence and explicit initial sizes.
fn policy(cadence: u32, young: usize, mature: usize) -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: cadence,
        initial_young_words: young,
        initial_mature_words: mature,
        ..HeapPolicy::default()
    }
}

/// The small integers of a proper list, in order.
fn list_to_smalls(mut term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    while is_list(term) {
        // SAFETY: `term` is a live cons cell (`is_list`), so both its car and cdr words
        // are in-bounds and initialised.
        let cell = list_val(term);
        values.push(signed_value(unsafe { *cell }));
        term = unsafe { *cell.add(1) };
    }
    assert_eq!(term, NIL, "the list must be proper");
    values
}

/// Builds the small round-tagged graph `{round, [round, round+1, round+2]}` in `heap`.
///
/// It fits in one round's worth of nursery space and every leaf encodes the round, so a
/// root that survives a collection can be verified against the round that built it.
fn build_round_graph(heap: &mut Heap, round: i64) -> Eterm {
    let c2 = heap.cons(make_small(round + 2), NIL).expect("cons");
    let c1 = heap.cons(make_small(round + 1), c2).expect("cons");
    let c0 = heap.cons(make_small(round), c1).expect("cons");
    heap.tuple(&[make_small(round), c0]).expect("tuple")
}

/// Asserts `root` is still the round-`round` graph: a `{round, [round, round+1, round+2]}`
/// tuple with every leaf intact.
fn assert_round_graph(root: Eterm, round: i64) {
    // SAFETY: `root` is a live boxed term the owning heap keeps unmoved for the test.
    let header = unsafe { *boxed_val(root) };
    assert!(is_arity_value(header), "the round graph is a tuple");
    assert_eq!(arityval(header), 2, "the round tuple has two elements");
    // SAFETY: element 0 of the live 2-element tuple just validated.
    assert_eq!(
        signed_value(unsafe { *boxed_val(root).add(1) }),
        round,
        "the round tag survives the collection"
    );
    // SAFETY: element 1 of the same live 2-element tuple.
    let list = unsafe { *boxed_val(root).add(2) };
    assert_eq!(
        list_to_smalls(list),
        vec![round, round + 1, round + 2],
        "the list survives the collection"
    );
}

/// Whether `term` is resident in the strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

// ---------------------------------------------------------------------------
// Scenario 1a: threshold pressure through the copying `Collector`
// ---------------------------------------------------------------------------

/// Drives one `rounds`-long run with the given fullsweep cadence, allocating a fresh
/// round graph plus deliberate garbage each round, and asserts after **every** collection
/// that the nursery is empty and every accumulated root is intact. Returns the sequence of
/// collection kinds.
fn run_copying_cadence(cadence: u32, mature: usize, rounds: usize) -> Vec<CollectionKind> {
    let mut collector = Collector::new(policy(cadence, 2048, mature)).expect("collector");
    let mut roots: Vec<Eterm> = Vec::new();
    let mut kinds = Vec::new();

    for round in 0..rounds as i64 {
        let graph = build_round_graph(collector.young_mut().heap_mut(), round);
        roots.push(graph);
        // Deliberate garbage no root reaches; a collection must reclaim it.
        let _garbage = collector
            .young_mut()
            .heap_mut()
            .tuple(&[make_small(-1)])
            .expect("garbage tuple");

        let collection = collector.collect(&mut roots).expect("collect");
        kinds.push(collection.kind);

        assert_eq!(
            collector.young().used_words(),
            0,
            "the nursery is empty after round {round}"
        );

        // Every root allocated so far must still describe its own round, from the root
        // word `collect` just rewrote (young garbage list in a minor, the whole graph in a
        // major).
        for (i, root) in roots.iter().enumerate() {
            assert_round_graph(*root, i as i64);
            assert!(
                collector.mature().contains_term(*root),
                "root {i} must be resident in the mature space after round {round}"
            );
            assert!(
                !collector.young().contains_term(*root),
                "root {i} must no longer be in the nursery after round {round}"
            );
        }
    }

    kinds
}

/// `fullsweep_after = 1`: the sequence alternates Minor, Major, Minor, Major, …
#[test]
fn fullsweep_after_one_alternates_minor_and_major_under_pressure() {
    let kinds = run_copying_cadence(1, 4096, 40);
    for (i, kind) in kinds.iter().enumerate() {
        let expected = if i % 2 == 0 {
            CollectionKind::Minor
        } else {
            CollectionKind::Major
        };
        assert_eq!(*kind, expected, "cadence 1 at collection {i}");
    }
    assert!(kinds.contains(&CollectionKind::Minor));
    assert!(kinds.contains(&CollectionKind::Major));
}

/// `fullsweep_after = 2`: Minor, Minor, Major, Minor, Minor, Major, …
#[test]
fn fullsweep_after_two_runs_a_minor_then_a_major_every_third_collection() {
    let kinds = run_copying_cadence(2, 4096, 40);
    for (i, kind) in kinds.iter().enumerate() {
        let expected = if i % 3 == 2 {
            CollectionKind::Major
        } else {
            CollectionKind::Minor
        };
        assert_eq!(*kind, expected, "cadence 2 at collection {i}");
    }
}

/// The **default** cadence (65535) with a small mature space: minors run until the mature
/// space can no longer receive the nursery, at which point a fullsweep is forced by
/// escalation. Both kinds must occur, and the graph must survive both.
#[test]
fn default_cadence_escalates_to_major_when_the_mature_space_fills() {
    let kinds = run_copying_cadence(HeapPolicy::default().fullsweep_after, 64, 40);
    assert_eq!(kinds[0], CollectionKind::Minor, "the first collection is a minor");
    assert!(
        kinds.contains(&CollectionKind::Minor),
        "minors must run under the default cadence"
    );
    assert!(
        kinds.contains(&CollectionKind::Major),
        "a full mature space must escalate a would-not-fit minor to a fullsweep"
    );
}

/// The threshold semantics `Collector` exposes: an exact-fit allocation still fits, one
/// word more does not.
#[test]
fn should_collect_is_true_only_past_the_remaining_capacity() {
    let mut collector = Collector::new(policy(1, 512, 4096)).expect("collector");
    let _ = build_round_graph(collector.young_mut().heap_mut(), 0);

    let available = collector.young().available_words();
    assert!(!collector.should_collect(available), "an exact fit still fits");
    assert!(
        collector.should_collect(available + 1),
        "one word past the capacity must collect"
    );
}

// ---------------------------------------------------------------------------
// Scenario 1b: the same pressure through both mature strategies (`AnyCollector`)
// ---------------------------------------------------------------------------

/// Drives the scenario-1 corpus through the `kind` strategy and asserts the nursery is
/// emptied and every root is intact after **every** collection. Returns the collection
/// kinds.
fn run_any_cadence(kind: MatureKind, cadence: u32, rounds: usize) -> Vec<CollectionKind> {
    let config = GcConfig::copying(policy(cadence, 2048, 8192)).with_mature(kind);
    let mut collector = AnyCollector::from_config(&config).expect("collector");
    assert_eq!(collector.kind(), kind, "the config selected the requested strategy");

    let mut roots: Vec<Eterm> = Vec::new();
    let mut kinds = Vec::new();

    for round in 0..rounds as i64 {
        let graph = build_round_graph(collector.young_mut().heap_mut(), round);
        roots.push(graph);
        let _garbage = collector
            .young_mut()
            .heap_mut()
            .tuple(&[make_small(-1)])
            .expect("garbage tuple");

        let collection = collector.collect(&mut roots).expect("collect");
        kinds.push(collection.kind);

        assert_eq!(
            collector.young().used_words(),
            0,
            "{kind:?}: the nursery is empty after round {round}"
        );
        for (i, root) in roots.iter().enumerate() {
            assert_round_graph(*root, i as i64);
            assert!(
                !collector.young().contains_term(*root),
                "{kind:?}: root {i} stayed in the nursery after round {round}"
            );
            assert!(
                in_mature(&collector, *root),
                "{kind:?}: root {i} was not promoted into the mature generation after round {round}"
            );
        }
    }

    kinds
}

/// The copying and GenImmix strategies must produce **identical** collection-kind
/// sequences and equally intact live graphs under the same cadence — switching the mature
/// strategy may change where data lives, never what the mutator observes.
#[test]
fn both_mature_strategies_survive_the_same_pressure_cadence() {
    for cadence in [1u32, 2] {
        let copying = run_any_cadence(MatureKind::Copying, cadence, 30);
        let immix = run_any_cadence(MatureKind::Immix, cadence, 30);

        assert_eq!(
            copying, immix,
            "cadence {cadence}: the strategies disagree on the collection-kind sequence"
        );
        assert!(copying.contains(&CollectionKind::Minor));
        assert!(copying.contains(&CollectionKind::Major));
    }
}

/// The **default cadence** (65535) on GenImmix with a live set that stays well inside the
/// fixed-size mark-region space: the cadence never forces a fullsweep and the region is
/// never tight, so only minors (promotions) run — yet the nursery is still emptied and the
/// whole live graph survives every one of them.
///
/// This pins the documented GenImmix behaviour: unlike the copying collector, its mature
/// region is fixed-size and is only reclaimed by a fullsweep, so under the default cadence
/// the collector promotes indefinitely as long as the region has room.
#[test]
fn gen_immix_default_cadence_promotes_repeatedly_while_the_region_has_room() {
    let kinds = run_any_cadence(MatureKind::Immix, HeapPolicy::default().fullsweep_after, 60);
    assert_eq!(kinds[0], CollectionKind::Minor);
    assert!(
        kinds.iter().all(|k| *k == CollectionKind::Minor),
        "with the default cadence and free region lines, only promotions run"
    );
}
