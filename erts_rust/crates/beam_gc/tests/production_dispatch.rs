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

//! **Production dispatch** — [`AnyCollector`] as a drop-in for the concrete
//! [`Collector`](beam_gc::Collector).
//!
//! The host `beam_process::Process` drives a concrete `Collector`: it owns one, exposes it,
//! and calls `should_collect` / `collect` / `ensure_capacity` / `young()` / `young_mut()` /
//! `policy()` / `minor_gcs()` / `mature()` at its heap-admission point. This file is the
//! evidence that the **strategy-dispatching** seam supports the same call set — so switching
//! the host to `AnyCollector` is a one-line change, and the measurement-gated mark-region
//! mature generation (measured in `tests/immix_break_even.rs`, ruled in `src/selection.rs`)
//! becomes reachable from the production path.
//!
//! Three things are proven:
//!
//! 1. **Observational equivalence.** One process-shaped workload — allocate on `young_mut()`,
//!    consult `should_collect`, `collect`, `ensure_capacity` — is driven through **both**
//!    strategy variants **in lockstep**, comparing after every collection. The two runs ask
//!    the same admission questions, run the same collection kinds on the same cadence, and
//!    hold a structurally identical live graph (`beam_term::compare::eq`) at every collection
//!    boundary. The only divergences allowed are the strategy itself and the mark-region
//!    space's line-granular word accounting.
//! 2. **The strategy is really reachable.** `GcConfig::default()` resolves to the copying
//!    variant, while a policy whose mature generation is at the crate's break-even resolves
//!    to the mark-region variant through the **automatic** rule (no pin) — and
//!    `AnyCollector::new(policy)`, the counterpart of `Collector::new`, resolves both the
//!    same way.
//! 3. **The mature-space geometry the host reads is strategy-agnostic.** The added
//!    forwarders (`mature_capacity_words` / `mature_used_words` / `mature_available_words`,
//!    plus `mature_base` / `mature_contains`) reproduce exactly what the **concrete** mature
//!    space answers, for both pinned strategies — including the one deliberate difference
//!    between them (the mark-region range test also covers free lines).
//!
//! Every corpus is deterministic (no randomness), every assertion is on a deterministic
//! count or a structural comparison, and **no timing is ever asserted**.

use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, GcStats, HeapInfo, HeapPolicy, MatureKind,
    MatureSource, IMMIX_BREAK_EVEN_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{is_boxed, make_small, ptr_val, AtomTable, Eterm};
// ---------------------------------------------------------------------------
// The process-shaped workload
// ---------------------------------------------------------------------------

/// The nursery the workload allocates in, in words.
const NURSERY_WORDS: usize = 256;
/// The mature generation the workload promotes into, in words. A whole number of
/// mark-region blocks (32 × 256), so the mark-region space's block rounding lands on exactly
/// this capacity too and the two strategies see the same geometry.
const MATURE_WORDS: usize = 8192;
/// How many tuples the mutator writes per round (3 words each — far more than the nursery
/// holds, so collections fire mid-burst).
const BURST: usize = 120;
/// The word count `ensure_capacity` is asked for on the growth rounds.
const GROW_REQUEST: usize = 48;

/// A **fixed-geometry** policy: a small, pinned nursery plus a large, pinned mature
/// generation, so neither strategy ever resizes a space and both see the same cadence.
///
/// `min_heap_words` equals the mature capacity, which is what pins it: `should_shrink` can
/// only fire *above* that floor, so the mature generation is never shrunk — and the
/// mark-region space's block-rounded capacity therefore never lands on a different size than
/// the copying space's. The nursery is pinned the same way, and its only growth path (a
/// surviving set larger than itself) is unreachable here because a collection always fires
/// before the nursery fills.
fn workload_policy() -> HeapPolicy {
    HeapPolicy {
        min_heap_words: MATURE_WORDS,
        initial_young_words: NURSERY_WORDS,
        initial_mature_words: MATURE_WORDS,
        fullsweep_after: 2,
        ..HeapPolicy::default()
    }
}

