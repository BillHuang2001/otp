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

//! Direct tests of [`GenImmixCollector::grow_major`] — the **growth fallback** a fullsweep
//! takes when its in-place mature reclaim leaves fewer free lines than the nursery's
//! promotion needs.
//!
//! The public-API half of this contract — the capped corpus that reports the typed heap
//! exhaustion and the growable one that collects — lives in `tests/immix_major_growth.rs`.
//! Reaching the fallback itself needs the collector's private state: the in-place reclaim
//! plus the nursery-reference rewrite that precedes it (both reproduced here exactly as
//! `run_major` performs them), and `grow_major` itself. Under the current mature-selection
//! rule the fallback is a defensive net — a policy that keeps the same capacity keeps it
//! only when the promotion already fits — so the *grow* arm can only be exercised from
//! inside the module, which is what these tests do (and why they are not integration tests).
//!
//! `unsafe` below only dereferences pointers this test allocated, or that a collection just
//! wrote; every block carries a `// SAFETY:` rationale.

use beam_common::{AllocKind, BeamError};
use beam_term::prelude::{boxed_val, list_val, make_small, signed_value, Eterm, NIL};

use super::*;

/// A collector over `policy`, on the default allocator.
fn collector(policy: HeapPolicy) -> GenImmixCollector {
    GenImmixCollector::from_policy(policy).expect("collector")
}

/// The one-block mature-region policy used by both tests: `16` lines, ceiling `max`.
fn one_block(max_heap_words: usize) -> HeapPolicy {
    HeapPolicy {
        initial_mature_words: 1, // exactly one block: 16 lines
        max_heap_words,
        ..HeapPolicy::default()
    }
}

/// Promotes `n` live one-line tuples `{i, 2i}` (`i` from `from`) into the mature region and
/// returns their (now mature) terms.
fn promote_tuples(c: &mut GenImmixCollector, from: i64, n: i64) -> Vec<Eterm> {
    let mut roots = Vec::new();
    for i in 0..n {
        let a = from + i;
        roots.push(
            c.young_mut()
                .heap_mut()
                .tuple(&[make_small(a), make_small(2 * a)])
                .expect("tuple"),
        );
    }
    c.collect_minor(&mut roots)
        .expect("the base fits the one-block region");
    roots
}

/// Adds the nursery half of the corpus: seven live cons cells `[100+i | nil]` plus one
/// `[999 | <mature term>]` whose second word points **back into** the mature region — the
/// young→mature edge the fallback has to re-resolve. Returns the eight nursery roots.
fn nursery_cells(c: &mut GenImmixCollector, back_reference: Eterm) -> Vec<Eterm> {
    let mut cells = Vec::new();
    for i in 0..7i64 {
        cells.push(
            c.young_mut()
                .heap_mut()
                .cons(make_small(100 + i), NIL)
                .expect("cell"),
        );
    }
    cells.push(
        c.young_mut()
            .heap_mut()
            .cons(make_small(999), back_reference)
            .expect("holder"),
    );
    cells
}

/// Brings `c` to exactly the state `run_major` calls
/// [`GenImmixCollector::grow_major`] from: the in-place mature reclaim has run (with the
/// nursery's mature references as additional roots) and every nursery reference word into
/// the mature region has been rewritten to its final in-place address. Returns the
/// promotion's exact line count, and the exact line **and word** need of the whole live set
/// the fallback has to relocate (what `run_major` passes through to `grow_major`).
fn reach_the_in_place_shortfall(
    c: &mut GenImmixCollector,
    roots: &mut [Eterm],
) -> (usize, usize, usize) {
    let (needed, refs) = scan_young(roots, c.nursery(), &c.mature).expect("scan the nursery");
    // The exact live need, read *before* the reclaim moves anything (the counter is
    // read-only, the reclaim is not).
    let live = crate::live::live_full(c.nursery(), &c.mature, roots);
    let mut promoted: Vec<Eterm> = refs.iter().map(|(_, term)| *term).collect();
    c.mature
        .collect(roots, &mut promoted)
        .expect("the in-place reclaim never has to fail here");
    for ((slot, _), new) in refs.iter().zip(promoted.iter()) {
        // SAFETY: `slot` addresses a live reference word of a reachable nursery object
        // (yielded by `scan_young`), which the in-place reclaim left untouched.
        unsafe { **slot = *new };
    }
    (needed, live.lines, live.words)
}

