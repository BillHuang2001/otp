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

//! **The no-relocation growth seam's contract** — strict progress, a faithful reserved-capacity
//! account, geometric chain growth, inert refusals and the collection that bounds the chain.
//!
//! `Nursery::grow` (`src/nursery.rs`) is the **no-relocation** growth seam: a space that cannot
//! hold the next allocation is not enlarged in place, it is **retired** into the chain and a
//! fresh space is installed (`Nursery::install`). Nothing ever frees a retired space outside a
//! **collection** (`Nursery::collapse`), and `Nursery::capacity_words` — which is what
//! [`HeapObservable::heap_size_words`] / [`HeapObservable::total_heap_size_words`] report — is
//! the **chain sum**, not the current space's size.
//!
//! # The rule these cases pin (what the seam does)
//!
//! A growth that has to allocate a fresh space sizes it as
//!
//! ```text
//! target = max( strict_progress, min(candidate, max(one_step, live_target)) )
//! ```
//!
//! where `one_step = next_words(need)` floored by the current capacity, `candidate` is `one_step`
//! when that already enlarges the space (else at least a doubling), `live_target` is the target the
//! last collection derived, and `strict_progress = HeapPolicy::next_words_above(capacity)` is the
//! C's "never the same size when *growing*" step (`erl_gc.c:310-338`). The live-set bound therefore
//! only **narrows the choice from above**: it can no longer discard the progress, which is what used
//! to install a space exactly the size of the one it retired (`[90→90, 90→90, …]`) and append a
//! whole capacity to the chain per event.
//!
//! **A nursery that is never collected grows with its allocation; bounding it requires a
//! collection, which is the caller's decision and is what
//! [`AnyCollector::collection_debt`] reports.** The seam is not the instrument for that bound: a
//! refused growth cannot be honoured by this host (every growth site in `beam_rt` absorbs the
//! `Err`, leaving the BIF result on its own pool), so the seam's contract is *strict progress plus
//! an honest account* and the collapse seam — a collection — is the one reclamation point. The C
//! says the same thing by other means: `delay_garbage_collection` (`erl_gc.c:500-599`) hands out
//! one bounded fragment and sets `F_FORCE_GC`, i.e. it *asks for a collection* rather than
//! refusing the allocation.
//!
//! # The contract (asserted below)
//!
//! 1. **A granted growth makes strict progress** — the fresh current space is at least
//!    `next_words_above` the space it retires, so a growth event can never stack an equally sized
//!    space onto the chain (one whole retained capacity that grants no room, repeatable forever).
//! 2. **The chain grows *geometrically*.** Every curated size is at least `1.2x` its predecessor
//!    (`heap_sizes`), so `N` granted growths cost `O(log N)` spaces and the reported figure is
//!    `O(current capacity)`: the chain sum is at most the current space plus the geometric series
//!    behind it (`<= base + 6 * current`), never one whole capacity per event on top of it.
//! 3. **The reported figures are a faithful reserved-capacity account** —
//!    [`HeapObservable::heap_size_words`] is exactly the sum of the spaces the chain has
//!    installed (the initial one included), [`HeapObservable::total_heap_size_words`] is that plus
//!    the mature generation, and [`HeapObservable::heap_used_words`] is the chain's fill.
//! 4. **Nothing bounds the chain but a collection** — the chain sum strictly grows with the number
//!    of events at a fixed live set, and every event is granted. This is the residual the module
//!    doc states: the figure tracks the interval's allocation, not a live set.
//! 5. **A refused growth changes nothing.** The two legitimate grounds — the growth table's
//!    ceiling and a [`HeapPolicy::max_heap_words`] budget — leave
//!    [`HeapObservable::heap_size_words`], the current space's base, the chain length and the
//!    collection debt **exactly** as they were, so the caller can collect and retry; and the
//!    refusal is `O(1)`-ish, so a long loop of refused growths stays fast.
//! 6. **A collection still collapses the chain onto its live-set target** (the green control) —
//!    after [`AnyCollector::collect`] / [`AnyCollector::collect_minor`] the chain is exactly one
//!    space ([`AnyCollector::young_space_count`] == 1), its capacity is what
//!    [`HeapPolicy::young_after_collection`] / [`HeapPolicy::young_after_fullsweep`] prescribe
//!    for that chain, and [`AnyCollector::collection_debt`] is false.
//! 7. **A provably empty retired space is re-adopted, not replaced** — the smallest adequate one,
//!    allocating nothing and consuming none of the capacity budget (the `O(log)`-indexed
//!    [`Nursery::empty_retired_for`] path that keeps a growth from scanning the chain).
//!
//! Every corpus here is deterministic — assertions are on word counts, never on wall time (the one
//! exception is a generous ceiling that pins the refusal path is not a chain scan) — and every case
//! is driven through both mature strategies wherever the cost allows (the nursery chain is shared by
//! [`AnyCollector::Copying`] and [`AnyCollector::Immix`]).

