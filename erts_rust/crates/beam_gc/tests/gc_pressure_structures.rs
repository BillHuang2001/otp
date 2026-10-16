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

//! **Correctness under pressure** — structure-heavy corpora driven through promotions and
//! fullsweeps, and shared/cyclic graphs (deliverable 3, scenarios 4 and 5).
//!
//! ## Scenario 4 — a HAMT map spanning a promotion
//! A >[`MAP_SMALL_MAP_LIMIT`]-key map is a CHAMP trie of interior nodes (a head node, inner
//! bitmap nodes and `[Key,Value]` collision leaves). Every one of those nodes is a *boxed*
//! object the copier must size and forward, and every interior slot is a *reference* it
//! must rewrite. A map built in the nursery, promoted by a minor collection and then
//! relocated again by a fullsweep, therefore exercises the interior-node fix-up twice. The
//! tests read the whole key set back (`map_get`/`map_pairs`) from the updated root after
//! *each* collection — a single mis-forwarded interior pointer either dangles or fails to
//! resolve a key.
//!
//! ## Scenario 5 — shared and cyclic graphs
//! A subterm referenced from several places must be copied **once** and stay shared (not
//! duplicated): the Cheney scan must find the forwarding marker (`IS_MOVED_*`) on the
//! second reference rather than copy the object again. This is pinned through
//! [`GcStats::objects_copied`] — a duplicated shared object would inflate it. A hand-made
//! self-referential cons cell and a two-cell cycle are then collected: Erlang itself cannot
//! construct a cyclic term, so these are built by writing raw heap words, purely to prove
//! the copier's marker protocol closes the cycle on the *copies* and terminates (no infinite
//! scan, no corruption).
//!
//! Scenarios 4 and 5 are also driven through `AnyCollector` so the same corpus runs under
//! both the copying mature space and the **GenImmix** mark-region mature space.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale.

use beam_alloc::{default_allocator, Heap};
use beam_gc::prelude::{
    collect, AnyCollector, CollectionKind, Collector, GcConfig, HeapPolicy, HeapSpace, MatureKind,
};
use beam_term::prelude::{
    boxed_val, is_list, list_val, make_arityval, make_boxed, make_list, make_map_header,
    make_small, map_get, map_pairs, map_size, map_words_capacity, signed_value, write_map,
    AtomTable, Eterm, MAP_HEADER_TAG_HAMT_HEAD_BITMAP, MAP_SMALL_MAP_LIMIT, NIL,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

/// A policy with the given cadence and explicit initial sizes.
fn policy(cadence: u32, young: usize, mature: usize) -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: cadence,
        initial_young_words: young,
        initial_mature_words: mature,
        ..HeapPolicy::default()
    }
}

/// Like [`policy`], but with an explicit `min_heap_words` floor.
///
/// The (C-faithful) post-collection resize (`HeapPolicy::young_after_collection`, mirroring
/// `erl_gc.c:1493`) *shrinks* a nursery the round leaves mostly empty, so a test that must
/// reserve a fresh, *larger* block in the nursery each round has to pin a minimum.
fn policy_with_min(cadence: u32, young: usize, mature: usize, min_heap_words: usize) -> HeapPolicy {
    HeapPolicy {
        min_heap_words,
        ..policy(cadence, young, mature)
    }
}

/// The `i -> 3*i` pair set (`n` distinct integer keys).
fn pairs(n: usize) -> Vec<(Eterm, Eterm)> {
    (0..n as i64)
        .map(|i| (make_small(i), make_small(i * 3)))
        .collect()
}

/// Reserves the [`map_words_capacity`] upper bound at the heap top and writes the `n`-key
/// `i -> 3*i` map there.
fn build_hamt(heap: &mut Heap, atoms: &AtomTable, n: usize) -> Eterm {
    let cap = map_words_capacity(n);
    let input = pairs(n);
    // SAFETY: `cap` exclusively-owned, writable words are reserved; `write_map` writes a
    // contiguous prefix of them with well-formed terms and returns a root into that prefix.
    unsafe {
        let hp = heap.alloc_words(cap).expect("reserve the map block");
        write_map(hp, &input, atoms).expect("build the map")
    }
}