#[test]
fn the_growth_fallback_grows_the_region_instead_of_failing() {
    let mut c = collector(one_block(0));
    let mature = promote_tuples(&mut c, 0, 12);
    assert_eq!(c.mature().used_lines(), 12);
    let region = c.mature().capacity_words();
    assert_eq!(region, 256, "one block");

    let mut roots = mature.clone();
    roots.extend(nursery_cells(&mut c, mature[0]));

    let (needed, live_lines, live_words) = reach_the_in_place_shortfall(&mut c, &mut roots);
    assert_eq!(needed, 8, "one line per nursery object");
    assert_eq!(
        c.mature().used_lines(),
        12,
        "the reclaim kept the live lines"
    );
    assert!(
        c.mature().free_lines() < needed,
        "the in-place region cannot hold the promotion"
    );

    let stats = c
        .grow_major(&mut roots, needed, live_lines, live_words)
        .expect("the fallback grows the region instead of failing");

    assert!(
        c.mature().capacity_words() > region,
        "the region grew from {region} to {}",
        c.mature().capacity_words()
    );
    assert_eq!(c.young().used_words(), 0, "the nursery was evacuated");
    assert_eq!(c.minor_gcs(), 0, "the fallback resets the minor counter");
    assert!(
        c.last_immix_stats().is_none(),
        "the fallback relocates, so no in-place reclaim is reported"
    );
    assert_eq!(
        stats.objects_copied, 20,
        "12 mature tuples + 8 nursery cells"
    );
    assert_eq!(
        stats.words_copied,
        12 * 3 + 8 * 2,
        "the whole live set moved exactly once"
    );

    // The 12 mature tuples round-trip, relocated into the grown region.
    for (i, root) in roots[..12].iter().enumerate() {
        assert!(
            c.mature().contains_term(*root),
            "mature root {i} was relocated"
        );
        // SAFETY: `root` is a live 2-element tuple in the mature region.
        let elements = unsafe { boxed_val(*root).add(1) };
        assert_eq!(signed_value(unsafe { *elements }), i as i64);
        assert_eq!(signed_value(unsafe { *elements.add(1) }), 2 * i as i64);
    }

    // The seven plain cons cells round-trip too.
    for (i, root) in roots[12..19].iter().enumerate() {
        assert!(c.mature().contains_term(*root), "nursery cell {i} moved");
        // SAFETY: `root` is a live cons cell (2 words) in the mature region.
        let cell = list_val(*root);
        assert_eq!(signed_value(unsafe { *cell }), 100 + i as i64);
        assert_eq!(unsafe { *cell.add(1) }, NIL);
    }

    // The backward reference: the holder moved as well, and its second word now names the
    // *relocated* mature tuple rather than the abandoned in-place address.
    let holder = roots[19];
    assert!(c.mature().contains_term(holder), "the holder was promoted");
    // SAFETY: `holder` is a live cons cell in the mature region.
    let cell = list_val(holder);
    assert_eq!(signed_value(unsafe { *cell }), 999);
    let back = unsafe { *cell.add(1) };
    assert!(
        c.mature().contains_term(back),
        "the young-to-mature edge was re-resolved into the grown region"
    );
    // SAFETY: `back` is the live 2-element mature tuple the holder points at.
    let elements = unsafe { boxed_val(back).add(1) };
    assert_eq!(signed_value(unsafe { *elements }), 0);
    assert_eq!(signed_value(unsafe { *elements.add(1) }), 0);
}

#[test]
fn the_growth_fallback_is_capped_by_max_heap_words_and_promotes_nothing() {
    // The ceiling *is* the one-block capacity, so the region may not grow.
    let mut c = collector(one_block(256));
    let mature = promote_tuples(&mut c, 0, 12);
    assert_eq!(c.mature().capacity_words(), 256);

    let mut roots = mature.clone();
    roots.extend(nursery_cells(&mut c, mature[0]));

    let (needed, live_lines, live_words) = reach_the_in_place_shortfall(&mut c, &mut roots);
    assert!(c.mature().free_lines() < needed, "the shortfall fires");

    let before = roots.clone();
    let young_used = c.young().used_words();
    let mature_lines = c.mature().used_lines();
    let minor_gcs = c.minor_gcs();

    let err = c
        .grow_major(&mut roots, needed, live_lines, live_words)
        .expect_err("the ceiling forbids growing the region");

    let failure = match err {
        BeamError::Alloc(failure) => failure,
        other => panic!("expected a typed BeamError::Alloc, got {other:?}"),
    };
    assert_eq!(failure.kind, AllocKind::Heap, "the allocation class");
    assert_eq!(
        failure.requested,
        needed * LINE_WORDS * core::mem::size_of::<Eterm>(),
        "the exhaustion reports the promotion's {needed} lines"
    );

    // Nothing was moved: no larger region was built, the mature region is untouched, the
    // nursery is un-forwarded and not one root word was rewritten.
    assert_eq!(
        c.mature().capacity_words(),
        256,
        "no fresh region was created"
    );
    assert_eq!(
        c.mature().used_lines(),
        mature_lines,
        "the region is unchanged"
    );
    assert_eq!(
        c.young().used_words(),
        young_used,
        "the nursery is unchanged"
    );
    assert_eq!(roots, before, "no root word was rewritten");
    assert!(
        roots[12..].iter().all(|r| c.young().contains_term(*r)),
        "the nursery objects were not promoted"
    );
    assert_eq!(
        c.minor_gcs(),
        minor_gcs,
        "the failed fallback resets nothing"
    );
}