use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind,
    YOUNG_SHRINK_FACTOR,
};
use beam_term::prelude::{make_small, Eterm};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Both mature strategies: the nursery chain is strategy-independent, so every pin below holds
/// for both.
const KINDS: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// The runtime's copy-out growth-request floor, in term words — `GROW_MIN_WORDS` in
/// `beam_rt::context::calls` (mirrored here because `beam_rt` depends on this crate, never the
/// other way round). A host's copy-out growth asks for `copy_shortfall * 4` floored at this, so
/// a copy-out-driven growth event is a **≥1024-word request**.
const HOST_GROW_MIN_WORDS: usize = 1024;

/// A small request — the shape a growth takes when the space is already much larger than what
/// the next allocation needs (the host's bare-shortfall fallback is the same shape).
const SMALL_REQUEST: usize = 32;

/// The curated growth table's smallest ratio: every entry is at least `1.2x` its predecessor
/// (`policy::heap_sizes`: Fibonacci-ish up to 233, then `+20%` per step), so `n` granted growths
/// are covered by `O(log n)` spaces and the chain sum is a bounded multiple of its last space.
fn at_least_one_point_two(before: usize, after: usize) -> bool {
    after >= before.saturating_add(before / 5)
}

/// A collector for `policy` with `kind` **pinned** (an explicit override of the automatic
/// rule), so a case is independent of the mature-generation selection rule.
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::new(policy).with_mature(kind)).expect("collector")
}

/// Writes 3-word tuples into `space` until it has **fewer than** `keep_free` words available —
/// "the mutator filled the current space", the state a growth event answers.
///
/// Returns the number of tuples written, so a caller can assert the fill really happened.
fn fill_to(space: &mut HeapSpace, keep_free: usize) -> usize {
    let filler = [make_small(1), make_small(2)];
    let mut tuples = 0;
    while space.available_words() >= keep_free {
        if space.heap_mut().tuple(&filler).is_err() {
            break;
        }
        tuples += 1;
    }
    tuples
}

/// Builds a **live term of exactly `live_words` words** in the nursery and returns it as a root
/// set: one tuple of arity `live_words - 1` (its header is the extra word). An empty live set is
/// an empty root set.
fn build_live(c: &mut AnyCollector, live_words: usize) -> Vec<Eterm> {
    if live_words == 0 {
        return Vec::new();
    }
    let elems = vec![make_small(7); live_words - 1];
    let term = c
        .young_mut()
        .heap_mut()
        .tuple(&elems)
        .expect("the establishing live term fits the initial nursery");
    vec![term]
}

/// Runs the **one** collection a growth loop starts from, and returns the nursery capacity right
/// after it — the live-set figure every bound in this file is expressed against.
///
/// The roots are returned too so the caller can keep the live set reachable; the copier has
/// forwarded them into the mature generation, so they stay valid terms.
fn establish(c: &mut AnyCollector, live_words: usize) -> (Vec<Eterm>, usize) {
    let mut roots = build_live(c, live_words);
    let stats = c.collect_minor(&mut roots).expect("the establishing minor");
    assert_eq!(
        stats.words_copied, live_words,
        "the establishing collection measured the live set it was given"
    );
    let base = c.young().capacity_words();
    assert_eq!(
        c.young_space_count(),
        1,
        "the establishing collection collapsed the chain to one space"
    );
    (roots, base)
}

/// One **growth event**: fill the current space, then ask the seam for `hint` words of room.
///
/// Returns `(before, after)`: the capacity of the current space immediately before the growth,
/// and the capacity of the fresh space it installed — `None` when the seam **refused** the growth
/// (one of the two legitimate ceilings), which is a normal, non-destructive outcome.
fn growth_event(c: &mut AnyCollector, hint: usize) -> (usize, Option<usize>) {
    let had_room = c.young().available_words() >= hint;
    let filled = fill_to(c.young_mut(), hint);
    assert!(
        !had_room || filled > 0,
        "the event fills the current space when it had room to spare"
    );
    assert!(
        c.young().available_words() < hint,
        "the request must outgrow the current space, or the seam is a no-op"
    );
    let before = c.young().capacity_words();
    match c.grow_nursery(hint) {
        Ok(()) => {
            let after = c.young().capacity_words();
            assert!(
                after > before,
                "a successful growth installs a space strictly larger than the {before}-word one \
                 it retired, got {after}"
            );
            (before, Some(after))
        }
        Err(err) => {
            assert_eq!(
                err.as_atom(),
                Some("out_of_memory"),
                "a refused growth is the typed heap exhaustion"
            );
            (before, None)
        }
    }
}