/// Everything the lockstep run observed.
struct Lockstep {
    copying: AnyCollector,
    immix: AnyCollector,
    /// The kind of every collection, in order (asserted equal between the two runs).
    kinds: Vec<CollectionKind>,
    /// Every `should_collect` answer, in order (asserted equal between the two runs).
    asked: Vec<bool>,
}

/// Asserts the two runs' root sets are structurally equal.
fn assert_roots_equal(a: &[Eterm], b: &[Eterm], atoms: &AtomTable, what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: root count");
    for (j, (x, y)) in a.iter().zip(b).enumerate() {
        assert!(
            term_eq(*x, *y, atoms),
            "{what}: root {j} differs between the copying and mark-region runs"
        );
    }
}

/// Writes the same 3-word tuple into both runs' nurseries, **after** running the heap-admission
/// point `beam_process::Process` runs: ask [`AnyCollector::should_collect`], collect when it
/// says so, then write the term. Every step goes through the strategy seam — no concrete
/// collector is named — and every step asserts the two runs agreed.
///
/// A term is valid only until the next collection relocates it, which is why the caller pushes
/// both words into their run's root set at once.
fn alloc_pair(
    copying: &mut AnyCollector,
    roots_c: &mut Vec<Eterm>,
    immix: &mut AnyCollector,
    roots_i: &mut Vec<Eterm>,
    atoms: &AtomTable,
    a: i64,
    b: i64,
    d: i64,
) -> (Eterm, Eterm, bool, Option<CollectionKind>) {
    let asked_c = copying.should_collect(3);
    let asked_i = immix.should_collect(3);
    assert_eq!(
        asked_c, asked_i,
        "the two strategies must agree on every admission query"
    );

    let kind = if asked_c {
        let kc = copying
            .collect(roots_c)
            .expect("the collection fits the fixed geometry")
            .kind;
        let ki = immix
            .collect(roots_i)
            .expect("the collection fits the fixed geometry")
            .kind;
        assert_eq!(kc, ki, "the two strategies must run the same collection");
        assert_eq!(
            copying.young().used_words(),
            0,
            "a collection empties the nursery"
        );
        assert_eq!(
            immix.young().used_words(),
            0,
            "a collection empties the nursery"
        );
        assert_roots_equal(roots_c, roots_i, atoms, "after a collection");
        Some(kc)
    } else {
        None
    };

    let tc = copying
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(b), make_small(d)])
        .expect("the nursery `should_collect` just cleared holds a 3-word tuple");
    let ti = immix
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(b), make_small(d)])
        .expect("the nursery `should_collect` just cleared holds a 3-word tuple");
    (tc, ti, asked_c, kind)
}

