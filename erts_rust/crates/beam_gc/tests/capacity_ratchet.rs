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

//! **Capacity ratchet** — the in-crate reproducer for a nursery-chain capacity feedback.
//!
//! `Nursery::grow` (`src/nursery.rs`) is the **no-relocation** growth seam: a space that cannot
//! hold the next allocation is not enlarged in place, it is **retired** into the chain and a
//! fresh space is installed (`Nursery::install`). Nothing ever frees a retired space outside a
//! **collection** (`Nursery::collapse`), and `Nursery::capacity_words` — which is what
//! [`HeapObservable::heap_size_words`] / [`HeapObservable::total_heap_size_words`] report — is
//! the **chain sum**, not the current space's size. A growth event that is not followed by a
//! collection therefore strands a whole space's worth of capacity into the reported figure, and
//! the question this file answers is: *does a loop of growth events with no collection in
//! between bound that figure by the live set, or does it ratchet?*
//!
//! The loop modelled here is the **host's** use of the seam (`beam_rt`'s copy-out gate:
//! `context/calls.rs`'s `hint_grow_gate` / `move_result_into_process_heap`, which grow the
//! destination rather than collect because the running process's root set is not published at
//! that point): fill the current space, then ask for room with a fixed hint, and collect
//! nothing. One collection runs **before** the loop, so the nursery carries the live-set bound
//! (`live_target_words`) that the seam is clamped by.
//!
//! # The contract (all four properties are asserted below)
//!
//! 1. **The reported capacity stays live-set bounded** — [`HeapObservable::total_heap_size_words`]
//!    (and [`HeapObservable::heap_size_words`]) after `N` growth events stay within the heap-size
//!    policy's own keep band (`YOUNG_SHRINK_FACTOR` × the capacity the establishing collection
//!    installed) and are **flat in `N`**: identical at `N` and `2N`, because the number of growth
//!    events is not a workload property.
//! 2. **A successful growth makes strict progress** — the fresh current space is strictly larger
//!    than the space it replaces, and because the chain gains a whole space
//!    ([`AnyCollector::young_space_count`] +1) the summed capacity grows by **at least** that
//!    space's own size.
//! 3. **A refused growth changes nothing** — the chain cap's refusal leaves
//!    [`HeapObservable::heap_size_words`], the current space's base, the chain length and the
//!    collection debt **exactly** as they were, so the caller can collect and retry.
//! 4. **A collection still collapses the chain onto its live-set target** (the green control) —
//!    after [`AnyCollector::collect`] / [`AnyCollector::collect_minor`] the chain is exactly one
//!    space ([`AnyCollector::young_space_count`] == 1), its capacity is what
//!    [`HeapPolicy::young_after_collection`] / [`HeapPolicy::young_after_fullsweep`] prescribe
//!    for that chain, and [`AnyCollector::collection_debt`] is false.
//!
//! Every corpus here is deterministic — assertions are on word counts, never on wall time — and
//! every case is driven through both mature strategies wherever the cost allows (the nursery
//! chain is shared by [`AnyCollector::Copying`] and [`AnyCollector::Immix`]).
//!
//! # The rule these cases pin (what the seam does now)
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
//! And a chain that already **retains** a space may only grow while its summed capacity stays
//! within [`HeapPolicy::chain_cap_words`]`(live_target)` — `YOUNG_SHRINK_FACTOR` (8) × the live
//! target, the same multiple the heap-size policy treats as the acceptable in-place band. That is
//! the C's one-young-block-plus-one-bounded-fragment shape (`delay_garbage_collection`,
//! `erl_gc.c:500-599`): the **first** growth after a collection is always granted (one fragment),
//! and a second one has to collect first — so the reported capacity is bounded by the live set and
//! **flat in the number of growth events**, and a growth loop is no longer quadratic in them
//! (`Nursery::empty_retired_for` is an index, not a scan).

use beam_gc::prelude::{
    AnyCollector, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind, YOUNG_SHRINK_FACTOR,
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
/// (the chain cap), which is a normal, non-destructive outcome.
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
                "the chain cap's refusal is the typed heap exhaustion"
            );
            (before, None)
        }
    }
}