/// Drives up to `events` growth events with **no collection in between**, and returns the
/// capacities of the spaces the seam granted (the refusals are dropped).
fn drive_growth(c: &mut AnyCollector, hint: usize, events: usize) -> Vec<usize> {
    let mut granted = Vec::new();
    for _ in 0..events {
        if let (_, Some(after)) = growth_event(c, hint) {
            granted.push(after);
        }
    }
    granted
}

// ---------------------------------------------------------------------------
// 1. Strict progress, and a chain that grows geometrically
// ---------------------------------------------------------------------------

/// One growth event's capacity trace.
#[derive(Debug, Clone, Copy)]
struct Step {
    event: usize,
    before: usize,
    after: usize,
    total_before: usize,
    total_after: usize,
}

/// The first `events` growth events of a post-collection loop, step by step.
fn progress_trace(
    kind: MatureKind,
    policy: HeapPolicy,
    live_words: usize,
    hint: usize,
    events: usize,
) -> Vec<Step> {
    let mut c = any_for(kind, policy);
    let (_roots, _base) = establish(&mut c, live_words);
    (0..events)
        .map(|event| {
            let total_before = c.heap_size_words();
            let (before, after) = growth_event(&mut c, hint);
            Step {
                event,
                before,
                after: after.unwrap_or(before),
                total_before,
                total_after: c.heap_size_words(),
            }
        })
        .collect()
}

fn trace_report(title: &str, kind: MatureKind, hint: usize, steps: &[Step]) -> String {
    let mut s = format!("\n{title}\n  {kind:?}, request {hint} words\n");
    s.push_str(&format!(
        "  {:>5} {:>10} {:>10} {:>14} {:>14}\n",
        "event", "before", "after", "chain_before", "chain_after"
    ));
    for st in steps {
        s.push_str(&format!(
            "  {:>5} {:>10} {:>10} {:>14} {:>14}\n",
            st.event, st.before, st.after, st.total_before, st.total_after
        ));
    }
    s
}

/// A growth that installs a space **no larger** than the one it retires adds a whole capacity to
/// the chain sum while making no progress in room: it can only be repeated, never finished.
///
/// The same case pins the **geometric** shape of the chain: because every curated size is at
/// least `1.2x` its predecessor, `N` granted growths cost `O(log N)` spaces, so the chain sum is
/// bounded by the current space plus the geometric series behind it — never one capacity per
/// event piled on top of the current space.
#[test]
fn a_successful_growth_enlarges_the_space_it_replaces() {
    let policy = HeapPolicy {
        initial_young_words: 90,
        ..HeapPolicy::default()
    };
    let mut traces: Vec<String> = Vec::new();
    let mut stalls: Vec<(MatureKind, usize, Step)> = Vec::new();
    let mut slopes: Vec<(MatureKind, usize, Step)> = Vec::new();
    for kind in KINDS {
        for hint in [SMALL_REQUEST, HOST_GROW_MIN_WORDS] {
            let steps = progress_trace(kind, policy.clone(), 0, hint, 4);
            traces.push(trace_report(
                "a post-collection growth loop, event by event:",
                kind,
                hint,
                &steps,
            ));
            for st in &steps {
                if st.total_after == st.total_before {
                    // A refused event installs nothing at all (its `after` is the `before` the
                    // helper substitutes); the refusal itself is pinned by
                    // `a_refused_growth_is_inert_and_cheap`.
                    continue;
                }
                if st.after <= st.before {
                    stalls.push((kind, hint, *st));
                }
                if !at_least_one_point_two(st.before, st.after) {
                    slopes.push((kind, hint, *st));
                }
            }
        }
    }
    let table = traces.join("");

    // (1a) Strict progress in the current space's capacity.
    let first = stalls.first().map(|(kind, hint, st)| {
        format!(
            "first stall: {kind:?}, request {hint}, event {} installed a {}-word space over the \
             {}-word space it retired, taking the chain from {} to {} words",
            st.event, st.after, st.before, st.total_before, st.total_after
        )
    });
    assert!(
        stalls.is_empty(),
        "every successful growth must install a space strictly larger than the one it retires \
         (a same-size install appends a whole capacity to the chain and grants no room); {} \
         growth events make no progress — {}\n{table}",
        stalls.len(),
        first.unwrap_or_default()
    );

    // (1b) The chain grows geometrically: no install is smaller than 1.2x the space it retires,
    // which is what makes the space count logarithmic in the chain's total.
    let slowest = slopes.first().map(|(kind, hint, st)| {
        format!(
            "slowest: {kind:?}, request {hint}, event {} installed {} over {} ({:.3}x)",
            st.event,
            st.after,
            st.before,
            st.after as f64 / st.before as f64
        )
    });
    assert!(
        slopes.is_empty(),
        "every growth must step the curated table by at least its smallest ratio (1.2x), so the \
         chain's space count is O(log total) instead of one space per event; {} events fall short \
         — {}\n{table}",
        slopes.len(),
        slowest.unwrap_or_default()
    );

    // (1c) Because the chain gains exactly one space per granted growth, the summed capacity must
    // grow by at least the new space's own size.
    let mut shortfalls: Vec<(MatureKind, usize, Step)> = Vec::new();
    for kind in KINDS {
        for hint in [SMALL_REQUEST, HOST_GROW_MIN_WORDS] {
            for st in progress_trace(kind, policy.clone(), 0, hint, 4) {
                let delta = st.total_after.saturating_sub(st.total_before);
                if delta != 0 && delta < st.after {
                    shortfalls.push((kind, hint, st));
                }
            }
        }
    }
    assert!(
        shortfalls.is_empty(),
        "a growth event appends the whole fresh space to the chain, so the summed capacity must \
         grow by at least that space's size; {} events fell short\n{table}",
        shortfalls.len()
    );
}