/// Drives the process-shaped workload through a copying collector and a mark-region collector
/// **in lockstep**, asserting on the way that both ask the same admission questions, run the
/// same collections, and hold structurally identical live graphs.
fn run_lockstep() -> Lockstep {
    let atoms = AtomTable::new();
    // Explicitly pinned, so the variants under test are the ones that run.
    let mut copying =
        AnyCollector::from_config(&GcConfig::copying(workload_policy())).expect("collector");
    let mut immix =
        AnyCollector::from_config(&GcConfig::immix(workload_policy())).expect("collector");
    assert_eq!(
        copying.kind(),
        MatureKind::Copying,
        "the copy run is pinned"
    );
    assert_eq!(
        immix.kind(),
        MatureKind::Immix,
        "the mark-region run is pinned"
    );

    let mut kinds = Vec::new();
    let mut asked = Vec::new();
    let mut roots_c: Vec<Eterm> = Vec::new();
    let mut roots_i: Vec<Eterm> = Vec::new();

    // The process's long-lived terms, written before the workload starts.
    for i in 0..3i64 {
        let (tc, ti, a, kind) = alloc_pair(
            &mut copying,
            &mut roots_c,
            &mut immix,
            &mut roots_i,
            &atoms,
            i,
            i + 1,
            i + 2,
        );
        asked.push(a);
        if let Some(k) = kind {
            kinds.push(k);
        }
        roots_c.push(tc);
        roots_i.push(ti);
    }
    assert_roots_equal(&roots_c, &roots_i, &atoms, "after the long-lived terms");

    for round in 0..12i64 {
        // The mutator writes a burst of terms. Each is pushed into the root set immediately,
        // so a mid-burst collection relocates it instead of orphaning the word.
        for i in 0..BURST as i64 {
            let (tc, ti, a, kind) = alloc_pair(
                &mut copying,
                &mut roots_c,
                &mut immix,
                &mut roots_i,
                &atoms,
                round,
                i,
                i + round,
            );
            asked.push(a);
            if let Some(k) = kind {
                kinds.push(k);
            }
            roots_c.push(tc);
            roots_i.push(ti);
        }

        // Keep two of the burst's terms; the rest — and the previous round's kept pair —
        // become garbage the next collections must reclaim.
        let (kept_c, kept_i) = (
            (roots_c[3], roots_c[3 + BURST - 1]),
            (roots_i[3], roots_i[3 + BURST - 1]),
        );
        roots_c.truncate(3);
        roots_i.truncate(3);
        roots_c.extend([kept_c.0, kept_c.1]);
        roots_i.extend([kept_i.0, kept_i.1]);

        // Every third round the runtime grows the heap on demand, exactly as
        // `beam_process::Process::ensure_capacity` does.
        if round % 3 == 2 {
            let kc = copying
                .ensure_capacity(GROW_REQUEST, &mut roots_c)
                .expect("grow on demand")
                .kind;
            let ki = immix
                .ensure_capacity(GROW_REQUEST, &mut roots_i)
                .expect("grow on demand")
                .kind;
            assert_eq!(kc, ki, "the two strategies must run the same collection");
            kinds.push(kc);
            assert!(copying.young().available_words() >= GROW_REQUEST);
            assert!(immix.young().available_words() >= GROW_REQUEST);
            assert_roots_equal(&roots_c, &roots_i, &atoms, "after `ensure_capacity`");
        }

        assert_roots_equal(&roots_c, &roots_i, &atoms, "after a round");
    }

    // A final collection, so the run ends on a collection boundary with the last round's
    // garbage reclaimed.
    let kc = copying
        .collect(&mut roots_c)
        .expect("final collection")
        .kind;
    let ki = immix.collect(&mut roots_i).expect("final collection").kind;
    assert_eq!(kc, ki, "the final collection must be the same kind");
    kinds.push(kc);
    assert_roots_equal(&roots_c, &roots_i, &atoms, "after the final collection");

    Lockstep {
        copying,
        immix,
        kinds,
        asked,
    }
}

/// The selected strategy's own mature-generation figures, read from its **concrete** mature
/// space — the reference the strategy-agnostic accessors must reproduce.
fn mature_of(c: &AnyCollector) -> (usize, usize, usize) {
    match c {
        AnyCollector::Copying(x) => (
            x.mature().capacity_words(),
            x.mature().used_words(),
            x.mature().available_words(),
        ),
        AnyCollector::Immix(x) => (
            x.mature().capacity_words(),
            x.mature().used_words(),
            x.mature().available_words(),
        ),
    }
}

/// What the selected strategy's own `should_collect` answers.
fn underlying_should_collect(c: &AnyCollector, need_words: usize) -> bool {
    match c {
        AnyCollector::Copying(x) => x.should_collect(need_words),
        AnyCollector::Immix(x) => x.should_collect(need_words),
    }
}

// ---------------------------------------------------------------------------
// 1. Both strategies are observationally equivalent on the process-shaped workload
// ---------------------------------------------------------------------------

