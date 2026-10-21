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
//! (`live_target_words`) that the seam is documented to be clamped by.
//!
//! Three properties must hold, and this file pins all three:
//!
//! 1. **The reported capacity stays live-set bounded** — [`HeapObservable::total_heap_size_words`]
//!    (and [`HeapObservable::heap_size_words`]) after `N` growth events stay within a small
//!    multiple of the capacity the establishing collection installed, and are **flat in `N`**:
//!    identical at `N` and `2N`, because the number of growth events is not a workload property.
//!    Today they grow linearly.
//! 2. **A successful growth makes strict progress** — the fresh current space is strictly larger
//!    than the space it replaces, and because the chain gains a whole space
//!    ([`AnyCollector::young_space_count`] +1) the summed capacity grows by **at least** that
//!    space's own size. A growth event that installs a space no larger than the one it retires
//!    makes no progress in capacity while appending a full space to the account. Today it does
//!    exactly that.
//! 3. **A collection still collapses the chain onto its live-set target** (the green control) —
//!    after [`AnyCollector::collect`] / [`AnyCollector::collect_minor`] the chain is exactly one
//!    space ([`AnyCollector::young_space_count`] == 1), its capacity is the live-set-derived
//!    target and [`AnyCollector::collection_debt`] is false. This already holds and must keep
//!    holding.
//!
//! Every corpus here is deterministic — assertions are on word counts, never on wall time — and
//! every case is driven through both mature strategies wherever the cost allows (the nursery
//! chain is shared by [`AnyCollector::Copying`] and [`AnyCollector::Immix`]).
//!
//! # What is measured today (the diagnosis this file was written for)
//!
//! **The `live_target_words` clamp is what produces the equal-size installation.** In
//! `Nursery::grow` the size rule computes, in order: the one-step size
//! `one_step = next_words(need).max(capacity)`; a target that is `one_step` when that enlarges
//! the space and otherwise at least a **doubling** `next_words(2 × capacity)`; and then, when the
//! nursery has collected,
//!
//! ```text
//! target = target.min(one_step.max(live_target))      // src/nursery.rs
//! ```
//!
//! After a collapse the nursery's capacity is `>= live_target`, and for a request smaller than
//! the space `one_step == capacity`; the clamp therefore evaluates to `capacity` and **discards
//! the doubling**. The installed space is exactly the size of the one it retires, while the chain
//! sum (`heap_size_words`, the reported `heap_size`) gains a whole capacity. Without the clamp
//! the same loop climbs the growth table (`610, 1598, 4185, 10958, 28690, …`), so the mechanism
//! is isolated by the paired A/B below rather than inferred.
//!
//! **Two things to keep in mind when fixing it.** (1) The requested *larger* request shape does
//! make progress — once — and then stacks: a ≥1024-word request installs one table step
//! (`1598`), and every further event at the same capacity installs another `1598`-word space.
//! (2) Properties 1 and 2 below are in **tension**, so neither a size rule alone satisfies both:
//! a seam that must strictly enlarge the space it replaces on every event necessarily doubles,
//! and a doubling chain sum is `O(2^N)`. Bounding the reported capacity by the live set while
//! never collecting therefore needs the seam to **reclaim** retired capacity (a re-adoption that
//! does not require the space to be empty, a release, or a mandated collection), not merely a
//! different size. These cases pin the target properties; they do not presume a fix's shape.