// ---------------------------------------------------------------------------
// 2. The reported figures are a faithful reserved-capacity account
// ---------------------------------------------------------------------------

/// The chain sum is exactly what the seam installed, the total adds the mature generation, and
/// the occupancy is the fill — the three figures a host reports (`process_info`,
/// `erlang:memory(processes)`, `max_heap_size`).
///
/// The account matters because it is what a host *sees*: a growth that installs a space makes the
/// reported capacity grow by exactly that space, and only a collection gives it back.
#[test]
fn the_reported_figures_are_the_installed_capacities() {
    for kind in KINDS {
        let policy = HeapPolicy {
            initial_young_words: 90,
            initial_mature_words: 4096,
            ..HeapPolicy::default()
        };
        let mut c = any_for(kind, policy.clone());
        let (_roots, base) = establish(&mut c, 0);
        let mature = c.mature_capacity_words();
        let mut installed = base;
        assert_eq!(c.heap_size_words(), installed, "{kind:?}: one space to start");

        for event in 0..12 {
            let (before, after) = growth_event(&mut c, SMALL_REQUEST);
            let after = after.expect("the default policy bounds nothing");
            // The retired space stays charged to the chain: the account gains exactly the fresh
            // space's capacity, not the difference.
            installed += after;
            assert_eq!(
                c.heap_size_words(),
                installed,
                "{kind:?}: after event {event} the chain sum is the {before}-word space plus the \
                 {after}-word one it installed, i.e. {installed} words"
            );
            assert_eq!(
                c.total_heap_size_words(),
                installed + mature,
                "{kind:?}: the total adds the mature generation"
            );
            assert_eq!(
                c.young_space_count(),
                event + 2,
                "{kind:?}: one space per granted growth, plus the current one"
            );
            // ... and the occupancy is the fill, which a growth only widens.
            assert!(
                c.heap_used_words() <= installed,
                "{kind:?}: the fill is within the reserved capacity"
            );
        }

        // The geometric bound: the chain sum is the current space plus a `1.2x`-ratio series
        // behind it, i.e. at most `base + 6 * current` words — not one capacity per event.
        let current = c.young().capacity_words();
        assert!(
            c.heap_size_words() <= base + 6 * current,
            "{kind:?}: the chain sum ({}) is bounded by the current space ({current}) plus the \
             geometric series behind it",
            c.heap_size_words()
        );

        // The collapse gives the whole figure back: one space, the live set's target.
        let expected = policy.young_after_collection(c.heap_size_words(), 0);
        let stats = c.collect_minor(&mut []).expect("the collapse");
        assert_eq!(stats.words_copied, 0, "{kind:?}: nothing live");
        assert_eq!(c.young_space_count(), 1, "{kind:?}: the collection collapses the chain");
        assert_eq!(
            c.heap_size_words(),
            expected,
            "{kind:?}: the collapsed capacity is young_after_collection(chain, 0)"
        );
        assert_eq!(
            c.heap_size_words(),
            c.young().capacity_words(),
            "{kind:?}: the chain sum is one space again"
        );
        assert_eq!(
            c.total_heap_size_words(),
            c.heap_size_words() + c.mature_capacity_words(),
            "{kind:?}: the total is the nursery plus the mature generation"
        );
        assert!(!c.collection_debt(), "{kind:?}: the debt is settled");
    }
}