/// Drives up to `events` growth events with **no collection in between**, and returns how many of
/// them the seam granted (the rest were refused at the chain cap).
fn drive_growth(c: &mut AnyCollector, hint: usize, events: usize) -> usize {
    let mut granted = 0usize;
    for _ in 0..events {
        if growth_event(c, hint).1.is_some() {
            granted += 1;
        }
    }
    granted
}

// ---------------------------------------------------------------------------
// the report
// ---------------------------------------------------------------------------

/// One row of the measured table: a growth loop's geometry after `events` growth events.
#[derive(Debug, Clone, Copy)]
struct Row {
    scenario: &'static str,
    kind: MatureKind,
    hint: usize,
    events: usize,
    /// The nursery capacity right after the establishing collection (the live-set figure).
    base: usize,
    /// The mature generation's capacity — constant over the loop, and part of `total`.
    mature: usize,
    /// [`HeapPolicy::chain_cap_words`] over the live-set target the establishing collection
    /// remembered — the cap the growth seam itself refuses past.
    cap: usize,
    /// The **one-step** size for this scenario's request (`next_words(hint)`): the largest fresh
    /// space a single growth can install for it.
    step: usize,
    /// `young_space_count()` after the loop.
    spaces: usize,
    /// How many of the `events` growth events the seam granted.
    granted: usize,
    /// `heap_size_words()` — the chain's **summed** nursery capacity.
    heap_size: usize,
    /// `total_heap_size_words()` — the chain plus the mature generation.
    total: usize,
}

impl Row {
    /// The live-set bound this file holds the reported capacity to, derived from the rule the
    /// seam implements rather than from a hand-picked multiple.
    ///
    /// Every growth granted **behind a retained space** checks
    /// `capacity_words() + target <= chain_cap_words(live_target)` *before* installing, so after
    /// such a grant the chain is inside [`HeapPolicy::chain_cap_words`] — `YOUNG_SHRINK_FACTOR`
    /// (8) times the live-set target the establishing collection remembered. The **first** growth
    /// after a collection is deliberately exempt (the C's single bounded fragment,
    /// `delay_garbage_collection`): it is granted whatever the cap, and it can only take the chain
    /// to `base + step` — the capacity the collection installed plus one one-step space for this
    /// request. The bound is therefore `max(cap, base + step)`, plus the mature generation (which
    /// the growth loop cannot touch).
    fn bound(&self) -> usize {
        self.cap.max(self.base + self.step) + self.mature
    }
}

/// Runs one measured growth loop: an establishing collection, then `events` growth events with
/// no collection.
fn run(kind: MatureKind, policy: HeapPolicy, live_words: usize, hint: usize, events: usize) -> Row {
    let mut c = any_for(kind, policy.clone());
    let (_roots, base) = establish(&mut c, live_words);
    let mature = c.mature_capacity_words();
    let cap = policy.chain_cap_words(policy.young_minor_target_words(live_words));
    let step = policy.next_words(hint, 0);
    let granted = drive_growth(&mut c, hint, events);
    Row {
        scenario: "",
        kind,
        hint,
        events,
        base,
        mature,
        cap,
        step,
        spaces: c.young_space_count(),
        granted,
        heap_size: c.heap_size_words(),
        total: c.total_heap_size_words(),
    }
}

/// The measured table, as one string, so a single assertion failure reports every figure.
fn report(title: &str, rows: &[Row]) -> String {
    let mut s = format!("\n{title}\n");
    s.push_str(&format!(
        "  {:<34} {:<8} {:>7} {:>7} {:>6} {:>7} {:>7} {:>6} {:>7} {:>10} {:>12} {:>12}\n",
        "scenario",
        "kind",
        "request",
        "events",
        "base",
        "cap",
        "step",
        "granted",
        "spaces",
        "heap_size",
        "total_heap",
        "bound"
    ));
    for r in rows {
        s.push_str(&format!(
            "  {:<34} {:<8} {:>7} {:>7} {:>6} {:>7} {:>7} {:>6} {:>7} {:>10} {:>12} {:>12}   \
             ({:.1}x the base, {:.1}x the bound)\n",
            r.scenario,
            format!("{:?}", r.kind),
            r.hint,
            r.events,
            r.base,
            r.cap,
            r.step,
            r.granted,
            r.spaces,
            r.heap_size,
            r.total,
            r.bound(),
            r.heap_size as f64 / r.base as f64,
            r.total as f64 / r.bound() as f64,
        ));
    }
    s
}

