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

//! The **measured gate** for the GenImmix mature generation: does replacing the copying
//! mature space with the mark-region [`ImmixSpace`] actually save copying work on the
//! workload the option was designed for — a **long-lived** data set with heavy garbage?
//!
//! `ERTS_RUST_REWRITE_DEEPDIVE.md` §D/§H makes GenImmix a *measurement-gated* option, so the
//! claim has to be measured with the crate's own counters rather than asserted:
//!
//! * the **copying** path reads [`GcStats`] of a forced fullsweep — `objects_copied` /
//!   `words_copied` are exactly the *whole* live set, because a fullsweep re-copies the
//!   mature generation into a fresh space;
//! * the **Immix** path reads the same [`GcStats`] (which, for a mature reclaim, counts only
//!   the nursery promotion) *and* [`GenImmixCollector::last_immix_stats`] — the
//!   evacuated-vs-pinned split of the in-place reclaim.
//!
//! The corpus is deliberately the favourable one: 128 one-line long-lived tuples (exactly
//! eight full Immix blocks) survive to the mature generation, 16 transient tuples are
//! promoted and then dropped (one further full block), 8 more are promoted and kept (half a
//! block), and the nursery is then filled with 300 garbage tuples before a fullsweep. In
//! that shape every full live block is `AllMarked`, so the long-lived set is pinned in place
//! while the copying path must move all of it again.
//!
//! Verified outcome (run with `-- --nocapture` to see the table): of the 136 live mature
//! objects, the copying path copies **136 objects / 400 words**, whereas the Immix path
//! evacuates **8 objects (8 lines, 128 line-words)** and **pins 128 objects / 384 words**,
//! reclaiming one block wholesale and freeing 16 mature lines. A caveat is recorded
//! honestly: the mark-region space's resident footprint is line-padded (`used_words` is
//! whole lines), so 400 words of live data occupy 136 *lines* = 2176 line-words there — the
//! internal fragmentation the mark-region trade buys the cheaper reclamation with
//! (`src/immix/mod.rs`, "Allocation is line-granular").
//!
//! Deterministic: fixed sizes, no timing and no randomness anywhere.

use beam_gc::prelude::{
    AnyCollector, GcConfig, GcStats, HeapPolicy, ImmixStats, MatureKind, LINE_WORDS,
};
use beam_term::prelude::{arityval, boxed_val, is_arity_value, make_small, signed_value, Eterm};

// ---------------------------------------------------------------------------
// The workload
// ---------------------------------------------------------------------------

/// Long-lived objects: one line each, so 128 of them fill exactly eight 16-line blocks.
const LONG_LIVED: usize = 128;
/// Transients promoted after the long-lived set and then dropped (`DROPPED` of them) or kept
/// (`KEPT` of them) — one full block plus half of the next.
const TRANSIENTS: usize = 24;
const DROPPED: usize = 16;
const KEPT: usize = TRANSIENTS - DROPPED;
/// Short-lived garbage allocated in the nursery just before the fullsweep.
const GARBAGE: usize = 300;

/// Both strategies get **identical** sizes, and the fullsweep cadence is disabled
/// (`fullsweep_after = 65535`) so every collection in this file is the explicit one the
/// test asked for — the measurement must not depend on which collection the policy picked.
fn policy() -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: 65535,
        initial_young_words: 8192,
        initial_mature_words: 8192,
        min_heap_words: 8192,
        ..HeapPolicy::default()
    }
}

/// The live words in the mature generation, in the strategy's own accounting (whole lines
/// for the mark-region space).
fn mature_words(c: &AnyCollector) -> usize {
    match c {
        AnyCollector::Copying(x) => x.mature().used_words(),
        AnyCollector::Immix(x) => x.mature().used_words(),
    }
}

/// The measured result of one forced fullsweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Measurement {
    kind: MatureKind,
    /// `GcStats` of the forced fullsweep: what the collection copied.
    stats: GcStats,
    /// The mark-region reclaim's own accounting (`None` for the copying strategy).
    immix: Option<ImmixStats>,
    /// The mature generation's resident footprint after the fullsweep.
    mature_words: usize,
    /// Words that died in the nursery during the fullsweep (never copied).
    nursery_garbage: usize,
}