/// Asserts `map` is a well-formed `n`-key `i -> 3*i` map — the full key set resolved from
/// the (possibly relocated) root.
fn assert_pairs(map: Eterm, n: usize, atoms: &AtomTable) {
    assert_eq!(map_size(map, atoms).expect("map_size"), n, "key count");
    for i in 0..n as i64 {
        assert_eq!(
            map_get(map, make_small(i), atoms).expect("map_get"),
            Some(make_small(i * 3)),
            "key {i} must resolve to its value"
        );
    }
    let resolved = map_pairs(map, atoms).expect("map_pairs");
    assert_eq!(resolved.len(), n);
    let keys: Vec<Eterm> = resolved.into_iter().map(|(k, _)| k).collect();
    let expected: Vec<Eterm> = (0..n as i64).map(make_small).collect();
    assert_eq!(keys, expected, "the key set must be exactly `0..n`");
}

/// Whether `term` is resident in the strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// The shared subterm reached through both `A` and `B` of the `[A, B]` corpus, plus the
/// small integers they hold in their second slot: `(s_routed_through_a, s_routed_through_b,
/// a_value, b_value)`.
fn shared_subterm_views(root: Eterm) -> (Eterm, Eterm, i64, i64) {
    // SAFETY: `root` is a live cons cell of the test's `[A|[B|[]]]` corpus.
    let cell0 = list_val(root);
    let a = unsafe { *cell0 };
    // SAFETY: the cdr of `root` is a live cons cell.
    let cell1 = list_val(unsafe { *cell0.add(1) });
    let b = unsafe { *cell1 };
    // SAFETY: `a`/`b` are live 2-element tuples `{S, Value}`.
    let sa = unsafe { *boxed_val(a).add(1) };
    let sb = unsafe { *boxed_val(b).add(1) };
    let va = signed_value(unsafe { *boxed_val(a).add(2) });
    let vb = signed_value(unsafe { *boxed_val(b).add(2) });
    (sa, sb, va, vb)
}

// ---------------------------------------------------------------------------
// Scenario 4a: a HAMT map promoted by a minor, then relocated by a fullsweep
// ---------------------------------------------------------------------------

#[test]
fn hamt_map_survives_a_promotion_then_a_fullsweep() {
    const N: usize = 80; // > MAP_SMALL_MAP_LIMIT => a HAMT trie
    const _: () = assert!(80 > MAP_SMALL_MAP_LIMIT);

    let atoms = AtomTable::new();
    let mut collector = Collector::new(policy(1, 8192, 16384)).expect("collector");
    let map = build_hamt(collector.young_mut().heap_mut(), &atoms, N);
    assert!(
        collector.young().contains_term(map),
        "the map starts in the nursery"
    );

    let mut roots = [map];
    let first = collector.collect(&mut roots).expect("collect");
    assert_eq!(
        first.kind,
        CollectionKind::Minor,
        "the first collect promotes the map"
    );
    assert!(
        collector.mature().contains_term(roots[0]) && !collector.young().contains_term(roots[0]),
        "the map is promoted out of the nursery into the mature space"
    );
    assert_pairs(roots[0], N, &atoms);

    // A fullsweep evacuates the mature space into a fresh one: the head node and every
    // inner/collision node move again and must be re-linked.
    let second = collector.collect(&mut roots).expect("collect");
    assert_eq!(
        second.kind,
        CollectionKind::Major,
        "the second collect is a fullsweep"
    );
    assert!(collector.mature().contains_term(roots[0]));
    assert_pairs(roots[0], N, &atoms);
    assert_eq!(collector.young().used_words(), 0);
}

// ---------------------------------------------------------------------------
// Scenario 4b: the same map across two disjoint-space hops (unambiguous relocation)
// ---------------------------------------------------------------------------

/// Two successive `collect`s into freshly allocated, *disjoint* to-spaces: after each hop
/// the map root lives in a space the from-space does not overlap, so "the interior nodes
/// moved" is not an allocator-address coincidence but a proven relocation, and the whole
/// key set still resolves.
#[test]
fn hamt_map_interior_nodes_are_relocated_across_two_disjoint_spaces() {
    const N: usize = 100;
    let atoms = AtomTable::new();
    let cap = map_words_capacity(N);

    let mut from = space(cap);
    // SAFETY: `cap` exclusively-owned words reserved for the map block.
    let map = unsafe {
        let hp = from.heap_mut().alloc_words(cap).expect("reserve");
        write_map(hp, &pairs(N), &atoms).expect("build the map")
    };
    assert_pairs(map, N, &atoms);

    let mut to = space(cap);
    let mut roots = [map];
    collect(&from, &mut to, &mut roots).expect("hop 1");
    assert!(
        to.contains_term(roots[0]) && !from.contains_term(roots[0]),
        "hop 1: the map moved into a disjoint to-space"
    );
    assert_pairs(roots[0], N, &atoms);

    let mut to2 = space(cap);
    collect(&to, &mut to2, &mut roots).expect("hop 2");
    assert!(
        to2.contains_term(roots[0]) && !to.contains_term(roots[0]),
        "hop 2: the map moved again, into a second disjoint space"
    );
    assert_pairs(roots[0], N, &atoms);
}