#[test]
fn both_strategies_run_the_same_collection_sequence_on_a_process_shaped_workload() {
    let run = run_lockstep();
    let Lockstep {
        copying,
        immix,
        kinds,
        asked,
    } = run;

    // The workload is a real one: many collections, and the cadence exercises both kinds.
    assert!(kinds.len() >= 8, "got {kinds:?}");
    assert!(
        kinds.contains(&CollectionKind::Minor) && kinds.contains(&CollectionKind::Major),
        "the workload must exercise both minors and majors, got {kinds:?}"
    );
    assert!(asked.iter().any(|a| *a), "the workload filled the nursery");

    // The lockstep assertions already proved the sequences and the live graphs agree
    // collection by collection; the counters and the geometry agree too.
    assert_eq!(immix.minor_gcs(), copying.minor_gcs());
    assert_eq!(
        immix.young().capacity_words(),
        copying.young().capacity_words()
    );
    assert_eq!(copying.young().capacity_words(), NURSERY_WORDS);
}

#[test]
fn the_strategy_seam_is_observationally_equivalent_apart_from_the_strategy() {
    let Lockstep {
        copying,
        immix,
        kinds,
        ..
    } = run_lockstep();

    // Both variants really ran.
    assert!(matches!(copying, AnyCollector::Copying(_)));
    assert!(matches!(immix, AnyCollector::Immix(_)));

    // The policy is the collector's own, forwarded verbatim on both variants.
    assert_eq!(copying.policy(), &workload_policy());
    assert_eq!(immix.policy(), copying.policy());

    // The `garbage_collection` item agrees field for field: this workload's fixed geometry
    // holds both mature generations at the same capacity, so even `total_heap_size` matches.
    let info_c: HeapInfo = copying.heap_info();
    let info_i: HeapInfo = immix.heap_info();
    assert_eq!(info_i, info_c);
    assert_eq!(info_c.heap_size, NURSERY_WORDS);
    assert_eq!(info_c.total_heap_size, NURSERY_WORDS + MATURE_WORDS);
    assert_eq!(info_c.min_heap_size, MATURE_WORDS);
    assert_eq!(info_c.minor_gcs, copying.minor_gcs());

    // The collection *counts* agree; only the strategy's copy work may differ (the mark-region
    // path pins whole blocks instead of re-copying them).
    let stats_c: GcStats = copying.stats();
    let stats_i: GcStats = immix.stats();
    assert_eq!(stats_i.minor_collections, stats_c.minor_collections);
    assert_eq!(stats_i.major_collections, stats_c.major_collections);
    assert_eq!(
        stats_c.minor_collections + stats_c.major_collections,
        kinds.len() as u64,
        "every collection the workload ran is counted"
    );
    assert!(stats_c.minor_collections > 0 && stats_c.major_collections > 0);

    // Both mature generations are at the configured capacity, and both retained the live roots.
    assert_eq!(copying.mature_capacity_words(), MATURE_WORDS);
    assert_eq!(immix.mature_capacity_words(), MATURE_WORDS);
    assert!(
        copying.mature_used_words() > 0,
        "the live roots were promoted"
    );
    assert!(
        immix.mature_used_words() >= copying.mature_used_words(),
        "the mark-region space accounts in whole lines, never in fewer words than it holds"
    );
    assert_eq!(
        copying.mature_used_words() + copying.mature_available_words(),
        MATURE_WORDS
    );
    assert_eq!(
        immix.mature_used_words() + immix.mature_available_words(),
        MATURE_WORDS
    );
}

// ---------------------------------------------------------------------------
// 2. The strategy is reachable from the production seam
// ---------------------------------------------------------------------------