// ---------------------------------------------------------------------------
// 1. The reported capacity must stay live-set bounded, and flat in the event count
// ---------------------------------------------------------------------------

/// The headline: a post-collection growth loop must not ratchet the reported heap size.
///
/// The two scenarios are the two request shapes a host produces — the copy-out gate's
/// `GROW_MIN_WORDS`-floored request against a default 233-word nursery, and a small request
/// against a nursery sized by the policy's own floor — each measured at three event counts, with
/// a true `N` / `2N` pair per scenario.
///
/// # Why the ladder stops where it does
///
/// The reported figure is **flat** once the chain cap refuses (each further event is a refusal
/// that changes nothing), so more events would report the same numbers. The ladder still reaches
/// three points per scenario to show the plateau, and the 50 000-event case below shows it holds
/// arbitrarily far out — and that the loop is not quadratic.
#[test]
fn the_reported_heap_size_does_not_ratchet_under_a_growth_loop() {
    // The shape of `beam_rt`'s copy-out growth: an empty establishing live set on the crate
    // default nursery (233 words), then ≥1024-word requests.
    let host_policy = HeapPolicy::default();
    // A nursery at the policy floor, so a long run stays cheap in bytes while the shape is the
    // same: `need_reserve_words == 64` ⇒ `nursery_floor_words() == 90`.
    let floor_policy = HeapPolicy {
        initial_young_words: 90,
        ..HeapPolicy::default()
    };
    assert_eq!(
        floor_policy.nursery_floor_words(),
        90,
        "the floor-sized scenario really starts at the crate floor"
    );

    /// The host-shape ladder: `100 -> 200` is its `N` / `2N` pair.
    const HOST_EVENTS: [usize; 3] = [100, 200, 1_000];
    /// The small-request ladder: `1_000 -> 2_000` is its `N` / `2N` pair.
    const FLOOR_EVENTS: [usize; 3] = [1_000, 2_000, 20_000];

    let mut rows: Vec<Row> = Vec::new();
    for kind in KINDS {
        for events in HOST_EVENTS {
            let mut r = run(kind, host_policy.clone(), 0, HOST_GROW_MIN_WORDS, events);
            r.scenario = "host request (>=1024), 233 nursery";
            rows.push(r);
        }
        for events in FLOOR_EVENTS {
            let mut r = run(kind, floor_policy.clone(), 0, SMALL_REQUEST, events);
            r.scenario = "small request (32), 90 nursery";
            rows.push(r);
        }
    }

    let table = report(
        "the nursery chain under a post-collection growth loop (no collection between events):",
        &rows,
    );
    // (1a) Every row stays within the rule's own bound: `max(chain cap, base + one-step space)`.
    let over: Vec<&Row> = rows.iter().filter(|r| r.total > r.bound()).collect();
    assert!(
        over.is_empty(),
        "the reported total heap size must stay within the growth seam's own bound \
         (`max(HeapPolicy::chain_cap_words(live target), base + one one-step space)`, i.e. {}x the \
         live-set target — `YOUNG_SHRINK_FACTOR` — plus at most the one unconditional fragment a \
         collection's first growth may install) plus the mature generation; {} of {} rows exceed \
         it — the growth loop ratchets the account: {table}",
        YOUNG_SHRINK_FACTOR,
        over.len(),
        rows.len()
    );

    // (1a') The rule stated exactly: a growth granted behind a retained space never leaves the
    // chain past the cap, so the only way to be outside it is the one unconditional first grant.
    let outside: Vec<&Row> = rows
        .iter()
        .filter(|r| r.heap_size > r.cap && r.granted > 1)
        .collect();
    assert!(
        outside.is_empty(),
        "a chain that keeps a retained space may only grow inside `chain_cap_words(live target)` \
         ({} x the live-set target = {{cap}}), so any row outside it must have made exactly one \
         growth (the C's one bounded fragment); {} of {} rows grew past the cap more than once: \
         {table}",
        YOUNG_SHRINK_FACTOR,
        outside.len(),
        rows.len()
    );

    // (1b) The figure is flat in the number of growth events: N and 2N must agree, because the
    // number of growth events is not a workload property. The pairs are the first two rungs of
    // each ladder; the third rung shows the plateau continues.
    let mut dyads: Vec<(&Row, &Row)> = Vec::new();
    for kind in KINDS {
        for hint in [HOST_GROW_MIN_WORDS, SMALL_REQUEST] {
            let ladder: Vec<&Row> = rows
                .iter()
                .filter(|r| r.kind == kind && r.hint == hint)
                .collect();
            assert_eq!(ladder[1].events, 2 * ladder[0].events, "an N / 2N pair");
            dyads.push((ladder[0], ladder[1]));
        }
    }
    let moving: Vec<(&Row, &Row)> = dyads
        .iter()
        .copied()
        .filter(|(a, b)| a.total != b.total || a.heap_size != b.heap_size)
        .collect();
    assert!(
        moving.is_empty(),
        "the reported total heap size must be identical at N and 2N growth events (flat in the \
         event count); {} of {} pairs move — each extra event strands another space's capacity: \
         {table}",
        moving.len(),
        dyads.len()
    );

    // (1c) The cap really bites: every scenario ran more events than the seam granted, so the
    // plateau above is the chain cap's doing and not an under-run ladder.
    let never_refused: Vec<&Row> = rows
        .iter()
        .filter(|r| r.granted == r.events || r.spaces < 2)
        .collect();
    assert!(
        never_refused.is_empty(),
        "every row must have exercised the chain cap (a granted growth and then a refusal), so \
         the flatness is the rule's and not a short ladder: {table}"
    );
}