impl Measurement {
    /// Objects this strategy actually *moved* during the fullsweep: the copying path's
    /// copies, or the mark-region reclaim's evacuations.
    fn objects_moved(&self) -> usize {
        match self.immix {
            Some(immix) => immix.objects_evacuated,
            None => self.stats.objects_copied,
        }
    }

    /// Words this strategy actually *moved* (for the mark-region reclaim, the line-rounded
    /// figure its own counters report).
    fn words_moved(&self) -> usize {
        match self.immix {
            Some(immix) => immix.lines_evacuated * LINE_WORDS,
            None => self.stats.words_copied,
        }
    }

    /// Objects the strategy left where they were.
    fn objects_left_in_place(&self) -> usize {
        match self.immix {
            Some(immix) => immix.objects_pinned,
            None => 0,
        }
    }

    fn report(&self) -> String {
        let immix = self
            .immix
            .map(|i| {
                format!(
                    "evacuated {} obj / {} lines / {} line-words, pinned {} obj / {} lines / {} words, \
                     blocks_reclaimed {}",
                    i.objects_evacuated,
                    i.lines_evacuated,
                    i.lines_evacuated * LINE_WORDS,
                    i.objects_pinned,
                    i.lines_pinned,
                    i.words_pinned,
                    i.blocks_reclaimed
                )
            })
            .unwrap_or_else(|| "n/a (copying strategy)".to_string());
        format!(
            "{:?}: copied {} obj / {} words, made {} words available, mature footprint {} words, \
             nursery garbage {} words | {}",
            self.kind,
            self.stats.objects_copied,
            self.stats.words_copied,
            self.stats.words_reclaimed,
            self.mature_words,
            self.nursery_garbage,
            immix
        )
    }
}

/// Drives the workload against `kind` and measures the forced fullsweep. Returns the
/// measurement plus the **contents** of the surviving roots, so the caller can also check
/// that the data actually survived the collection it measured. The contents are read out
/// while the collector is still alive: the roots are raw `Eterm` pointers into the
/// collector's own spaces, so they would dangle once it is dropped (they are not kept alive
/// by the `Vec` that holds them).
fn measure(kind: MatureKind) -> (Measurement, Vec<Vec<i64>>) {
    let mut c = AnyCollector::from_config(&GcConfig::copying(policy()).with_mature(kind))
        .expect("collector");
    assert_eq!(c.kind(), kind);

    // ---- phase 1: the long-lived set survives to the mature generation ----------------
    let mut roots: Vec<Eterm> = Vec::new();
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..LONG_LIVED as i64 {
            roots.push(
                heap.tuple(&[make_small(i), make_small(2 * i)])
                    .expect("long-lived tuple"),
            );
        }
    }
    match &mut c {
        AnyCollector::Copying(x) => x.collect_major(&mut roots),
        AnyCollector::Immix(x) => x.collect_major(&mut roots),
    }
    .expect("promote the long-lived set");
    assert!(
        roots.iter().all(|r| !c.young().contains_term(*r)),
        "the long-lived set is mature after the first fullsweep"
    );

    // ---- phase 2: transients promoted into the mature generation, then mostly dropped --
    let mut transients: Vec<Eterm> = Vec::new();
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..TRANSIENTS as i64 {
            transients.push(
                heap.tuple(&[make_small(1000 + i)])
                    .expect("transient tuple"),
            );
        }
    }
    roots.extend(transients.iter().copied());
    match &mut c {
        AnyCollector::Copying(x) => x.collect_minor(&mut roots),
        AnyCollector::Immix(x) => x.collect_minor(&mut roots),
    }
    .expect("promote the transients");
    // Only the last `KEPT` transients stay reachable: the earlier `DROPPED` ones are now
    // mature garbage the fullsweep has to get rid of.
    roots.drain(LONG_LIVED..LONG_LIVED + DROPPED);
    assert_eq!(roots.len(), LONG_LIVED + KEPT);
    // The corpus's block layout assumption, made explicit: the promotion lands the
    // long-lived set plus all `TRANSIENTS` transients in the mature generation — eight full
    // 16-line blocks plus one full and one half-full block — which is *why* the reclaim
    // below sees full blocks. (The copying space just holds their exact word counts.)
    let mature_before = mature_words(&c);
    let expected_mature = match kind {
        MatureKind::Copying => LONG_LIVED * 3 + TRANSIENTS * 2,
        MatureKind::Immix => (LONG_LIVED + TRANSIENTS) * LINE_WORDS,
    };
    assert_eq!(
        mature_before, expected_mature,
        "the workload promotes into the mature generation as the corpus assumes"
    );

    // ---- phase 3: heavy nursery garbage, then the measured fullsweep -------------------
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..GARBAGE as i64 {
            let _ = heap
                .tuple(&[make_small(2000 + i), make_small(i)])
                .expect("garbage tuple");
        }
    }
    let nursery_garbage = c.young().used_words();

    let (stats, immix) = match &mut c {
        AnyCollector::Copying(x) => (
            x.collect_major(&mut roots).expect("measured fullsweep"),
            None,
        ),
        AnyCollector::Immix(x) => {
            let stats = x.collect_major(&mut roots).expect("measured fullsweep");
            (stats, x.last_immix_stats())
        }
    };

    let measurement = Measurement {
        kind,
        stats,
        immix,
        mature_words: mature_words(&c),
        nursery_garbage,
    };
    // The nursery never paid for the garbage.
    assert_eq!(
        c.young().used_words(),
        0,
        "the nursery is empty after the fullsweep"
    );
    assert!(c.young().available_words() > 0);
    // Read the survivors' contents out of the collector's spaces *now*, before `c` drops.
    let observed: Vec<Vec<i64>> = roots.iter().map(|r| tuple_to_smalls(*r)).collect();
    (measurement, observed)
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