// ---------------------------------------------------------------------------
// Scenario 4c: a hand-built collision node spanning a promotion
// ---------------------------------------------------------------------------

/// A collision node is a *transparent* `arityval` tuple whose cells are `[Key,Value]` cons
/// cells. This hand-builds the minimal valid HAMT head (`[hdr, size, slot -> collision]`)
/// so the copier meets a boxed slot holding a tuple it must size as `arity + 1` and then
/// walk transparently to forward each cons cell — the exact scan path a real collision node
/// takes. It is promoted by a minor and then relocated by a fullsweep.
#[test]
fn collision_node_survives_a_promotion_then_a_fullsweep() {
    const WORDS: usize = 10; // [k0,v0][k1,v1][arityval(2),cell0,cell1][hdr,size,slot]
    let mut collector = Collector::new(policy(1, 8192, 16384)).expect("collector");

    // SAFETY: `WORDS` exclusively-owned words are reserved; every word is written below and
    // belongs to the single live map graph rooted at the returned head.
    let map = unsafe {
        let hp = collector
            .young_mut()
            .heap_mut()
            .alloc_words(WORDS)
            .expect("reserve the minimal map block");
        *hp = make_small(0);
        *hp.add(1) = make_small(0);
        *hp.add(2) = make_small(1);
        *hp.add(3) = make_small(3);
        let cell0 = make_list(hp as *const Eterm);
        let cell1 = make_list(hp.add(2) as *const Eterm);
        let coll = hp.add(4);
        *coll = make_arityval(2);
        *coll.add(1) = cell0;
        *coll.add(2) = cell1;
        let head = hp.add(7);
        *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 1);
        *head.add(1) = Eterm::from_raw(2); // the head's size word
        *head.add(2) = make_boxed(coll);
        make_boxed(head)
    };

    let mut roots = [map];
    let first = collector.collect(&mut roots).expect("collect");
    assert_eq!(first.kind, CollectionKind::Minor);
    let copied = roots[0];
    assert!(collector.mature().contains_term(copied));

    // The head node kept its header and size word; its slot was forwarded to a copied
    // collision tuple whose two cells carry the original keys and values.
    // SAFETY: `copied` is the live, copied 3-word head node.
    let head = boxed_val(copied);
    assert_eq!(
        unsafe { *head },
        make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 1)
    );
    assert_eq!(unsafe { *head.add(1) }.raw(), 2);
    let slot = unsafe { *head.add(2) };
    assert!(
        collector.mature().contains_term(slot),
        "the collision tuple moved"
    );
    // SAFETY: `slot` is the live, copied collision tuple (a transparent arityval tuple).
    let coll = boxed_val(slot);
    assert_eq!(unsafe { *coll }, make_arityval(2));
    // SAFETY: the collision tuple holds two cons-cell terms (words 1 and 2).
    let (c0, c1) = unsafe { (*coll.add(1), *coll.add(2)) };
    assert!(collector.mature().contains_term(c0) && collector.mature().contains_term(c1));
    // SAFETY: `c0`/`c1` are live, copied cons cells.
    unsafe {
        assert_eq!(*list_val(c0), make_small(0));
        assert_eq!(*list_val(c0).add(1), make_small(0));
        assert_eq!(*list_val(c1), make_small(1));
        assert_eq!(*list_val(c1).add(1), make_small(3));
    }

    // A fullsweep then relocates the whole structure once more; re-check from the new root.
    let second = collector.collect(&mut roots).expect("collect");
    assert_eq!(second.kind, CollectionKind::Major);
    let copied = roots[0];
    assert!(collector.mature().contains_term(copied));
    // SAFETY: `copied` is the live, copied head node after the fullsweep.
    let head = boxed_val(copied);
    assert_eq!(
        unsafe { *head },
        make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 1)
    );
    let slot = unsafe { *head.add(2) };
    // SAFETY: `slot` is the live, copied collision tuple.
    let coll = boxed_val(slot);
    assert_eq!(unsafe { *coll }, make_arityval(2));
    // SAFETY: the collision tuple's two cons-cell slots.
    let (c0, c1) = unsafe { (*coll.add(1), *coll.add(2)) };
    // SAFETY: `c0`/`c1` are live, copied cons cells.
    unsafe {
        assert_eq!(*list_val(c0), make_small(0));
        assert_eq!(*list_val(c0).add(1), make_small(0));
        assert_eq!(*list_val(c1), make_small(1));
        assert_eq!(*list_val(c1).add(1), make_small(3));
    }
}