// ---------------------------------------------------------------------------
// 2. A successful growth must make strict progress
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
#[test]
fn a_successful_growth_enlarges_the_space_it_replaces() {
    let policy = HeapPolicy {
        initial_young_words: 90,
        ..HeapPolicy::default()
    };
    let mut traces: Vec<String> = Vec::new();
    let mut stalls: Vec<(MatureKind, usize, Step)> = Vec::new();
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
                // A refused event installs nothing at all (its `after` is the `before` the
                // helper substitutes), so it is excluded here; the refusal itself is pinned by
                // `the_growth_seam_bounds_the_chain_without_collecting`.
                if st.total_after != st.total_before && st.after <= st.before {
                    stalls.push((kind, hint, *st));
                }
            }
        }
    }
    let table = traces.join("");

    // (2a) Strict progress in the current space's capacity.
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

    // (2b) Because the chain gains exactly one space per granted growth, the summed capacity must
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
// 3. The paired A/B: only a measured live set bounds the chain
// ---------------------------------------------------------------------------

/// The same growth loop, from the same nursery and the same request, with **one** difference:
/// whether a collection has run first.
///
/// * **No collection yet** — the nursery has no remembered live-set target
///   (`live_target_words == None`), so the chain cap is derived from the request's own size
///   (`YOUNG_SHRINK_FACTOR × target`): every event is granted and the installed capacity climbs
///   the growth table.
/// * **After a collection** — the cap is `YOUNG_SHRINK_FACTOR × live_target`, so the first growth
///   (the C's one bounded fragment) is granted and every further one is **refused** until a
///   collection collapses the chain. The reported capacity is flat from then on.
///
/// The earlier form of this test isolated a *defect*: with the live-set bound applied to the
/// whole choice, a collected nursery installed a space exactly the size of the one it retired on
/// every event. The pair below now isolates the **fix** — the bound narrows the choice, it no
/// longer discards the progress, and the cap is what stops the chain.
#[test]
fn only_a_measured_live_set_bounds_the_chain() {
    // The nursery geometry is the crate default (a 233-word nursery). The mature generation is
    // enlarged only so the **retry** collection at the end can run at all: a minor evacuates the
    // whole chain's occupancy, so its mature space has to have room for it (the typed
    // `heap_exhausted` otherwise, which is a different contract and not what this case pins). The
    // mature capacity does not enter any nursery figure asserted here.
    let policy = HeapPolicy {
        initial_mature_words: 32768,
        ..HeapPolicy::default()
    };
    let hint = SMALL_REQUEST;
    const EVENTS: usize = 5;

    let mut fresh: Vec<usize> = Vec::new();
    let mut after: Vec<usize> = Vec::new();
    let mut refusals = 0usize;
    for kind in KINDS {
        // The uncollected control: a fresh nursery, never collected.
        let mut c = any_for(kind, policy.clone());
        assert_eq!(c.young().capacity_words(), 233, "{kind:?}: the default nursery");
        assert!(!c.collection_debt(), "{kind:?}: no chain yet");
        for _ in 0..EVENTS {
            let (_before, installed) = growth_event(&mut c, hint);
            fresh.push(installed.expect("an uncollected nursery's chain is bounded by the request"));
        }
        // The case: identical policy, identical loop — one collection first.
        let mut c = any_for(kind, policy.clone());
        let (roots, base) = establish(&mut c, 0);
        assert_eq!(base, 233, "{kind:?}: the collection kept the 233-word nursery");
        for _ in 0..EVENTS {
            let (_before, installed) = growth_event(&mut c, hint);
            match installed {
                Some(words) => after.push(words),
                None => refusals += 1,
            }
        }
        // The refused loop left the chain collected-and-retryable: one collection collapses it and
        // clears the debt, which is what a host does next.
        assert!(c.collection_debt(), "{kind:?}: the granted growth is debt");
        let mut roots = roots;
        c.collect_minor(&mut roots).expect("the retry collection");
        assert_eq!(c.young_space_count(), 1, "{kind:?}");
        assert!(!c.collection_debt(), "{kind:?}");
    }

    let detail = format!(
        "\n  request {hint} words, {EVENTS} growth events each, policy {policy:?}\n  \
         uncollected nursery, installed spaces: {fresh:?}\n  \
         collected nursery,   installed spaces: {after:?} (then {refusals} refusals)\n"
    );

    // (3a) Without a remembered live set the seam climbs the growth table on every event: the cap
    // is derived from the request, so it does not bite here.
    for pair in fresh.chunks(EVENTS) {
        assert_eq!(pair.len(), EVENTS, "an uncollected nursery grants every event");
        assert!(
            pair.windows(2).all(|w| w[1] > w[0]),
            "an uncollected nursery must climb the growth table on every event (the doubling \
             fallback is live when no live set has been measured){detail}"
        );
    }
    // (3b) With one, the chain cap bounds the loop: some events are granted, each strictly larger
    // than its predecessor, and the rest are refused (never a same-size install).
    assert!(!after.is_empty(), "the first growth is always granted{detail}");
    assert!(
        refusals >= KINDS.len(),
        "a collected nursery's chain must hit the cap within {EVENTS} events{detail}"
    );
    for pair in after.chunks(after.len() / KINDS.len()) {
        assert!(
            pair.windows(2).all(|w| w[1] > w[0]),
            "every granted growth installs a strictly larger space{detail}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. The green control: a collection still collapses the chain onto the live set
// ---------------------------------------------------------------------------

/// Whatever the growth seam did, a collection must still take the chain back to **exactly one**
/// space whose capacity is what the heap-size policy prescribes for that chain, and clear the
/// collection debt.
///
/// This is the behaviour the fix must not disturb: the collapse seam is the only reclamation
/// point of the chain, so it has to keep working while the growth seam is bounded. Both collapse
/// seams are exercised — a **minor** (sized from the young live set) and a **fullsweep** (sized
/// from the whole-heap live set), including its shrink branch.
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
            let granted = drive_growth(&mut c, hint, 20);
            let chained_spaces = c.young_space_count();
            let chained_words = c.heap_size_words();
            assert!(
                granted >= 1 && chained_spaces == granted + 1,
                "{kind:?}: the chain is live ({granted} granted growths, {chained_spaces} spaces)"
            );
            assert!(c.collection_debt(), "{kind:?}: the chain is debt");

            // (4a) A **minor** over an empty young live set: the policy's own answer for the
            // collected chain — its target when the chain is past the keep band, the chain's own
            // capacity when it is inside it (the cap keeps it inside).
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

            // (4b) A **minor** over a non-empty young live set: the target is the live set's own
            // once the chain is below it.
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

            // (4c) A **fullsweep** over a live set far **below** the chain: the shrink branch. A
            // fresh collector (with a nursery large enough for the establishing term) establishes
            // a 401-word live set — so the chain cap and the capacity it leaves are that live
            // set's — builds a 40-word live term, drops the 401-word root (which makes it
            // garbage), and only then grows the chain past the band. The fullsweep therefore
            // measures a live set far below the chain it inherits and gives the memory back onto
            // the whole-heap target. The surviving 40-word root sits in a **retired** space by
            // then, so this also pins that a fullsweep evacuates the whole chain, not just the
            // current space.
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
            drive_growth(&mut c, hint, 20);
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

// ---------------------------------------------------------------------------
// 5. The new properties: a bounded, non-quadratic, non-destructive growth seam
// ---------------------------------------------------------------------------

/// The growth seam's contract, pinned directly:
///
/// * **(i) strict progress** — every granted growth installs a space strictly larger than the one
///   it retires (and the chain gains exactly one space);
/// * **(ii) a refusal is inert** — `capacity_words()`, the current space's base and the chain
///   length are byte-for-byte unchanged, and the collection debt stays true so the caller knows to
///   collect;
/// * **(iii) the loop is not quadratic** — 50 000 growth events against a capped chain complete
///   quickly (they are refusals once the cap is reached) and leave the reported figures flat, which
///   the earlier `empty_retired_for` chain scan could not do.
#[test]
fn the_growth_seam_bounds_the_chain_without_collecting() {
    let policy = HeapPolicy::default();
    let hint = SMALL_REQUEST;

    for kind in KINDS {
        // (i) + (ii): the granted events, then the first refusal.
        let mut c = any_for(kind, policy.clone());
        let (_roots, base) = establish(&mut c, 0);
        let cap = policy.chain_cap_words(base);
        let mut granted = 0usize;
        loop {
            let (before, after) = growth_event(&mut c, hint);
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
                        c.heap_size_words() <= cap,
                        "{kind:?}: the chain stays within the {}x-live-set cap ({cap}), got {}",
                        YOUNG_SHRINK_FACTOR,
                        c.heap_size_words()
                    );
                }
                None => break,
            }
        }
        assert!(granted >= 1, "{kind:?}: the first growth is always granted");

        // (ii) The refusal is inert: nothing was allocated, retired, moved or forgotten.
        let (spaces, capacity, base_addr, used) = (
            c.young_space_count(),
            c.heap_size_words(),
            c.young().base(),
            c.heap_used_words(),
        );
        assert!(c.collection_debt(), "{kind:?}: the chain stays debt");
        for _ in 0..8 {
            let (before, after) = growth_event(&mut c, hint);
            assert!(after.is_none(), "{kind:?}: the cap keeps refusing");
            assert_eq!(before, c.young().capacity_words(), "{kind:?}: same current space");
        }
        assert_eq!(c.young_space_count(), spaces, "{kind:?}: a refusal retires nothing");
        assert_eq!(c.heap_size_words(), capacity, "{kind:?}: a refusal consumes no capacity");
        assert_eq!(c.young().base(), base_addr, "{kind:?}: a refusal replaces no space");
        assert_eq!(c.heap_used_words(), used, "{kind:?}: a refusal grants no room");

        // (iii) 50 000 events: every one after the cap is a refusal, so the loop is fast and the
        // reported figures do not move.
        let before_figures = (c.heap_size_words(), c.total_heap_size_words(), c.young_space_count());
        let start = std::time::Instant::now();
        for _ in 0..50_000 {
            let (_before, after) = growth_event(&mut c, hint);
            assert!(after.is_none(), "{kind:?}: the chain stays capped");
        }
        let elapsed = start.elapsed();
        assert_eq!(
            (c.heap_size_words(), c.total_heap_size_words(), c.young_space_count()),
            before_figures,
            "{kind:?}: 50 000 more growth events must not move the reported figures"
        );
        assert_eq!(c.young_space_count(), granted + 1, "{kind:?}: the chain length is unchanged");
        // A ceiling, not a benchmark: the loop is 50 000 refusals of an `O(log)` lookup, and a
        // quadratic regression would need minutes rather than the milliseconds it takes here.
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "{kind:?}: 50 000 growth events took {elapsed:?} — the growth path scans the chain"
        );
    }
}