use beam_gc::prelude::{
    AnyCollector, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind,
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
/// Returns `(before, after)` — the capacity of the current space immediately before the growth
/// and of the fresh space it installed.
fn growth_event(c: &mut AnyCollector, hint: usize) -> (usize, usize) {
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
    c.grow_nursery(hint).expect("a growth event");
    let after = c.young().capacity_words();
    (before, after)
}

/// Drives `events` growth events with **no collection in between**, returning the collector for
/// further inspection.
fn drive_growth(c: &mut AnyCollector, hint: usize, events: usize) {
    for _ in 0..events {
        let _ = growth_event(c, hint);
    }
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
    /// `young_space_count()` after the loop.
    spaces: usize,
    /// `heap_size_words()` — the chain's **summed** nursery capacity.
    heap_size: usize,
    /// `total_heap_size_words()` — the chain plus the mature generation.
    total: usize,
}

impl Row {
    /// The live-set bound this file holds the reported capacity to: `8 ×` the capacity the
    /// establishing collection installed (`YOUNG_SHRINK_FACTOR`, the heap-size policy's own
    /// hysteresis width — the largest nursery a *collection* may leave in place).
    fn bound(&self) -> usize {
        8 * self.base
    }

    /// The capacity the loop stranded **per growth event**: the summed nursery capacity beyond
    /// the base, divided by the number of events.
    fn stranded_per_event(&self) -> f64 {
        (self.heap_size.saturating_sub(self.base)) as f64 / self.events as f64
    }
}

/// Runs one measured growth loop: an establishing collection, then `events` growth events with
/// no collection.
fn run(kind: MatureKind, policy: HeapPolicy, live_words: usize, hint: usize, events: usize) -> Row {
    let mut c = any_for(kind, policy);
    let (_roots, base) = establish(&mut c, live_words);
    drive_growth(&mut c, hint, events);
    Row {
        scenario: "",
        kind,
        hint,
        events,
        base,
        spaces: c.young_space_count(),
        heap_size: c.heap_size_words(),
        total: c.total_heap_size_words(),
    }
}

/// The measured table, as one string, so a single assertion failure reports every figure.
fn report(title: &str, rows: &[Row]) -> String {
    let mut s = format!("\n{title}\n");
    s.push_str(&format!(
        "  {:<34} {:<8} {:>7} {:>8} {:>7} {:>8} {:>12} {:>14} {:>10}\n",
        "scenario",
        "kind",
        "request",
        "events",
        "base",
        "spaces",
        "heap_size",
        "total_heap",
        "8*base"
    ));
    for r in rows {
        s.push_str(&format!(
            "  {:<34} {:<8} {:>7} {:>8} {:>7} {:>8} {:>12} {:>14} {:>10}   ({:.1} words/event stranded, {:.1}x the bound)\n",
            r.scenario,
            format!("{:?}", r.kind),
            r.hint,
            r.events,
            r.base,
            r.spaces,
            r.heap_size,
            r.total,
            r.bound(),
            r.stranded_per_event(),
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
/// # Why the event ladder stops where it does
///
/// The measured figure is **exactly linear** in the event count (`base + N × installed`), so
/// three points already fix the line and a fourth at 100 000 events would report nothing new —
/// while costing far more than one more point should: `Nursery::grow` scans the whole retired
/// chain on every call (`empty_retired_for`, which has no empty space to find here), so a
/// growth loop that never collects is **O(N²)** on top of being O(N) in stranded capacity. The
/// 100 000-event point measured ~40 s per strategy on its own; the ladder below keeps the file
/// fast. (That quadratic scan is a second, separate cost of the same kind and is recorded in the
/// crate's known structural debt, not asserted here.)
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

    // (1a) Every row stays within the live-set bound.
    let over: Vec<&Row> = rows.iter().filter(|r| r.total > r.bound()).collect();
    assert!(
        over.is_empty(),
        "the reported total heap size must stay within 8x the capacity the establishing \
         collection installed (the heap-size policy's own `YOUNG_SHRINK_FACTOR` band); \
         {} of {} rows exceed it — the growth loop ratchets the account: {table}",
        over.len(),
        rows.len()
    );

    // (1b) The figure is flat in the number of growth events: N and 2N must agree, because the
    // number of growth events is not a workload property. The pairs are the first two rungs of
    // each ladder.
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
        .filter(|(a, b)| a.total != b.total)
        .collect();
    assert!(
        moving.is_empty(),
        "the reported total heap size must be identical at N and 2N growth events (flat in the \
         event count); {} of {} pairs move — each extra event strands another space's capacity: \
         {table}",
        moving.len(),
        dyads.len()
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
                after,
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
                if st.after <= st.before {
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
         (a same-size install appends a whole capacity to the chain and grants no room); {} of \
         {} growth events make no progress — {}\n{table}",
        stalls.len(),
        KINDS.len() * 2 * 4,
        first.unwrap_or_default()
    );

    // (2b) Because the chain gains exactly one space per growth, the summed capacity must grow by
    // at least the new space's own size.
    let mut shortfalls: Vec<(MatureKind, usize, Step)> = Vec::new();
    for kind in KINDS {
        for hint in [SMALL_REQUEST, HOST_GROW_MIN_WORDS] {
            for st in progress_trace(kind, policy.clone(), 0, hint, 4) {
                let delta = st.total_after.saturating_sub(st.total_before);
                if delta < st.after {
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
// 3. The paired A/B: the remembered live-set target is what defeats the doubling rule
// ---------------------------------------------------------------------------

/// The same growth loop, from the same 233-word nursery and the same request, with **one**
/// difference: whether a collection has run first.
///
/// * **No collection yet** — the nursery has no remembered live-set target
///   (`live_target_words == None`), so the seam takes its plain curated rule: the one-step size
///   when that enlarges the space, and at least a **doubling** when it does not. The installed
///   capacity climbs the growth table every event, which is progress.
/// * **After a collection** — the nursery carries its live-set target and the seam clamps the
///   fresh space to `target.min(one_step.max(live_target))`. With `one_step == capacity` (a
///   request smaller than the space) and `live_target <= capacity`, the clamp evaluates to
///   `capacity` and **discards the doubling**: the fresh space is exactly as large as the one it
///   retires, every event.
///
/// The second half is the red assertion (it repeats test 2's finding as a paired measurement);
/// the first half is green and pins the contrast, so the cause is isolated to the clamp rather
/// than to the doubling rule.
#[test]
fn the_remembered_live_target_is_what_defeats_the_doubling_rule() {
    let policy = HeapPolicy::default();
    let hint = SMALL_REQUEST;
    const EVENTS: usize = 5;

    let mut fresh: Vec<usize> = Vec::new();
    let mut after: Vec<usize> = Vec::new();
    for kind in KINDS {
        // The control: a fresh nursery, never collected.
        let mut c = any_for(kind, policy.clone());
        assert_eq!(c.young().capacity_words(), 233, "{kind:?}: the default nursery");
        assert!(!c.collection_debt(), "{kind:?}: no chain yet");
        for _ in 0..EVENTS {
            let (_before, installed) = growth_event(&mut c, hint);
            fresh.push(installed);
        }
        // The case: identical policy, identical loop — one collection first.
        let mut c = any_for(kind, policy.clone());
        let (_roots, base) = establish(&mut c, 0);
        assert_eq!(base, 233, "{kind:?}: the collection kept the 233-word nursery");
        for _ in 0..EVENTS {
            let (_before, installed) = growth_event(&mut c, hint);
            after.push(installed);
        }
    }

    let detail = format!(
        "\n  request {hint} words, {EVENTS} growth events each, policy {policy:?}\n  \
         uncollected nursery, installed spaces: {fresh:?}\n  \
         collected nursery,   installed spaces: {after:?}\n"
    );

    // Green: without a remembered live set the seam climbs (the curated rule works).
    for pair in fresh.chunks(EVENTS) {
        assert!(
            pair.windows(2).all(|w| w[1] > w[0]),
            "an uncollected nursery must climb the growth table on every event (the doubling \
             fallback is live when no live set has been measured){detail}"
        );
    }
    // Red: with one, the doubling is clamped away and the seam installs same-size spaces.
    let stalled: Vec<usize> = after
        .iter()
        .enumerate()
        .filter(|(i, installed)| {
            let previous = if i % EVENTS == 0 {
                233
            } else {
                after[i - 1]
            };
            **installed <= previous
        })
        .map(|(_i, installed)| *installed)
        .collect();
    assert!(
        stalled.is_empty(),
        "a nursery that has measured a live set must still enlarge its space on every growth \
         event; {} of {} events installed a space no larger than the one they replaced{detail}",
        stalled.len(),
        after.len()
    );
}

// ---------------------------------------------------------------------------
// 4. The green control: a collection still collapses the chain onto the live set
// ---------------------------------------------------------------------------

/// Whatever the growth seam did, a collection must still take the chain back to **exactly one**
/// space whose capacity is the live-set-derived target, and clear the collection debt.
///
/// This half already holds and is the behaviour the fix must not disturb: the collapse seam is
/// the only reclamation point of the chain, so it has to keep working while the growth seam is
/// made bounded. Both collapse seams are exercised — a **minor** (sized from the young live set)
/// and a **fullsweep** (sized from the whole-heap live set).
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
            drive_growth(&mut c, hint, 20);
            let chained_spaces = c.young_space_count();
            let chained_words = c.heap_size_words();
            assert_eq!(chained_spaces, 21, "{kind:?}: the chain is live");
            assert!(c.collection_debt(), "{kind:?}: the chain is debt");

            // (4a) A **minor** over an empty young live set: the target is the policy's floor.
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
                policy.nursery_floor_words(),
                "{kind:?}: the collapsed capacity is the empty live set's target"
            );
            assert_eq!(
                c.heap_size_words(),
                c.young().capacity_words(),
                "{kind:?}: the chain sum is one space again"
            );

            // (4b) A **minor** over a non-empty young live set: the target is the live set's own.
            let live_words = 40;
            let mut roots = build_live(&mut c, live_words);
            let pre = c.young().capacity_words();
            let stats = c.collect_minor(&mut roots).expect("the live-set collapse");
            assert_eq!(stats.words_copied, live_words, "{kind:?}");
            assert_eq!(c.young_space_count(), 1, "{kind:?}");
            assert_eq!(
                policy.young_after_collection(pre, live_words),
                policy.young_minor_target_words(live_words),
                "{kind:?}: a collapse from below the target installs the live-set target"
            );
            assert_eq!(
                c.heap_size_words(),
                policy.young_minor_target_words(live_words),
                "{kind:?}: the collapsed capacity is the live set's target"
            );

            // (4c) A **fullsweep** over the same shape: the whole-heap target, still one space.
            // Enough growth events that the chain sum is far above the band, so the fullsweep's
            // shrink branch (`capacity > 8 × target`) is the one that fires.
            drive_growth(&mut c, hint, 20);
            assert_eq!(c.young_space_count(), 21, "{kind:?}");
            let mut roots = build_live(&mut c, live_words);
            let stats = c.collect_major(&mut roots).expect("the fullsweep collapse");
            assert_eq!(c.young_space_count(), 1, "{kind:?}");
            assert!(!c.collection_debt(), "{kind:?}");
            assert_eq!(
                policy.young_after_fullsweep(policy.nursery_floor_words(), live_words),
                policy.young_target_words(live_words),
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
