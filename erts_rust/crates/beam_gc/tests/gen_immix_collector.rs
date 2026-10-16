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

//! End-to-end evidence for the config-driven collector seam ([`AnyCollector`]).
//!
//! Drives the **same** corpus through the copying and the GenImmix strategies behind one
//! public API and asserts the observable result is identical: the sequence of collection
//! kinds, the emptied nursery, every root relocated out of the nursery into the mature
//! generation, and the live graph reachable from each root structurally intact. This is
//! exactly what `beam_process` sees when it drives a collection at a safepoint.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that
//! a collection just wrote); every block carries a `// SAFETY:` rationale.

use beam_gc::prelude::{
    lines_for, AnyCollector, CollectionKind, GcConfig, GenImmixCollector, HeapPolicy, MatureKind,
    BLOCK_WORDS, LINES_PER_BLOCK,
};
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, is_atom, is_list, is_small, list_val, make_atom,
    make_small, signed_value, Atom, Eterm, NIL,
};

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
fn build_graph(c: &mut AnyCollector) -> (Eterm, Eterm) {
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

/// Whether `term` is resident in the selected strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// Asserts the graph reachable from `root` has the shape/leaf values of `{atom,[1,2,3],{4,5}}`.
fn assert_graph(c: &AnyCollector, root: Eterm, atom: Eterm) {
    // SAFETY: `root` is a live 3-element tuple.
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
    assert!(in_mature(c, e1), "the list must be resident in mature");
    assert_eq!(list_to_smalls(e1), vec![1, 2, 3]);

    assert!(
        in_mature(c, e2),
        "the inner tuple must be resident in mature"
    );
    // SAFETY: `e2` is a live 2-element tuple.
    let inner_header = unsafe { *boxed_val(e2) };
    assert!(is_arity_value(inner_header));
    assert_eq!(arityval(inner_header), 2);
    // SAFETY: elements 0..2 lie within that tuple.
    assert_eq!(signed_value(unsafe { *boxed_val(e2).add(1) }), 4);
    assert_eq!(signed_value(unsafe { *boxed_val(e2).add(2) }), 5);
}

/// Promotes three successive graphs through `kind`'s collector (two minors then a major),
/// asserting the live graph is intact after every collection; returns the collection kinds.
fn drive(kind: MatureKind) -> Vec<CollectionKind> {
    let policy = HeapPolicy {
        fullsweep_after: 2,
        initial_mature_words: 512,
        ..HeapPolicy::default()
    };
    let config = GcConfig::copying(policy).with_mature(kind);
    let mut c = AnyCollector::from_config(&config).expect("collector");
    assert_eq!(c.kind(), kind, "the config selected the requested strategy");

    let mut kinds = Vec::new();
    let mut roots: Vec<Eterm> = Vec::new();
    let mut atoms: Vec<Eterm> = Vec::new();

    for round in 0..3 {
        let (outer, atom) = build_graph(&mut c);
        roots.push(outer);
        atoms.push(atom);

        let collection = c.collect(&mut roots).expect("collect");
        kinds.push(collection.kind);
        assert_eq!(c.young().used_words(), 0, "the nursery is emptied");

        for (root, atom) in roots.iter().zip(&atoms) {
            assert!(
                !c.young().contains_term(*root),
                "no root may stay in the nursery after round {round}"
            );
            assert!(
                in_mature(&c, *root),
                "every root must be relocated into the mature generation"
            );
            assert_graph(&c, *root, *atom);
        }
    }

    kinds
}

#[test]
fn the_same_corpus_stays_intact_under_both_strategies() {
    let copying = drive(MatureKind::Copying);
    let immix = drive(MatureKind::Immix);

    // The GenImmix mature space selects the same collections as the copying one, and the
    // corpus is young for the first two rounds (two minors) and due a fullsweep on the
    // third (a major) — so the escalating path is exercised for both strategies.
    assert_eq!(
        copying,
        vec![
            CollectionKind::Minor,
            CollectionKind::Minor,
            CollectionKind::Major
        ]
    );
    assert_eq!(
        copying, immix,
        "both strategies pick the same collection kinds"
    );
}

#[test]
fn default_config_selects_the_copying_path() {
    let any = AnyCollector::from_config(&GcConfig::default()).expect("collector");
    assert_eq!(any.kind(), MatureKind::Copying);
    assert!(matches!(any, AnyCollector::Copying(_)));
    assert_eq!(any.policy(), &HeapPolicy::default());
}

// ---------------------------------------------------------------------------
// Multi-block ("large") objects through the promotion path
// ---------------------------------------------------------------------------
//
// `ImmixSpace` gives a request larger than one block a run of whole, block-aligned blocks
// (`find_run`/`alloc_words`), which makes the first block of the run fully covered by the
// object's own marks and therefore `AllMarked`/pinned. The space-level rule is pinned by
// `immix::tests::an_object_larger_than_a_block_claims_a_run_of_blocks_and_is_pinned`; the
// tests below pin the *collector* half of the same path: promoting such an object out of
// the nursery through `PromoteDest::reserve`, and its fate in a following major's in-place
// reclaim.

/// Element count of the multi-block tuple: `BLOCK_WORDS` elements occupy `BLOCK_WORDS + 1`
/// words, i.e. 17 lines - one whole line more than a block, so it needs a two-block run.
const LARGE_ARITY: usize = BLOCK_WORDS;

/// The words the [`LARGE_ARITY`]-element tuple occupies (its header plus its elements).
const LARGE_WORDS: usize = LARGE_ARITY + 1;

/// Element count of the second multi-block fixture: `2 * BLOCK_WORDS + 1` words, i.e. 33
/// lines - a three-block run, a *different* run length from [`LARGE_ARITY`].
const WIDE_ARITY: usize = 2 * BLOCK_WORDS;

/// The words the [`WIDE_ARITY`]-element tuple occupies.
const WIDE_WORDS: usize = WIDE_ARITY + 1;

/// The whole blocks `lines_for(words)` lines need - `find_run`'s `k` for the request.
fn run_blocks(words: usize) -> usize {
    lines_for(words).div_ceil(LINES_PER_BLOCK)
}

/// A policy whose nursery holds the fixtures and whose mature region is whole blocks (so a
/// multi-block run has room), with a long fullsweep cadence so each test picks the
/// collection it wants to drive.
fn large_policy() -> HeapPolicy {
    HeapPolicy {
        initial_young_words: 4096,
        initial_mature_words: 4 * BLOCK_WORDS,
        fullsweep_after: 65535,
        ..HeapPolicy::default()
    }
}

/// Builds a tuple of `arity` elements (element `i` is the small integer `i`) in the nursery.
fn build_tuple(c: &mut GenImmixCollector, arity: usize) -> Eterm {
    let elems: Vec<Eterm> = (0..arity).map(|i| make_small(i as i64)).collect();
    c.young_mut()
        .heap_mut()
        .tuple(&elems)
        .expect("the tuple fits the nursery")
}

/// Asserts `root` is a multi-block object resident in the mature region: an `arity`-element
/// tuple, on a block boundary (the whole-block run `find_run` gives it), whose element `i`
/// is the small integer `i`.
fn assert_multi_block(c: &GenImmixCollector, root: Eterm, arity: usize) {
    assert!(
        c.mature().contains_term(root),
        "the multi-block object is resident in the mature region"
    );
    // SAFETY: `contains_term` established that `root` is a boxed/list term pointing into
    // this space; the header check below establishes it is a live tuple.
    let header = unsafe { *boxed_val(root) };
    assert!(is_arity_value(header), "the object is a tuple");
    assert_eq!(arityval(header), arity, "the tuple keeps its arity");
    // SAFETY: elements 0..arity follow the validated header of a live tuple.
    for i in 0..arity {
        let elem = unsafe { *boxed_val(root).add(1 + i) };
        assert!(is_small(elem), "element {i} is an immediate");
        assert_eq!(signed_value(elem), i as i64, "element {i} survives");
    }
    // A request larger than a block always claims a whole run of blocks, so the object
    // starts on a block boundary (this is also what makes its first block fully marked).
    let base = c.mature().base() as usize;
    let offset = (boxed_val(root) as usize - base) / core::mem::size_of::<Eterm>();
    assert_eq!(
        offset % BLOCK_WORDS,
        0,
        "a multi-block object starts on a block boundary"
    );
}

/// Asserts the live cons cell `root` holds `[value, NIL]` and lives in the mature region.
fn assert_small_cell(c: &GenImmixCollector, root: Eterm, value: i64) {
    assert!(
        c.mature().contains_term(root),
        "the small object is resident in the mature region"
    );
    let cell = list_val(root);
    // SAFETY: `root` is a live cons cell (validated by `contains_term`/`list_val`).
    assert_eq!(signed_value(unsafe { *cell }), value);
    // SAFETY: the second word of a read cons cell.
    assert_eq!(unsafe { *cell.add(1) }, NIL);
}

#[test]
fn a_multi_block_object_promotes_through_a_minor_and_survives_a_major() {
    assert_eq!(
        run_blocks(LARGE_WORDS),
        2,
        "the fixture needs a two-block run"
    );
    let mut c = GenImmixCollector::from_policy(large_policy()).expect("collector");

    let large = build_tuple(&mut c, LARGE_ARITY);
    assert_eq!(
        c.young().used_words(),
        LARGE_WORDS,
        "only the fixture is live"
    );

    let mut roots = [large];
    let minor = c
        .collect_minor(&mut roots)
        .expect("the run fits the mature region");

    // One object of exactly `LARGE_WORDS` words was promoted, into a line-rounded run.
    assert_eq!(minor.objects_copied, 1);
    assert_eq!(minor.words_copied, LARGE_WORDS);
    assert_eq!(c.young().used_words(), 0, "the nursery is emptied");
    assert_eq!(c.mature().used_lines(), lines_for(LARGE_WORDS));
    assert_multi_block(&c, roots[0], LARGE_ARITY);

    let promoted = roots[0];

    // The following fullsweep reclaims the mature generation in place. The fixture's start
    // block is wholly covered by its own marks, so it is `AllMarked`: pinned in its run,
    // never evacuated, and its root keeps its address.
    let major = c
        .collect_major(&mut roots)
        .expect("the fullsweep keeps the fixture");
    assert_eq!(
        roots[0].raw(),
        promoted.raw(),
        "a pinned multi-block object keeps its address"
    );
    assert_multi_block(&c, roots[0], LARGE_ARITY);
    assert_eq!(c.mature().used_lines(), lines_for(LARGE_WORDS));

    let immix = c.last_immix_stats().expect("the in-place reclaim ran");
    assert_eq!(
        immix.objects_pinned, 1,
        "the fixture is pinned, not evacuated"
    );
    assert_eq!(immix.objects_evacuated, 0);
    assert_eq!(immix.lines_pinned, lines_for(LARGE_WORDS));
    assert_eq!(immix.words_pinned, LARGE_WORDS);
    assert_eq!(
        major.words_reclaimed, 0,
        "every line of the run is retained"
    );
}

#[test]
fn a_multi_block_object_promotes_directly_in_a_major() {
    // `fullsweep_after = 0` makes the very first collection a fullsweep: the fixture is
    // promoted by the major's promotion half, without a minor ever running.
    let policy = HeapPolicy {
        fullsweep_after: 0,
        ..large_policy()
    };
    let mut c = GenImmixCollector::from_policy(policy).expect("collector");

    let large = build_tuple(&mut c, LARGE_ARITY);
    assert!(
        !c.mature().contains_term(large),
        "the fixture is built in the nursery"
    );
    let mut roots = [large];

    let collection = c.collect(&mut roots).expect("the fullsweep promotes it");
    assert_eq!(
        collection.kind,
        CollectionKind::Major,
        "fullsweep_after = 0"
    );
    assert_eq!(collection.stats.objects_copied, 1);
    assert_eq!(collection.stats.words_copied, LARGE_WORDS);
    assert_eq!(c.young().used_words(), 0, "the nursery is emptied");
    assert_multi_block(&c, roots[0], LARGE_ARITY);

    // And the following fullsweep keeps it in place.
    let pinned = roots[0];
    c.collect_major(&mut roots)
        .expect("the fullsweep keeps the fixture");
    assert_eq!(roots[0].raw(), pinned.raw());
    assert_multi_block(&c, roots[0], LARGE_ARITY);
}

#[test]
fn a_multi_block_object_survives_alongside_small_objects_and_garbage() {
    let mut c = GenImmixCollector::from_policy(large_policy()).expect("collector");

    // The multi-block fixture first, then 20 live cons cells, then unreferenced garbage.
    let large = build_tuple(&mut c, LARGE_ARITY);
    let mut roots: Vec<Eterm> = vec![large];
    for i in 0..20i64 {
        let cell = c
            .young_mut()
            .heap_mut()
            .cons(make_small(i), NIL)
            .expect("live cell");
        roots.push(cell);
    }
    for i in 0..8i64 {
        c.young_mut()
            .heap_mut()
            .cons(make_small(i), make_small(i))
            .expect("garbage cell");
    }

    let minor = c.collect_minor(&mut roots).expect("the promotion fits");
    assert_eq!(
        minor.objects_copied, 21,
        "the fixture plus the 20 live cells"
    );
    assert_eq!(minor.words_copied, LARGE_WORDS + 20 * 2);
    assert_eq!(
        c.mature().used_lines(),
        lines_for(LARGE_WORDS) + 20,
        "only live objects claim mature lines"
    );
    assert_multi_block(&c, roots[0], LARGE_ARITY);
    for (i, &cell) in roots[1..21].iter().enumerate() {
        assert_small_cell(&c, cell, i as i64);
    }

    // A fresh round of nursery allocation (one live cell plus garbage) drives the next
    // fullsweep: it reclaims the mature generation in place - where the fixture's run
    // interacts with the line-granular small objects - and then promotes the new round.
    let promoted = roots[0];
    let extra = c
        .young_mut()
        .heap_mut()
        .cons(make_small(99), NIL)
        .expect("extra cell");
    c.young_mut()
        .heap_mut()
        .cons(make_small(7), NIL)
        .expect("garbage cell");
    roots.push(extra);

    let major = c
        .collect_major(&mut roots)
        .expect("the fullsweep keeps every live object");
    assert_eq!(
        major.objects_copied, 1,
        "the fresh round's live cell is promoted"
    );
    assert_eq!(
        roots[0].raw(),
        promoted.raw(),
        "the pinned fixture keeps its address"
    );
    assert_multi_block(&c, roots[0], LARGE_ARITY);
    for (i, &cell) in roots[1..21].iter().enumerate() {
        assert_small_cell(&c, cell, i as i64);
    }
    assert_small_cell(&c, roots[21], 99);
    assert_eq!(c.young().used_words(), 0, "the nursery is emptied");
    assert_eq!(
        c.mature().used_lines(),
        lines_for(LARGE_WORDS) + 20 + 1,
        "the live set is unchanged (plus the fresh cell)"
    );
}

#[test]
fn two_multi_block_objects_of_different_run_sizes_share_the_run_allocator() {
    assert_eq!(run_blocks(LARGE_WORDS), 2);
    assert_eq!(run_blocks(WIDE_WORDS), 3);
    let policy = HeapPolicy {
        initial_mature_words: 8 * BLOCK_WORDS,
        ..large_policy()
    };
    let mut c = GenImmixCollector::from_policy(policy).expect("collector");

    // A small object first, then a two-block run and a three-block run, so the run search
    // has to skip blocks that hold free *lines* but are not wholly free. `roots` order is
    // the promotion order, so the placement below is deterministic.
    let first = c
        .young_mut()
        .heap_mut()
        .cons(make_small(0), NIL)
        .expect("live cell");
    let narrow = build_tuple(&mut c, LARGE_ARITY);
    let wide = build_tuple(&mut c, WIDE_ARITY);
    let mut roots: Vec<Eterm> = vec![first, narrow, wide];
    for i in 1..4i64 {
        let cell = c
            .young_mut()
            .heap_mut()
            .cons(make_small(i), NIL)
            .expect("live cell");
        roots.push(cell);
    }

    c.collect_minor(&mut roots).expect("both runs fit");
    assert_multi_block(&c, roots[1], LARGE_ARITY);
    assert_multi_block(&c, roots[2], WIDE_ARITY);

    let base = c.mature().base() as usize;
    let offset = |term: Eterm| (boxed_val(term) as usize - base) / core::mem::size_of::<Eterm>();
    assert_eq!(
        offset(roots[0]),
        0,
        "the small cell takes the region's first line"
    );
    assert_eq!(
        offset(roots[1]),
        BLOCK_WORDS,
        "the two-block run skips block 0 (partly used, so not a whole free block)"
    );
    assert_eq!(
        offset(roots[2]),
        3 * BLOCK_WORDS,
        "the three-block run skips both earlier blocks"
    );
    for (i, &cell) in roots[3..6].iter().enumerate() {
        assert_small_cell(&c, cell, i as i64 + 1);
    }

    // A fullsweep pins both runs (each start block is wholly covered by its own marks), so
    // neither fixture moves and both keep their content; the small cells are reclaimed
    // around them.
    let before: Vec<Eterm> = roots.clone();
    c.collect_major(&mut roots)
        .expect("the fullsweep keeps both runs");
    assert_eq!(roots[1].raw(), before[1].raw());
    assert_eq!(roots[2].raw(), before[2].raw());
    assert_multi_block(&c, roots[1], LARGE_ARITY);
    assert_multi_block(&c, roots[2], WIDE_ARITY);
    assert_small_cell(&c, roots[0], 0);
    for (i, &cell) in roots[3..6].iter().enumerate() {
        assert_small_cell(&c, cell, i as i64 + 1);
    }
    let immix = c.last_immix_stats().expect("the in-place reclaim ran");
    assert_eq!(
        immix.objects_pinned, 2,
        "both multi-block fixtures are pinned"
    );
    assert_eq!(
        immix.lines_pinned,
        lines_for(LARGE_WORDS) + lines_for(WIDE_WORDS)
    );
}