// ---------------------------------------------------------------------------
// Scenario 4d: the HAMT corpus through both mature strategies
// ---------------------------------------------------------------------------

/// A HAMT map is built in the nursery each round and the accumulated maps are collected
/// under the alternating (minor, major) cadence — so every map is promoted and then
/// fullswept — through **both** mature strategies. After each collection every map must
/// still resolve its full key set.
#[test]
fn hamt_maps_survive_every_collection_on_both_strategies() {
    const ROUNDS: usize = 4;
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        // The nursery is re-sized after every collection (the chain collapses to one space,
        // then `HeapPolicy::young_after_collection` sizes it from the surviving words,
        // mirroring C `erl_gc.c:1493`), which *shrinks* it when a round leaves it mostly
        // empty. Each round below reserves a fresh, growing HAMT block in the nursery
        // (`map_words_capacity(100) == 2118` words by the last round), so pin a
        // `min_heap_words`/`initial_young_words` floor at/above that; otherwise the resize
        // shrinks the nursery below the next round's reservation and the build fails (a
        // brittle test, not a collector fault).
        let config = GcConfig::copying(policy_with_min(1, 4096, 16384, 4096)).with_mature(kind);
        let mut collector = AnyCollector::from_config(&config).expect("collector");
        let atoms = AtomTable::new();

        let mut roots: Vec<Eterm> = Vec::new();
        let mut ns: Vec<usize> = Vec::new();
        for round in 0..ROUNDS {
            let n = 40 + round * 20; // all > MAP_SMALL_MAP_LIMIT => HAMT tries
            roots.push(build_hamt(collector.young_mut().heap_mut(), &atoms, n));
            ns.push(n);

            collector.collect(&mut roots).expect("collect");
            assert_eq!(
                collector.young().used_words(),
                0,
                "{kind:?}: nursery empty after round {round}"
            );
            for (root, &n) in roots.iter().zip(&ns) {
                assert!(
                    in_mature(&collector, *root),
                    "{kind:?}: map moved into mature"
                );
                assert_pairs(*root, n, &atoms);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario 5a: a shared subterm is copied exactly once
// ---------------------------------------------------------------------------

/// `[A, B]` where both `A = {S, 10}` and `B = {S, 20}` reference the same `S = {7, 8}`:
/// the shared subterm must be copied **once** (asserted through `objects_copied`/`words_copied`),
/// both paths must reach the *same* copied object, and its leaves must be intact.
#[test]
fn a_shared_subterm_is_copied_exactly_once() {
    let mut from = space(64);
    let s = from
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("shared tuple");
    let a = from.heap_mut().tuple(&[s, make_small(10)]).expect("A");
    let b = from.heap_mut().tuple(&[s, make_small(20)]).expect("B");
    let tail = from.heap_mut().cons(b, NIL).expect("tail");
    let root = from.heap_mut().cons(a, tail).expect("root");
    assert_eq!(
        from.used_words(),
        13,
        "S(3) + A(3) + B(3) + two cons cells(4)"
    );

    let mut to = space(13);
    let mut roots = [root];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    assert_eq!(
        stats.objects_copied, 5,
        "S must be copied once (it is reachable through both A and B)"
    );
    assert_eq!(stats.words_copied, 13);
    assert_eq!(
        to.used_words(),
        13,
        "the to-space holds the whole live graph once"
    );
    assert_eq!(stats.words_reclaimed, 0, "the whole from-space was live");
    assert!(to.contains_term(roots[0]));

    let (sa, sb, va, vb) = shared_subterm_views(roots[0]);
    assert_eq!(sa, sb, "the shared subterm must be a single copied object");
    assert!(
        to.contains_term(sa),
        "the shared subterm moved into the to-space"
    );
    assert_eq!((va, vb), (10, 20), "the two paths keep their own values");
    // SAFETY: `sa` is the live, copied shared `{7,8}` tuple.
    assert_eq!(unsafe { *boxed_val(sa) }, make_arityval(2));
    assert_eq!(unsafe { *boxed_val(sa).add(1) }, make_small(7));
    assert_eq!(unsafe { *boxed_val(sa).add(2) }, make_small(8));
}

/// The tightest sharing proof: a 3-way diamond `{S, S, S}` — three references, one copy.
#[test]
fn a_three_way_diamond_copies_the_shared_leaf_once() {
    let mut from = space(16);
    let s = from.heap_mut().tuple(&[make_small(1)]).expect("S");
    let root = from
        .heap_mut()
        .tuple(&[s, s, s])
        .expect("three-way diamond");
    assert_eq!(from.used_words(), 6, "S (2 words) + the 3-tuple (4 words)");

    let mut to = space(6);
    let mut roots = [root];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    assert_eq!(
        stats.objects_copied, 2,
        "S is copied once for all three references"
    );
    assert_eq!(stats.words_copied, 6);
    assert_eq!(to.used_words(), 6);

    // SAFETY: `roots[0]` is the live, copied 3-element tuple.
    let p = boxed_val(roots[0]);
    let e: Vec<Eterm> = unsafe { (0..3).map(|i| *p.add(1 + i)).collect() };
    assert_eq!(e[0], e[1], "all three slots share one copied object");
    assert_eq!(e[1], e[2]);
    assert!(to.contains_term(e[0]));
}

// ---------------------------------------------------------------------------
// Scenario 5b/c: hand-made cycles
// ---------------------------------------------------------------------------

/// A **self-referential cons cell** (`[1 | self]`) is copied without corruption and without
/// an infinite Cheney scan: the cycle closes on the *copy*.
///
/// Erlang cannot construct a cyclic term; this writes raw heap words purely to exercise the
/// copier's forwarding-marker path (the second visit of the cell must follow the marker,
/// not re-copy it).
#[test]
fn a_self_referential_cons_cycle_is_copied_without_corruption() {
    let mut from = space(8);
    // SAFETY: 2 words are reserved for one cons cell; both words are written and the cdr is
    // a LIST-tagged pointer back to the cell itself (a deliberate cycle).
    let root = unsafe {
        let hp = from.heap_mut().alloc_words(2).expect("cons cell");
        *hp = make_small(1);
        *hp.add(1) = make_list(hp as *const Eterm);
        make_list(hp as *const Eterm)
    };

    let mut to = space(2);
    let mut roots = [root];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    assert_eq!(
        stats.objects_copied, 1,
        "the cell is copied once, not repeatedly"
    );
    assert_eq!(stats.words_copied, 2);
    let copied = roots[0];
    assert!(to.contains_term(copied) && !from.contains_term(copied));

    // SAFETY: `copied` is the live, copied cons cell.
    let cell = list_val(copied);
    assert_eq!(unsafe { *cell }, make_small(1));
    assert_eq!(
        unsafe { *cell.add(1) },
        copied,
        "the cycle closes on the copy"
    );
}

/// A **two-cell cycle** (`X -> Y -> X`) is copied without corruption: both cells are copied
/// once and the cycle closes between the two *copies*.
#[test]
fn a_two_cell_cons_cycle_is_copied_without_corruption() {
    let mut from = space(8);
    // SAFETY: two 2-word cons cells are reserved; each is fully written with the other's
    // LIST-tagged pointer as its cdr (a deliberate two-cell cycle).
    let (x, y) = unsafe {
        let px = from.heap_mut().alloc_words(2).expect("cell X");
        let py = from.heap_mut().alloc_words(2).expect("cell Y");
        *px = make_small(1);
        *px.add(1) = make_list(py as *const Eterm);
        *py = make_small(2);
        *py.add(1) = make_list(px as *const Eterm);
        (make_list(px as *const Eterm), make_list(py as *const Eterm))
    };
    let _ = y;

    let mut to = space(4);
    let mut roots = [x];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    assert_eq!(stats.objects_copied, 2, "each cell is copied exactly once");
    assert_eq!(stats.words_copied, 4);

    let cx = roots[0];
    assert!(to.contains_term(cx));
    // SAFETY: `cx` is a live, copied cons cell.
    let px = list_val(cx);
    assert_eq!(unsafe { *px }, make_small(1));
    let cy = unsafe { *px.add(1) };
    assert!(to.contains_term(cy), "the cycle's second cell moved too");
    // SAFETY: `cy` is the live, copied second cell.
    let py = list_val(cy);
    assert_eq!(unsafe { *py }, make_small(2));
    assert_eq!(
        unsafe { *py.add(1) },
        cx,
        "the two-cycle closes on the copies"
    );
}

// ---------------------------------------------------------------------------
// Scenario 5d: shared + cyclic corpora through both mature strategies
// ---------------------------------------------------------------------------

/// What each retained root of the `AnyCollector` pressure corpus is.
#[derive(Clone, Copy)]
enum Corpus {
    /// A `[A, B]` list where `A = {S, base+10}` and `B = {S, base+20}` share `S`.
    Shared(i64),
    /// A two-cell cycle whose cells carry `1` and `2`.
    Cycle,
}

/// Verifies a retained corpus root against its description, from the (possibly relocated)
/// root word.
fn assert_corpus(root: Eterm, corpus: Corpus) {
    match corpus {
        Corpus::Shared(base) => {
            let (sa, sb, va, vb) = shared_subterm_views(root);
            assert_eq!(
                sa, sb,
                "base {base}: the shared subterm stays a single object"
            );
            assert_eq!(
                (va, vb),
                (base + 10, base + 20),
                "base {base}: path values intact"
            );
            // SAFETY: `sa` is the live, copied shared `{base, base+1}` tuple.
            assert_eq!(unsafe { *boxed_val(sa).add(1) }, make_small(base));
            assert_eq!(unsafe { *boxed_val(sa).add(2) }, make_small(base + 1));
        }
        Corpus::Cycle => {
            // SAFETY: `root` is a live, copied cons cell of the cycle.
            let px = list_val(root);
            assert_eq!(unsafe { *px }, make_small(1));
            let cy = unsafe { *px.add(1) };
            assert!(is_list(cy), "the cycle's second cell is a list");
            // SAFETY: `cy` is the live, copied second cell.
            let py = list_val(cy);
            assert_eq!(unsafe { *py }, make_small(2));
            assert_eq!(unsafe { *py.add(1) }, root, "the two-cycle stays closed");
        }
    }
}

/// Drives shared and cyclic graphs through a promotion and a fullsweep under **both**
/// mature strategies: after every collection each root is intact, sharing is preserved and
/// every cycle stays closed.
#[test]
fn shared_and_cyclic_graphs_survive_both_strategies() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let config = GcConfig::copying(policy(1, 4096, 8192)).with_mature(kind);
        let mut collector = AnyCollector::from_config(&config).expect("collector");

        let mut roots: Vec<Eterm> = Vec::new();
        let mut corpora: Vec<Corpus> = Vec::new();

        for round in 0..4i64 {
            let base = round * 100;

            // A `[A, B]` list sharing the subterm `S`.
            let shared = {
                let heap = collector.young_mut().heap_mut();
                let s = heap
                    .tuple(&[make_small(base), make_small(base + 1)])
                    .expect("S");
                let a = heap.tuple(&[s, make_small(base + 10)]).expect("A");
                let b = heap.tuple(&[s, make_small(base + 20)]).expect("B");
                let tail = heap.cons(b, NIL).expect("tail");
                heap.cons(a, tail).expect("shared root")
            };
            roots.push(shared);
            corpora.push(Corpus::Shared(base));

            // A two-cell hand-made cycle.
            let cycle = {
                let heap = collector.young_mut().heap_mut();
                // SAFETY: two 2-word cons cells are reserved and fully written; each cell's
                // cdr is the other's LIST-tagged pointer (a deliberate cycle).
                unsafe {
                    let px = heap.alloc_words(2).expect("cell X");
                    let py = heap.alloc_words(2).expect("cell Y");
                    *px = make_small(1);
                    *px.add(1) = make_list(py as *const Eterm);
                    *py = make_small(2);
                    *py.add(1) = make_list(px as *const Eterm);
                    make_list(px as *const Eterm)
                }
            };
            roots.push(cycle);
            corpora.push(Corpus::Cycle);

            collector.collect(&mut roots).expect("collect");
            assert_eq!(
                collector.young().used_words(),
                0,
                "{kind:?}: nursery empty after round {round}"
            );
            for (root, corpus) in roots.iter().zip(&corpora) {
                assert!(
                    in_mature(&collector, *root),
                    "{kind:?}: root moved into mature"
                );
                assert_corpus(*root, *corpus);
            }
        }
    }
}