#[test]
fn the_automatic_rule_reaches_both_strategies_through_the_production_seam() {
    // The default configuration resolves to the copying path ...
    let default = AnyCollector::from_config(&GcConfig::default()).expect("collector");
    assert_eq!(default.kind(), MatureKind::Copying);
    assert!(matches!(default, AnyCollector::Copying(_)));
    assert_eq!(default.mature_choice().source, MatureSource::Automatic);

    // ... and `AnyCollector::new(policy)` — the counterpart of `Collector::new`, and the one
    // the host would call — resolves it the same way.
    let fresh = AnyCollector::new(HeapPolicy::default()).expect("collector");
    assert_eq!(fresh.kind(), MatureKind::Copying);
    assert_eq!(
        fresh.mature_capacity_words(),
        HeapPolicy::default().initial_mature_words
    );

    // A policy whose mature generation is exactly at the crate's break-even selects the
    // mark-region path through the AUTOMATIC rule — no explicit pin anywhere.
    let at_break_even = HeapPolicy {
        initial_mature_words: IMMIX_BREAK_EVEN_WORDS,
        ..HeapPolicy::default()
    };
    let auto =
        AnyCollector::from_config(&GcConfig::auto(at_break_even.clone())).expect("collector");
    assert_eq!(auto.kind(), MatureKind::Immix);
    assert!(matches!(auto, AnyCollector::Immix(_)));
    let choice = auto.mature_choice();
    assert_eq!(choice.source, MatureSource::Automatic);
    assert_eq!(choice.mature_words, IMMIX_BREAK_EVEN_WORDS);
    assert_eq!(auto.mature_capacity_words(), IMMIX_BREAK_EVEN_WORDS);
    assert_eq!(
        AnyCollector::new(at_break_even.clone())
            .expect("collector")
            .kind(),
        MatureKind::Immix
    );

    // One word below the break-even it stays on the copying path, so the switch sits exactly
    // at the crate's constant.
    let below = HeapPolicy {
        initial_mature_words: IMMIX_BREAK_EVEN_WORDS - 1,
        ..HeapPolicy::default()
    };
    assert_eq!(
        AnyCollector::new(below).expect("collector").kind(),
        MatureKind::Copying
    );

    // An explicit pin still beats the rule in both directions, through the same seam.
    let pinned_copying =
        AnyCollector::from_config(&GcConfig::copying(at_break_even)).expect("collector");
    assert_eq!(pinned_copying.kind(), MatureKind::Copying);
    assert!(pinned_copying.mature_choice().is_override());
    let tiny = HeapPolicy {
        initial_mature_words: 8,
        ..HeapPolicy::default()
    };
    let pinned_immix = AnyCollector::from_config(&GcConfig::immix(tiny)).expect("collector");
    assert_eq!(pinned_immix.kind(), MatureKind::Immix);
    assert!(pinned_immix.mature_choice().is_override());
}

// ---------------------------------------------------------------------------
// 3. The added forwarders are the underlying strategy's own answers
// ---------------------------------------------------------------------------

#[test]
fn the_mature_accessors_forward_to_the_selected_strategy() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let config = GcConfig::copying(workload_policy()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(c.kind(), kind);

        // A fresh mature generation: the capacity is reported, nothing is used, all available.
        let (capacity, used, available) = mature_of(&c);
        assert_eq!(c.mature_capacity_words(), capacity, "{kind:?} capacity");
        assert_eq!(c.mature_used_words(), used, "{kind:?} used");
        assert_eq!(c.mature_available_words(), available, "{kind:?} available");
        assert_eq!(used, 0, "{kind:?}: a fresh mature generation is empty");
        assert_eq!(available, capacity);
        assert!(
            capacity >= MATURE_WORDS,
            "{kind:?}: the configured capacity is honoured"
        );

        // Promote a live term: the accessors track the underlying strategy, and stay
        // self-consistent (`used + available == capacity`).
        let term = c
            .young_mut()
            .heap_mut()
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("tuple");
        let mut roots = [term];
        c.collect_minor(&mut roots).expect("the promotion fits");

        let (capacity, used, available) = mature_of(&c);
        assert_eq!(c.mature_capacity_words(), capacity, "{kind:?} capacity");
        assert_eq!(c.mature_used_words(), used, "{kind:?} used");
        assert_eq!(c.mature_available_words(), available, "{kind:?} available");
        assert!(
            used >= 3,
            "{kind:?}: the promoted tuple is resident in mature"
        );
        assert_eq!(
            used + available,
            capacity,
            "{kind:?}: used + available == capacity"
        );
    }
}