#[test]
fn the_mark_region_mature_path_moves_far_less_than_the_copying_path() {
    let (copying, copying_roots) = measure(MatureKind::Copying);
    let (immix, immix_roots) = measure(MatureKind::Immix);

    // --- the numbers, so the gate is real and reviewable --------------------------------
    eprintln!("GenImmix measured gate (fullsweep over a long-lived mature set)");
    eprintln!("  {}", copying.report());
    eprintln!("  {}", immix.report());
    eprintln!(
        "  copying moved {} obj / {} words; immix moved {} obj / {} words and left {} obj in place",
        copying.objects_moved(),
        copying.words_moved(),
        immix.objects_moved(),
        immix.words_moved(),
        immix.objects_left_in_place()
    );

    // --- the copying path is the wholesale baseline ------------------------------------
    // A fullsweep re-copies the entire live set: the 128 long-lived tuples (3 words each)
    // plus the 8 kept transients (2 words each).
    assert_eq!(copying.stats.objects_copied, LONG_LIVED + KEPT);
    assert_eq!(
        copying.stats.words_copied,
        LONG_LIVED * 3 + KEPT * 2,
        "the copying fullsweep moves every live word"
    );
    assert_eq!(
        copying.stats.words_reclaimed,
        (copying.nursery_garbage + LONG_LIVED * 3 + TRANSIENTS * 2) - copying.stats.words_copied,
        "the copying fullsweep also reclaims the garbage it refuses to copy"
    );

    // --- the Immix path is genuinely cheaper on this shape -----------------------------
    let immix_stats = immix.immix.expect("an Immix fullsweep reports its reclaim");
    assert_eq!(
        immix_stats.objects_evacuated + immix_stats.objects_pinned,
        copying.stats.objects_copied,
        "both strategies must find the same live mature set"
    );
    assert_eq!(
        immix_stats.objects_pinned, LONG_LIVED,
        "every full live block is pinned, so the long-lived set never moves"
    );
    assert_eq!(immix_stats.lines_pinned, LONG_LIVED);
    assert_eq!(immix_stats.words_pinned, LONG_LIVED * 3);
    assert_eq!(
        immix_stats.objects_evacuated, KEPT,
        "only the half-full block's live objects are evacuated"
    );
    assert_eq!(
        immix_stats.blocks_reclaimed, 1,
        "the half-full block whose live objects move out is reclaimed wholesale; the fully \
         dead block's lines are reused as the evacuation destination, so it is reclaimed \
         line by line and is not counted as a whole block"
    );
    assert!(
        immix.objects_moved() < copying.objects_moved(),
        "the mark-region path must move strictly fewer objects: {} vs {}",
        immix.objects_moved(),
        copying.objects_moved()
    );
    assert_eq!(immix.objects_moved(), KEPT);
    assert!(
        immix.words_moved() < copying.words_moved(),
        "and strictly fewer words: {} vs {}",
        immix.words_moved(),
        copying.words_moved()
    );
    // The mark-region path reclaims the mature garbage *without* copying the survivors, so
    // its reclaim is a real number too (mature lines freed + the nursery garbage).
    assert!(immix_stats.words_reclaimed > 0);
    assert!(
        immix.stats.words_reclaimed >= immix_stats.words_reclaimed,
        "the fullsweep's reclaimed words include the in-place mature reclaim"
    );
    // The nursery garbage is identical for both strategies — it is the same corpus.
    assert_eq!(immix.nursery_garbage, copying.nursery_garbage);
    assert_eq!(immix.nursery_garbage, GARBAGE * 3);

    // --- both paths produced the same, live result ------------------------------------
    assert_eq!(copying_roots.len(), LONG_LIVED + KEPT);
    assert_eq!(immix_roots.len(), LONG_LIVED + KEPT);
    for (i, (c_root, i_root)) in copying_roots.iter().zip(&immix_roots).enumerate() {
        assert_eq!(
            c_root, i_root,
            "root {i} has the same contents under both strategies"
        );
    }
    let expected_long_last: Vec<i64> = vec![LONG_LIVED as i64 - 1, 2 * (LONG_LIVED as i64 - 1)];
    assert_eq!(copying_roots[LONG_LIVED - 1], expected_long_last);
    assert_eq!(immix_roots[LONG_LIVED - 1], expected_long_last);
    // The kept transients (`1000 + TRANSIENTS - KEPT .. 1000 + TRANSIENTS - 1`) survive too.
    for k in 0..KEPT {
        let value = 1000 + (DROPPED as i64 + k as i64);
        assert_eq!(copying_roots[LONG_LIVED + k], vec![value]);
        assert_eq!(immix_roots[LONG_LIVED + k], vec![value]);
    }
    // And the dropped transients' values are gone from the graph.
    let immix_graph: String = immix_roots.iter().map(|r| format!("{r:?}")).collect();
    for dropped in [1000, 1000 + (DROPPED as i64) - 1] {
        assert!(
            !immix_graph.contains(&format!("[{dropped}]")),
            "the dropped transient {dropped} is reclaimed, not retained"
        );
    }

    // --- honest caveat: the mark-region space pays internal fragmentation ---------------
    // The line-granular allocator hands out whole 16-word lines, so the same 400 live words
    // occupy far more *resident* storage than the copying space's exact bump pointer. That
    // is the documented trade (`src/immix/mod.rs`), and it is printed rather than hidden.
    eprintln!(
        "  resident mature footprint: copying {} words vs immix {} line-words for {} live words",
        copying.mature_words,
        immix.mature_words,
        LONG_LIVED * 3 + KEPT * 2
    );
    assert!(
        immix.mature_words >= LONG_LIVED * 3 + KEPT * 2,
        "line-rounded occupancy can only over-state the live words"
    );
}

/// The small integers of a tuple `{...}`, in order.
fn tuple_to_smalls(term: Eterm) -> Vec<i64> {
    // SAFETY: `term` is a live boxed term this test (or a collection) built.
    let header = unsafe { *boxed_val(term) };
    assert!(is_arity_value(header), "not a tuple header");
    let mut values = Vec::with_capacity(arityval(header));
    for i in 0..arityval(header) {
        // SAFETY: elements `1..=n` lie inside the `n`-element tuple just validated.
        values.push(signed_value(unsafe { *boxed_val(term).add(1 + i) }));
    }
    values
}