// ---------------------------------------------------------------------------
// 3. Nothing bounds the chain but a collection (the stated residual)
// ---------------------------------------------------------------------------

/// **The residual, pinned as a contract**: at a fixed live set, the reported figure grows with
/// the number of growth events, and every event is granted — the seam does not bound the chain.
///
/// This is the honest account of a seam that cannot collect: a bound would have to be a refusal,
/// and a refusal cannot be honoured by a host that holds no root set (the allocation it was
/// making simply stays in its own pool). The seam therefore keeps strict progress and honest
/// accounting, and the caller — which *can* collect — owns the bound
/// ([`AnyCollector::collection_debt`] reports it, and the collapse cases below show what the
/// collection does with it).
#[test]
fn the_chain_grows_with_the_interval_and_only_a_collection_bounds_it() {
    let policy = HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 4096,
        ..HeapPolicy::default()
    };
    let hint = SMALL_REQUEST;

    let mut report = String::from("\n  events -> (granted, spaces, chain words)\n");
    let mut figures: Vec<(usize, usize, usize, usize)> = Vec::new();
    for kind in KINDS {
        for events in [4usize, 8] {
            let mut c = any_for(kind, policy.clone());
            let (_roots, base) = establish(&mut c, 0);
            let granted = drive_growth(&mut c, hint, events);
            assert_eq!(
                granted.len(),
                events,
                "{kind:?}: nothing but a collection bounds the chain, so every event is granted"
            );
            figures.push((events, granted.len(), c.young_space_count(), c.heap_size_words()));
            report.push_str(&format!(
                "  {kind:?} {events:>3} -> ({}, {}, {})\n",
                granted.len(),
                c.young_space_count(),
                c.heap_size_words()
            ));
            assert!(
                c.heap_size_words() > base,
                "{kind:?}: {events} growth events must grow the chain past the {base}-word \
                 collection target{report}"
            );
            assert!(c.collection_debt(), "{kind:?}: the chain is collection debt");
        }
    }
    // ... and it is the *event count* it tracks: the same live set at twice the events reports
    // strictly more capacity (the figure is not a live-set bound).
    let pairs: Vec<&(usize, usize, usize, usize)> = figures.iter().filter(|f| f.0 == 4).collect();
    for (i, small) in pairs.iter().enumerate() {
        let large = figures.iter().filter(|f| f.0 == 8).nth(i).expect("the 2N row");
        assert!(
            large.3 > small.3,
            "the reported capacity must grow with the event count (there is no cap): {small:?} vs \
             {large:?}{report}"
        );
    }

    // A collection, and only a collection, gives it back — on both strategies and both collapse
    // seams (a minor sized from the young live set; see
    // `a_collection_collapses_the_chain_onto_its_live_set_target` for the fullsweep).
    for kind in KINDS {
        let mut c = any_for(kind, policy.clone());
        let (roots, _base) = establish(&mut c, 0);
        drive_growth(&mut c, hint, 8);
        assert!(c.collection_debt(), "{kind:?}");
        let mut roots = roots;
        c.collect_minor(&mut roots).expect("the collapse");
        assert_eq!(c.young_space_count(), 1, "{kind:?}");
        assert!(!c.collection_debt(), "{kind:?}: the collection settles the debt");
        assert_eq!(
            c.heap_size_words(),
            policy.young_minor_target_words(0),
            "{kind:?}: and lands on the live set's floor"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. A refused growth is inert, and cheap
// ---------------------------------------------------------------------------

/// The seam's two legitimate refusals — a [`HeapPolicy::max_heap_words`] budget that cannot reach
/// the request, and the growth table's own ceiling — and the state a refusal leaves behind.
///
/// * **(i) strict progress** — every granted growth installs a space strictly larger than the one
///   it retires (and the chain gains exactly one space);
/// * **(ii) a refusal is inert** — `capacity_words()`, the current space's base, the chain length
///   and the fill are byte-for-byte unchanged, and the collection debt stays true so the caller
///   knows to collect;
/// * **(iii) a refusal is not a chain scan** — a long loop of refused growths against a chained
///   nursery completes in milliseconds, which the per-growth whole-chain scan the empty-retired
///   index replaced could not do.
#[test]
fn a_refused_growth_is_inert_and_cheap() {
    /// A budget that reaches the first few growths and then bites, so the refusal path (not the
    /// allocation path) is what the loop below exercises.
    fn capped() -> HeapPolicy {
        HeapPolicy {
            max_heap_words: 2048,
            ..HeapPolicy::default()
        }
    }

    for kind in KINDS {
        let policy = capped();
        let mut c = any_for(kind, policy.clone());
        let (_roots, base) = establish(&mut c, 0);
        assert_eq!(base, 233, "{kind:?}: the default nursery");

        // (i) The granted phase, up to the budget.
        let mut granted = 0usize;
        loop {
            let (before, after) = growth_event(&mut c, SMALL_REQUEST);
            match after {
                Some(words) => {
                    assert!(
                        words > before,
                        "{kind:?}: event {granted} must install a strictly larger space \
                         ({words} <= {before})"
                    );
                    granted += 1;
                    assert_eq!(
                        c.young_space_count(),
                        granted + 1,
                        "{kind:?}: each granted growth appends exactly one space"
                    );
                    assert!(
                        c.heap_size_words() <= policy.max_heap_words,
                        "{kind:?}: the chain's summed capacity stays within the ceiling ({})",
                        policy.max_heap_words
                    );
                }
                None => break,
            }
        }
        assert!(granted >= 1, "{kind:?}: the first growth is granted");

        // (ii) The refusal is inert: nothing was allocated, retired, moved or forgotten.
        let (spaces, capacity, base_addr, used) = (
            c.young_space_count(),
            c.heap_size_words(),
            c.young().base(),
            c.heap_used_words(),
        );
        assert!(c.collection_debt(), "{kind:?}: the chain stays debt");
        for _ in 0..8 {
            let (before, after) = growth_event(&mut c, SMALL_REQUEST);
            assert!(after.is_none(), "{kind:?}: the ceiling keeps refusing");
            assert_eq!(before, c.young().capacity_words(), "{kind:?}: same current space");
        }
        assert_eq!(c.young_space_count(), spaces, "{kind:?}: a refusal retires nothing");
        assert_eq!(c.heap_size_words(), capacity, "{kind:?}: a refusal consumes no capacity");
        assert_eq!(c.young().base(), base_addr, "{kind:?}: a refusal replaces no space");
        assert_eq!(c.heap_used_words(), used, "{kind:?}: a refusal grants no room");

        // (iii) 20 000 refusals: each is a couple of comparisons against the ceiling, so the loop
        // is fast and the reported figures do not move.
        let before_figures = (
            c.heap_size_words(),
            c.total_heap_size_words(),
            c.young_space_count(),
        );
        let start = std::time::Instant::now();
        for _ in 0..20_000 {
            let (_before, after) = growth_event(&mut c, SMALL_REQUEST);
            assert!(after.is_none(), "{kind:?}: the ceiling stays fixed");
        }
        let elapsed = start.elapsed();
        assert_eq!(
            (c.heap_size_words(), c.total_heap_size_words(), c.young_space_count()),
            before_figures,
            "{kind:?}: 20 000 more refused growths must not move the reported figures"
        );
        assert_eq!(c.young_space_count(), granted + 1, "{kind:?}: the chain length is unchanged");
        // A ceiling, not a benchmark: the loop is 20 000 refusals, and a whole-chain scan per
        // event would need minutes rather than the milliseconds it takes here.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "{kind:?}: 20 000 refused growths took {elapsed:?} — the growth path scans the chain"
        );

        // The caller's move: a collection collapses the chain and gives the budget back, after
        // which the same request fits again.
        let stats = c.collect_minor(&mut []).expect("the collapse");
        assert_eq!(stats.words_copied, 0, "{kind:?}: nothing live");
        assert_eq!(c.young_space_count(), 1, "{kind:?}");
        assert!(!c.collection_debt(), "{kind:?}");
        growth_event(&mut c, SMALL_REQUEST);
        assert!(
            c.young_space_count() >= 2,
            "{kind:?}: the request is grantable again after the collection"
        );

        // (iv) The growth table's own ceiling: a request beyond the largest curated size cannot
        // be reached by any size the table can name, and is refused without touching the chain.
        let mut c = any_for(kind, HeapPolicy::default());
        let (_roots, _base) = establish(&mut c, 0);
        let before = (c.young_space_count(), c.heap_size_words(), c.young().base());
        let err = c
            .grow_nursery(usize::MAX)
            .expect_err("the growth table cannot reach an unbounded request");
        assert_eq!(err.as_atom(), Some("out_of_memory"), "{kind:?}");
        assert_eq!(
            (c.young_space_count(), c.heap_size_words(), c.young().base()),
            before,
            "{kind:?}: a table-ceiling refusal is inert too"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. A provably empty retired space is re-adopted, not replaced
// ---------------------------------------------------------------------------

/// A growth that retires a space the mutator never allocated in installs **that** space (the
/// smallest adequate one, the oldest among equals) instead of allocating a fresh one: nothing is
/// stranded, nothing is allocated, and the chain's summed capacity and length are unchanged.
///
/// This is the `O(log)`-indexed selection (`Nursery::empty_retired_for`) that keeps a growth from
/// scanning the chain, pinned on word counts rather than on a clock.
#[test]
fn an_empty_retired_space_is_re_adopted_rather_than_replaced() {
    for kind in KINDS {
        let mut c = any_for(kind, HeapPolicy::default());
        let (mut roots, _base) = establish(&mut c, 0);

        // A growth with **nothing** allocated in the 233-word default space: it retires that
        // space while empty and installs a larger one.
        let retired_base = c.young().base();
        let retired_capacity = c.young().capacity_words();
        c.grow_nursery(300).expect("the first growth");
        assert_ne!(
            c.young().base(),
            retired_base,
            "{kind:?}: the growth installed a fresh space"
        );
        assert!(
            c.young().capacity_words() > retired_capacity,
            "{kind:?}: and retired the smaller one (strict progress)"
        );
        assert_eq!(c.young_space_count(), 2, "{kind:?}: one retired space, one current");

        // Fill the current space so its remaining room is below the request below, while the
        // retired space is still **provably empty** (nothing was ever allocated in it).
        let filled = fill_to(c.young_mut(), 200);
        assert!(filled > 0, "{kind:?}: the current space really fills");
        assert!(
            c.young().available_words() < 200,
            "{kind:?}: the current space cannot serve the request"
        );
        let chained = c.heap_size_words();
        let used = c.heap_used_words();

        // The request the **retired** (smaller, but provably empty) space can serve: the index
        // answers with it and the seam swaps it in without allocating.
        c.grow_nursery(200).expect("the re-adoption");
        assert_eq!(
            c.young().base(),
            retired_base,
            "{kind:?}: the only adequate empty retired space is re-adopted"
        );
        assert_eq!(
            c.young().capacity_words(),
            retired_capacity,
            "{kind:?}: ... the space the growth above had retired"
        );
        assert_eq!(
            (c.young_space_count(), c.heap_size_words()),
            (2, chained),
            "{kind:?}: a re-adoption allocates nothing and changes no capacity"
        );
        assert_eq!(c.heap_used_words(), used, "{kind:?}: it strands nothing");
        assert!(c.collection_debt(), "{kind:?}: the chain is still debt");

        // The chain is still a nursery: a collection collapses it whole.
        roots.clear();
        c.collect_minor(&mut roots).expect("the collapse");
        assert_eq!(c.young_space_count(), 1, "{kind:?}");
        assert!(!c.collection_debt(), "{kind:?}");
    }
}

// ---------------------------------------------------------------------------
// 6. The green control: a collection still collapses the chain onto the live set
// ---------------------------------------------------------------------------

/// Whatever the growth seam did, a collection must still take the chain back to **exactly one**
/// space whose capacity is what the heap-size policy prescribes for that chain, and clear the
/// collection debt.
///
/// This is the behaviour the seam must not disturb: the collapse seam is the only reclamation
/// point of the chain, so it is what bounds the figure the growth seam accumulates. Both collapse
/// seams are exercised — a **minor** (sized from the young live set) and a **fullsweep** (sized
/// from the whole-heap live set), including its shrink branch and a live root that survives in a
/// **retired** space.
#[test]
fn a_collection_collapses_the_chain_onto_its_live_set_target() {
    /// A mature generation large enough for the whole chain's occupancy, so the minor path of
    /// the collapse seam is reachable at all (a minor that cannot fit its promotion is the
    /// typed heap exhaustion, and `collect` escalates it to a fullsweep).
    fn policy() -> HeapPolicy {
        HeapPolicy {
            initial_young_words: 90,
            initial_mature_words: 32768,
            ..HeapPolicy::default()
        }
    }

    for kind in KINDS {
        for hint in [SMALL_REQUEST, HOST_GROW_MIN_WORDS] {
            let policy = policy();
            let mut c = any_for(kind, policy.clone());
            let (_roots, base) = establish(&mut c, 0);
            assert_eq!(base, policy.nursery_floor_words(), "{kind:?}");
            let granted = drive_growth(&mut c, hint, 6);
            let chained_spaces = c.young_space_count();
            let chained_words = c.heap_size_words();
            assert_eq!(
                chained_spaces,
                granted.len() + 1,
                "{kind:?}: the chain is live, one space per granted growth"
            );
            assert!(c.collection_debt(), "{kind:?}: the chain is debt");

            // (6a) A **minor** over an empty young live set: the policy's own answer for the
            // collected chain — its target when the chain is past the keep band, the chain's own
            // capacity when it is inside it.
            let expected = policy.young_after_collection(chained_words, 0);
            let stats = c.collect_minor(&mut []).expect("the minor collapse");
            assert_eq!(
                stats.words_copied, 0,
                "{kind:?}: nothing live in the young generation"
            );
            assert_eq!(
                c.young_space_count(),
                1,
                "{kind:?}: the collection collapses the chain (it held {chained_spaces} spaces, \
                 {chained_words} words)"
            );
            assert!(!c.collection_debt(), "{kind:?}: the debt is settled");
            assert_eq!(
                c.young().capacity_words(),
                expected,
                "{kind:?}: the collapsed capacity is young_after_collection({chained_words}, 0)"
            );
            assert_eq!(
                c.heap_size_words(),
                c.young().capacity_words(),
                "{kind:?}: the chain sum is one space again"
            );

            // (6b) A **minor** over a non-empty young live set: the target is the live set's own.
            let live_words = 40;
            let mut roots = build_live(&mut c, live_words);
            let pre = c.young().capacity_words();
            let stats = c.collect_minor(&mut roots).expect("the live-set collapse");
            assert_eq!(stats.words_copied, live_words, "{kind:?}");
            assert_eq!(c.young_space_count(), 1, "{kind:?}");
            assert_eq!(
                c.heap_size_words(),
                policy.young_after_collection(pre, live_words),
                "{kind:?}: the collapsed capacity is the policy's answer for the collected chain"
            );

            // (6c) A **fullsweep** over a live set far **below** the chain: the shrink branch. A
            // fresh collector (with a nursery large enough for the establishing term) establishes
            // a 401-word live set, builds a 40-word live term, drops the 401-word root (which
            // makes it garbage), and only then grows the chain past the band. The fullsweep
            // therefore measures a live set far below the chain it inherits and gives the memory
            // back onto the whole-heap target. The surviving 40-word root sits in a **retired**
            // space by then, so this also pins that a fullsweep evacuates the whole chain, not just
            // the current space.
            let mut c = any_for(
                kind,
                HeapPolicy {
                    initial_young_words: 610,
                    ..policy.clone()
                },
            );
            let (establish_roots, _base) = establish(&mut c, 401);
            drop(establish_roots);
            let mut roots = build_live(&mut c, live_words);
            drive_growth(&mut c, hint, 6);
            let collected_chain = c.heap_size_words();
            let target = policy.young_target_words(live_words);
            assert!(
                collected_chain > YOUNG_SHRINK_FACTOR * target,
                "{kind:?}: the shrink branch must hold: the {collected_chain}-word chain is past \
                 {} x the {target}-word target",
                YOUNG_SHRINK_FACTOR
            );
            let stats = c.collect_major(&mut roots).expect("the fullsweep collapse");
            assert_eq!(
                stats.words_copied, live_words,
                "{kind:?}: the dropped 401-word term is garbage, so the live set is the 40-word one"
            );
            assert_eq!(c.young_space_count(), 1, "{kind:?}");
            assert!(!c.collection_debt(), "{kind:?}");
            assert_eq!(
                policy.young_after_fullsweep(collected_chain, live_words),
                target,
                "{kind:?}: a fullsweep from above the band installs the whole-heap target"
            );
            assert_eq!(
                c.heap_size_words(),
                policy.young_target_words(stats.words_copied),
                "{kind:?}: the fullsweep collapses onto the whole-heap live set's target"
            );
        }
    }
}

/// `collect_kind` still runs **exactly** the kind it is given, whatever the growth seam left
/// behind: a pinned minor over a chained nursery evacuates the whole chain and collapses it,
/// while a pinned fullsweep collapses onto the whole-heap target — the kind-pinned control the
/// runtime's `BEAM_RT_GC_STRATEGY`-style callers rely on.
#[test]
fn a_kind_pinned_collection_still_collapses_a_chained_nursery() {
    let policy = HeapPolicy {
        initial_young_words: 90,
        initial_mature_words: 32768,
        ..HeapPolicy::default()
    };
    for kind in KINDS {
        let mut c = any_for(kind, policy.clone());
        let (mut roots, _base) = establish(&mut c, 0);
        let mut live = build_live(&mut c, 40);
        roots.append(&mut live);
        drive_growth(&mut c, SMALL_REQUEST, 4);
        assert!(c.collection_debt(), "{kind:?}");
        let collection = c
            .collect_kind(CollectionKind::Minor, &mut roots)
            .expect("the pinned minor fits");
        assert_eq!(collection.kind, CollectionKind::Minor, "{kind:?}");
        assert_eq!(
            collection.stats.words_copied, 40,
            "{kind:?}: the whole chain was evacuated"
        );
        assert_eq!(c.young_space_count(), 1, "{kind:?}: the chain collapsed");
        assert!(!c.collection_debt(), "{kind:?}");
    }
}