#[test]
fn should_collect_answers_exactly_what_the_nursery_answers() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let config = GcConfig::copying(workload_policy()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");

        // A fresh nursery: every request up to the capacity fits, one word more does not.
        let capacity = c.young().capacity_words();
        assert_eq!(c.young().available_words(), capacity);
        for need in [
            0,
            1,
            capacity / 2,
            capacity - 1,
            capacity,
            capacity + 1,
            usize::MAX,
        ] {
            assert_eq!(
                c.should_collect(need),
                capacity < need,
                "{kind:?}: `should_collect({need})` must be the nursery's own arithmetic"
            );
        }

        // The forwarder agrees with the underlying strategy's own answer.
        assert!(!underlying_should_collect(&c, capacity));
        assert!(underlying_should_collect(&c, capacity + 1));
        assert!(!c.should_collect(capacity));
        assert!(c.should_collect(capacity + 1));

        // Fill the nursery and confirm the query flips exactly where the space runs out.
        let mut written = 0i64;
        while c.young().available_words() >= 3 {
            let _ = c
                .young_mut()
                .heap_mut()
                .tuple(&[make_small(written), make_small(0), make_small(0)])
                .expect("the space still has room for one 3-word tuple");
            written += 1;
        }
        assert!(written > 0, "the nursery was fillable");
        let left = c.young().available_words();
        assert!(left < 3, "the nursery was filled to its last few words");
        assert_eq!(c.should_collect(3), left < 3);
        assert!(c.should_collect(3));
        assert!(!c.should_collect(left));
        assert!(c.should_collect(left + 1));
    }
}

// ---------------------------------------------------------------------------
// 4. The mature-space geometry is queryable strategy-agnostically
// ---------------------------------------------------------------------------

/// The selected strategy's **concrete** mature space's `base` — the reference
/// [`AnyCollector::mature_base`] must reproduce.
fn concrete_mature_base(c: &AnyCollector) -> *mut Eterm {
    match c {
        AnyCollector::Copying(x) => x.mature().base(),
        AnyCollector::Immix(x) => x.mature().base(),
    }
}

/// The selected strategy's **concrete** mature space's own `contains` — the reference
/// [`AnyCollector::mature_contains`] must reproduce address for address.
fn concrete_mature_contains(c: &AnyCollector, ptr: *const Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains(ptr),
        AnyCollector::Immix(x) => x.mature().contains(ptr),
    }
}

/// One past the last byte of the mature region (`base + capacity_words`), computed from the
/// strategy-agnostic accessors alone.
fn mature_region_end(c: &AnyCollector) -> usize {
    c.mature_base() as usize + c.mature_capacity_words() * core::mem::size_of::<Eterm>()
}

/// The host's `root_is_unscanned` valve asks "is this term's address inside a region the
/// collector scans?" — the young nursery being one, the mature generation the other. This
/// pins the mature half of that question: for **both** pinned strategies the new
/// [`AnyCollector::mature_contains`] answers exactly what the concrete mature space answers,
/// for probe addresses inside a freshly allocated object, on a word the region owns but has
/// not allocated, and outside the region entirely, and [`AnyCollector::mature_base`] /
/// [`AnyCollector::mature_capacity_words`] name the same region the concrete space does.
#[test]
fn the_mature_membership_predicate_agrees_with_the_concrete_space() {
    let word = core::mem::size_of::<Eterm>();
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let config = GcConfig::copying(workload_policy()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(
            c.kind(),
            kind,
            "the variant under test is the one that runs"
        );

        // The workload's geometry is fixed, so the mature capacity is exactly the configured
        // one on both paths (a whole number of mark-region blocks).
        assert_eq!(
            c.mature_capacity_words(),
            MATURE_WORDS,
            "{kind:?}: capacity"
        );

        // Promote one live 3-word tuple into the mature generation.
        let term = c
            .young_mut()
            .heap_mut()
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("the nursery holds the tuple");
        let mut roots = [term];
        c.collect_minor(&mut roots).expect("the promotion fits");
        let live = roots[0];
        assert!(is_boxed(live), "{kind:?}: the promoted tuple is boxed");

        // `mature_base` + `mature_capacity_words` name exactly the concrete space's region:
        // its base, and `base + capacity_words` for its end (both spaces are contiguous).
        assert_eq!(c.mature_base(), concrete_mature_base(&c), "{kind:?}: base");
        assert_eq!(
            mature_region_end(&c),
            c.mature_base() as usize + MATURE_WORDS * word,
            "{kind:?}: the region runs `capacity_words` past the base"
        );

        // Probe addresses: the header of a freshly allocated object (inside the region, on an
        // allocated word) ...
        let in_object = ptr_val(live) as *const Eterm;
        // ... the region's last word (inside the region, past the copying space's live fill
        // but inside the mark-region space's block-aligned area) ...
        let last_word = (mature_region_end(&c) - word) as *const Eterm;
        // ... the base itself ...
        let at_base = c.mature_base() as *const Eterm;
        // ... one word before the base, one past the end, a stack word, and the nursery's own
        // base — every one of them outside the mature region.
        let before_base = (c.mature_base() as usize - word) as *const Eterm;
        let at_end = mature_region_end(&c) as *const Eterm;
        let on_stack: Eterm = make_small(7);
        let stack_word = core::ptr::addr_of!(on_stack);
        let nursery_base = c.young().base() as *const Eterm;
        let probes = [
            in_object,
            last_word,
            at_base,
            before_base,
            at_end,
            stack_word,
            nursery_base,
        ];

        // The predicate is the concrete mature space's own answer, address for address — the
        // strategy-agnostic accessor introduces no range logic of its own.
        for probe in probes {
            assert_eq!(
                c.mature_contains(probe),
                concrete_mature_contains(&c, probe),
                "{kind:?}: `mature_contains({probe:?})` must be the concrete space's answer"
            );
        }

        // ... and each anchor behaves as the two strategies' documented geometry says.
        assert!(
            c.mature_contains(in_object),
            "{kind:?}: a freshly allocated object is inside the mature region"
        );
        assert!(
            c.mature_contains(at_base),
            "{kind:?}: a region with allocated words owns its base"
        );
        assert!(
            !c.mature_contains(before_base),
            "{kind:?}: one word before the base is outside"
        );
        assert!(
            !c.mature_contains(at_end),
            "{kind:?}: the region is half-open — `base + capacity_words` is outside"
        );
        assert!(
            !c.mature_contains(stack_word),
            "{kind:?}: a stack word is outside"
        );
        assert!(
            !c.mature_contains(nursery_base),
            "{kind:?}: the nursery is a different region"
        );

        // The one semantic difference between the strategies: the copying space's range test
        // covers only its **live fill** `[base, top)`, so the region's last word — a
        // not-yet-allocated word — is not owned; the mark-region space owns its whole
        // block-aligned region, free lines included, so that same address IS owned. A caller
        // must read the over-inclusion as "the address belongs to the mature region", never as
        // "a live object lives there".
        assert_eq!(
            c.mature_contains(last_word),
            kind == MatureKind::Immix,
            "{kind:?}: only the mark-region range test is over-inclusive (it covers free lines)"
        );
        assert!(
            c.mature_used_words() < MATURE_WORDS,
            "{kind:?}: the promoted tuple did not fill the region, so `last_word` is unallocated"
        );
    }
}
