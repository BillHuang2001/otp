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

//! **The unscanned-root valve's invocation census** — an env-gated count of *how often* the
//! reachability walk runs, *which* door calls it, *how big* each walk is and *what* it answers.
//!
//! # Why this exists
//!
//! A profile of a *minimal* run (`beam_rt -noshell -eval 'halt().'`) attributes a large share of
//! its retired instructions to the valve's descent ([`super::gc_walk`]) — but a profile says
//! nothing about the **shape** of the work: whether the count is dominated by many tiny walks or a
//! few huge ones, whether the per-word phase or the descent owns the time, which door issues them,
//! and whether [`Visited`](super::gc_walk) is starved (a cold table rebuilt for a handful of
//! objects) or saturated. Those are questions about *counts*, so they are answered with counters,
//! not with timings.
//!
//! # The switch
//!
//! Set `BEAM_RT_VALVE_TRACE` to any value ([`enabled`]) to switch it on. It is a diagnostic of the
//! same kind as the crate's other channels (`BEAM_RT_GC_TRACE`/`BEAM_RT_MEM_TRACE` in
//! [`crate::debug_trace`]): **silent unless the variable is set**, nothing depends on it, and
//! nothing emits unconditionally. Every recording entry point tests [`enabled`] first — a
//! [`OnceLock`]-cached flag, so with the variable unset the cost is one relaxed load and one
//! perfectly predicted branch per event, and nothing else.
//!
//! # The report
//!
//! The counters of one thread live in a thread-local ([`Trace`]) whose `Drop` prints the aggregate
//! block to **stderr**, prefixed `[rt-valve]`. Only the **runtime driver thread** (`src/main.rs`
//! spawns it as `"beam_rt-driver"`) prints: every other thread keeps its own counters silently,
//! because the valve runs where the execution engine runs, and that is the driver thread. The
//! thread-local is created on the first *counted* event, so a run with no valve activity at all
//! (`-version`) creates nothing and prints nothing.
//!
//! # What is counted
//!
//! * **per door** ([`ValveDoor`]): valve calls, descents actually run, phase-1-only calls,
//!   declines (split into the ones the always-run per-word phase found and the ones only the
//!   descent found), `foreign_free` answers (the ones that may *close* `pid`'s descent latch), the
//!   visited-set size summed and peaked over the door's walks;
//! * **globally**: the root words the five [`Process`](beam_process::Process) root families seeded,
//!   the [`Visited`](super::gc_walk) table's inserts/probes/grows, the object expansions, the
//!   per-walk size histogram (once over every call, once over descents alone), and the two tiers of
//!   the scanned test ([`super::gc::word_region`]) — the aggregate fast reject versus the exact
//!   per-region fallback the profile shows as `Nursery::contains`.
//!
//! Nothing here changes a decision: a decline is still a decline and
//! [`ValveVerdict::foreign_free`](super::gc_walk::ValveVerdict) is still reported only for a
//! descent over the caller's complete root set. This module **only observes**.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// The thread that runs the runtime — the only one whose counters are printed (`src/main.rs`).
const DRIVER_THREAD: &str = "beam_rt-driver";

/// Whether the valve census is on (`BEAM_RT_VALVE_TRACE` present).
///
/// Resolved **once** per OS process, like every other `beam_rt` diagnostic
/// ([`crate::debug_trace`]): one atomic load and one predicted branch per event after the first
/// call.
#[inline]
pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BEAM_RT_VALVE_TRACE").is_some())
}

/// Which call site reached the walk. Every variant is one production entry point, so a report can
/// say *who* pays for the descents.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ValveDoor {
    /// `super::gc::ensure_heap` — the narrow admission point (x prefix + stack, no FR file).
    EnsureHeap,
    /// `super::gc::ensure_heap_full` — the engine's `test_heap`/`allocate_heap` door.
    EnsureHeapFull,
    /// `super::gc::alloc_words_rooted` — the engine's rooted term-construction seam.
    AllocWordsRooted,
    /// `super::gc::reds_door` — the slice-boundary (reductions) collection door.
    RedsBoundary,
    /// `Runtime::serve_gc_request` — the runner's park arm.
    ServeGcRequest,
    /// `RuntimeContext::delivery_collection_is_unsound` — a message delivery's receiver test.
    SendDelivery,
    /// `RuntimeCollectionGate::declines_relocation` — the `beam_process` signal sites.
    CollectionGate,
    /// `gc::bin_vheap::reclaim`'s **collection** valve.
    BinVheapCollection,
    /// `gc::bin_vheap::reclaim`'s **prune-only** valve (`reaches_an_unscanned_word`).
    BinVheapPrune,
    /// `gc::resources`' **prune-only** valve.
    ResourcePruneValve,
    /// `gc::resources`' reachability walk (`reachable_resource_words`).
    ResourceWalk,
    /// A call site that named no door (the unit suites' direct calls to a valve entry point).
    Other,
}

/// How many doors there are — the width of the per-door table.
pub(crate) const DOORS: usize = 12;

impl ValveDoor {
    /// Every door, in report order.
    pub(crate) const ALL: [ValveDoor; DOORS] = [
        ValveDoor::EnsureHeap,
        ValveDoor::EnsureHeapFull,
        ValveDoor::AllocWordsRooted,
        ValveDoor::RedsBoundary,
        ValveDoor::ServeGcRequest,
        ValveDoor::SendDelivery,
        ValveDoor::CollectionGate,
        ValveDoor::BinVheapCollection,
        ValveDoor::BinVheapPrune,
        ValveDoor::ResourcePruneValve,
        ValveDoor::ResourceWalk,
        ValveDoor::Other,
    ];

    /// This door's slot in [`Counts::doors`].
    fn index(self) -> usize {
        match self {
            ValveDoor::EnsureHeap => 0,
            ValveDoor::EnsureHeapFull => 1,
            ValveDoor::AllocWordsRooted => 2,
            ValveDoor::RedsBoundary => 3,
            ValveDoor::ServeGcRequest => 4,
            ValveDoor::SendDelivery => 5,
            ValveDoor::CollectionGate => 6,
            ValveDoor::BinVheapCollection => 7,
            ValveDoor::BinVheapPrune => 8,
            ValveDoor::ResourcePruneValve => 9,
            ValveDoor::ResourceWalk => 10,
            ValveDoor::Other => 11,
        }
    }

    /// The name the report prints for this door.
    fn label(self) -> &'static str {
        match self {
            ValveDoor::EnsureHeap => "EnsureHeap",
            ValveDoor::EnsureHeapFull => "EnsureHeapFull",
            ValveDoor::AllocWordsRooted => "AllocWordsRooted",
            ValveDoor::RedsBoundary => "RedsBoundary",
            ValveDoor::ServeGcRequest => "ServeGcRequest",
            ValveDoor::SendDelivery => "SendDelivery",
            ValveDoor::CollectionGate => "CollectionGate",
            ValveDoor::BinVheapCollection => "BinVheapCollection",
            ValveDoor::BinVheapPrune => "BinVheapPrune",
            ValveDoor::ResourcePruneValve => "ResourcePruneValve",
            ValveDoor::ResourceWalk => "ResourceWalk",
            ValveDoor::Other => "Other",
        }
    }
}

/// The five [`Process`](beam_process::Process) root families
/// [`Walk::seed_process_roots`](super::gc_walk) seeds, in seeding order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RootFamily {
    /// `x[0..live)` + the whole Erlang stack + the host-published extra roots.
    Registers,
    /// Three words per queued message (`payload`/`from`/`token`).
    Mailbox,
    /// Two words per dictionary entry (key and value).
    Dict,
    /// The saved BIF/NIF arguments.
    SavedArgs,
    /// The pending-signal queue.
    Signals,
}

/// How many buckets the per-walk visited-set histogram has (see [`Labels::HIST`]).
pub(crate) const HIST_BUCKETS: usize = 8;

/// The histogram's bucket edges and names — the shape of a *typical* walk: an empty descent, a
/// handful of objects, a few hundred, thousands, or a whole live graph.
struct Labels;

impl Labels {
    /// One label per bucket, in order.
    const HIST: [&'static str; HIST_BUCKETS] = [
        "0",
        "1-16",
        "17-64",
        "65-256",
        "257-1k",
        "1k-4k",
        "4k-16k",
        ">16k",
    ];
}

/// The bucket `visited` objects falls in.
fn hist_bucket(visited: usize) -> usize {
    match visited {
        0 => 0,
        1..=16 => 1,
        17..=64 => 2,
        65..=256 => 3,
        257..=1024 => 4,
        1025..=4096 => 5,
        4097..=16384 => 6,
        _ => 7,
    }
}

/// One door's tallies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DoorCounts {
    /// Valve calls the door made.
    pub(crate) calls: u64,
    /// Calls that actually ran the descent (the caller's gate was open **and** the per-word phase
    /// found nothing).
    pub(crate) descents: u64,
    /// Calls whose caller gate was closed, so only the `O(roots)` per-word phase ran.
    pub(crate) phase1_only: u64,
    /// Declines the always-run per-word phase found (a root word is itself a foreign container).
    pub(crate) declined_phase1: u64,
    /// Declines only the descent found — the nested shape the walk exists for.
    pub(crate) declined_descent: u64,
    /// Answers of `foreign_free`: the descent ran and reached no container from a region the
    /// collector does not scan. These are the answers that may **close** `pid`'s descent latch.
    pub(crate) foreign_free: u64,
    /// The visited-set size summed over this door's walks.
    pub(crate) visited_sum: u64,
    /// The largest visited-set size of any one of this door's walks.
    pub(crate) visited_peak: u64,
}

impl DoorCounts {
    /// Every decline this door reported.
    fn declined(&self) -> u64 {
        self.declined_phase1 + self.declined_descent
    }
}

/// The whole census: one entry per door plus the runtime-wide tallies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counts {
    /// One entry per door ([`ValveDoor::ALL`]'s order).
    doors: [DoorCounts; DOORS],
    /// Root words each [`RootFamily`] seeded, summed over every walk.
    roots: [u64; 5],
    /// [`Visited`](super::gc_walk) inserts — one per object *considered* for the set, including the
    /// duplicates a shared or cyclic graph produces (so `inserts - distinct` is the sharing the
    /// walk collapsed).
    inserts: u64,
    /// Probe steps inside [`Visited::insert`](super::gc_walk): the sum of every open-addressing
    /// collision chain length. `probes / inserts` is the table's own load-quality figure.
    probes: u64,
    /// Inserts that found the key **already recorded** — the sharing a cyclic or diamond-shaped
    /// graph produces. `inserts - revisits` is the number of distinct objects visited.
    revisits: u64,
    /// Table grows (the double-and-re-insert).
    grows: u64,
    /// [`Walk::expand`](super::gc_walk) calls — the objects whose children were classified.
    expands: u64,
    /// The per-walk visited-set histogram over **every** valve call ([`Labels::HIST`]).
    hist_calls: [u64; HIST_BUCKETS],
    /// The per-walk visited-set histogram over the **descents alone** ([`Labels::HIST`]).
    hist_descents: [u64; HIST_BUCKETS],
    /// Words the aggregate fast reject (`ScannedBounds::definitely_unscanned`) classified as
    /// outside every scanned region — the cheap tier.
    tier1_foreign: u64,
    /// Words the aggregate **could not** decide, so the exact per-region tests ran.
    tier2_calls: u64,
    /// Of those, the ones the exact tests answered "outside every scanned region".
    tier2_foreign: u64,
    /// And the ones they answered "inside a scanned region" — every one of these is a
    /// `Nursery`/mature `contains` probe the aggregate could not save.
    tier2_scanned: u64,
}

impl Counts {
    /// The all-zero census (the `const` form the thread-local's initialiser needs).
    const ZERO: Counts = Counts {
        doors: [DoorCounts {
            calls: 0,
            descents: 0,
            phase1_only: 0,
            declined_phase1: 0,
            declined_descent: 0,
            foreign_free: 0,
            visited_sum: 0,
            visited_peak: 0,
        }; DOORS],
        roots: [0; 5],
        inserts: 0,
        probes: 0,
        revisits: 0,
        grows: 0,
        expands: 0,
        hist_calls: [0; HIST_BUCKETS],
        hist_descents: [0; HIST_BUCKETS],
        tier1_foreign: 0,
        tier2_calls: 0,
        tier2_foreign: 0,
        tier2_scanned: 0,
    };

    /// The `n`th [`RootFamily`]'s slot in [`Counts::roots`].
    fn root_slot(family: RootFamily) -> usize {
        match family {
            RootFamily::Registers => 0,
            RootFamily::Mailbox => 1,
            RootFamily::Dict => 2,
            RootFamily::SavedArgs => 3,
            RootFamily::Signals => 4,
        }
    }
}

/// One thread's census plus the printing duty.
struct Trace {
    /// The tallies; a `RefCell` so an event mutates in place instead of copying a whole
    /// [`Counts`] through a `Cell` on every event.
    counts: RefCell<Counts>,
    /// Whether this thread prints the block when it exits (only the runtime driver does).
    print: bool,
}

impl Trace {
    /// This thread's census, created on first use.
    fn new() -> Trace {
        Trace {
            counts: RefCell::new(Counts::ZERO),
            print: std::thread::current().name() == Some(DRIVER_THREAD),
        }
    }

    /// Folds `f` over this thread's tallies.
    fn with<R>(f: impl FnOnce(&mut Counts) -> R) -> R {
        COUNTERS.with(|trace| f(&mut trace.counts.borrow_mut()))
    }
}

impl Drop for Trace {
    /// Prints the aggregate block on the driver thread. It reads only its own tallies, so it is
    /// safe to run as a thread-local destructor (it never re-enters the thread-local).
    fn drop(&mut self) {
        if !self.print {
            return;
        }
        let counts = *self.counts.borrow();
        let mut out = String::new();
        let mut total = DoorCounts::default();
        for door in ValveDoor::ALL {
            let door_counts = counts.doors[door.index()];
            total.calls += door_counts.calls;
            total.descents += door_counts.descents;
            total.phase1_only += door_counts.phase1_only;
            total.declined_phase1 += door_counts.declined_phase1;
            total.declined_descent += door_counts.declined_descent;
            total.foreign_free += door_counts.foreign_free;
            total.visited_sum += door_counts.visited_sum;
            total.visited_peak = total.visited_peak.max(door_counts.visited_peak);
        }
        let _ = writeln!(
            out,
            "[rt-valve] census: {} valve call(s), {} descent(s), {} phase-1-only, \
             {} declined ({} phase-1 / {} descent), {} foreign_free",
            total.calls,
            total.descents,
            total.phase1_only,
            total.declined(),
            total.declined_phase1,
            total.declined_descent,
            total.foreign_free
        );
        let _ = writeln!(
            out,
            "[rt-valve]   {:<20} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>10}",
            "door", "calls", "descents", "phase1", "declined", "ffree", "visited_sum", "vpeak"
        );
        for door in ValveDoor::ALL {
            let door_counts = counts.doors[door.index()];
            if door_counts.calls == 0 {
                continue;
            }
            let _ = writeln!(
                out,
                "[rt-valve]   {:<20} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12} {:>10}",
                door.label(),
                door_counts.calls,
                door_counts.descents,
                door_counts.phase1_only,
                door_counts.declined(),
                door_counts.foreign_free,
                door_counts.visited_sum,
                door_counts.visited_peak
            );
        }
        let _ = writeln!(
            out,
            "[rt-valve] visited: inserts={} revisits={} probes={} grows={} expands={} \
             mean_probes_per_insert={:.3} size_sum={} mean_visited_per_descent={:.1} peak={}",
            counts.inserts,
            counts.revisits,
            counts.probes,
            counts.grows,
            counts.expands,
            ratio(counts.probes, counts.inserts),
            total.visited_sum,
            ratio(total.visited_sum, total.descents),
            total.visited_peak
        );
        let _ = writeln!(out, "[rt-valve] walk size histogram (all calls):{}", hist(&counts.hist_calls));
        let _ = writeln!(
            out,
            "[rt-valve] walk size histogram (descents):{}",
            hist(&counts.hist_descents)
        );
        let _ = writeln!(
            out,
            "[rt-valve] roots seeded: registers={} mailbox={} dict={} saved_args={} signals={} total={}",
            counts.roots[0],
            counts.roots[1],
            counts.roots[2],
            counts.roots[3],
            counts.roots[4],
            counts.roots.iter().sum::<u64>()
        );
        let _ = writeln!(
            out,
            "[rt-valve] scanned test: tier1_foreign={} tier2_calls={} tier2_foreign={} \
             tier2_scanned={} tier2_share={:.3}",
            counts.tier1_foreign,
            counts.tier2_calls,
            counts.tier2_foreign,
            counts.tier2_scanned,
            ratio(
                counts.tier2_calls,
                counts.tier1_foreign + counts.tier2_calls
            )
        );
        eprint!("{out}");
    }
}

/// One histogram row: ` label=count` for every bucket, in order.
fn hist(buckets: &[u64; HIST_BUCKETS]) -> String {
    let mut row = String::new();
    for (index, label) in Labels::HIST.iter().enumerate() {
        let _ = write!(row, " {}={}", label, buckets[index]);
    }
    row
}

/// `numerator / denominator` as a float, `0.0` for an empty denominator (a `NaN` in the report
/// would be worse than a zero).
fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

thread_local! {
    /// This thread's census; prints at thread exit when the thread is the runtime driver.
    static COUNTERS: Trace = Trace::new();
}

/// Records one valve call and its answer.
///
/// `descend` is the caller's gate, `phase1_hit` says the always-run per-word phase already found a
/// decline, `descent_hit` says the descent did, and `visited` is the walk's visited-set size at the
/// end. `foreign_free` is the verdict's own flag, reported by the caller exactly as the valve
/// reported it (never re-derived here).
pub(crate) fn note_verdict(
    door: ValveDoor,
    descend: bool,
    phase1_hit: bool,
    descent_hit: bool,
    foreign_free: bool,
    visited: usize,
) {
    if !enabled() {
        return;
    }
    record_verdict(
        door,
        descend,
        phase1_hit,
        descent_hit,
        foreign_free,
        visited,
    );
}

/// The arithmetic of [`note_verdict`], without the switch (the unit suite's entry point).
#[inline]
pub(crate) fn record_verdict(
    door: ValveDoor,
    descend: bool,
    phase1_hit: bool,
    descent_hit: bool,
    foreign_free: bool,
    visited: usize,
) {
    Trace::with(|counts| {
        let tally = &mut counts.doors[door.index()];
        tally.calls += 1;
        let descended = descend && !phase1_hit;
        if descended {
            tally.descents += 1;
        }
        if !descend {
            tally.phase1_only += 1;
        }
        if phase1_hit {
            tally.declined_phase1 += 1;
        }
        if descent_hit {
            tally.declined_descent += 1;
        }
        tally.foreign_free += u64::from(foreign_free);
        tally.visited_sum += visited as u64;
        tally.visited_peak = tally.visited_peak.max(visited as u64);
        let bucket = hist_bucket(visited);
        counts.hist_calls[bucket] += 1;
        if descended {
            counts.hist_descents[bucket] += 1;
        }
    });
}

/// The arithmetic of [`note_roots`], without the switch.
#[inline]
pub(crate) fn record_roots(family: RootFamily, words: usize) {
    Trace::with(|counts| counts.roots[Counts::root_slot(family)] += words as u64);
}

/// The arithmetic of [`note_insert`], without the switch.
#[inline]
pub(crate) fn record_insert(steps: u64, inserted: bool) {
    Trace::with(|counts| {
        counts.inserts += 1;
        counts.probes += steps;
        if !inserted {
            // A duplicate is a re-visit: the walk's own sharing. Counted so that
            // `inserts - distinct` is readable from the report.
            counts.revisits += 1;
        }
    });
}

/// The arithmetic of [`note_grow`], without the switch.
#[inline]
pub(crate) fn record_grow() {
    Trace::with(|counts| counts.grows += 1);
}

/// The arithmetic of [`note_expand`], without the switch.
#[inline]
pub(crate) fn record_expand() {
    Trace::with(|counts| counts.expands += 1);
}

/// Records one word the aggregate fast reject classified as **outside** every scanned region.
///
/// Ungated on purpose: the scanned test's callers hoist the flag once per walk (`Walk::traced`,
/// passed down into `gc::word_region`/`outside_bounds`), because this test runs once per
/// classified *word* — millions of times per run — and an environment probe here was measured at
/// +4.3 % of the whole run.
#[inline]
pub(crate) fn record_tier1_foreign() {
    Trace::with(|counts| counts.tier1_foreign += 1);
}

/// Records one word the aggregate could not decide, and what the exact per-region tests answered.
///
/// Ungated for the reason [`record_tier1_foreign`] states.
#[inline]
pub(crate) fn record_tier2(outside: bool) {
    Trace::with(|counts| {
        counts.tier2_calls += 1;
        if outside {
            counts.tier2_foreign += 1;
        } else {
            counts.tier2_scanned += 1;
        }
    });
}

/// This thread's census, for the module's own tests.
#[cfg(test)]
fn snapshot() -> Counts {
    COUNTERS.with(|trace| *trace.counts.borrow())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bucket edges partition every size exactly once, and the empty walk has a bucket of its
    /// own (a walk that declined in phase 1 never touches the set).
    #[test]
    fn the_histogram_buckets_partition_every_size() {
        assert_eq!(hist_bucket(0), 0);
        assert_eq!(hist_bucket(1), 1);
        assert_eq!(hist_bucket(16), 1);
        assert_eq!(hist_bucket(17), 2);
        assert_eq!(hist_bucket(64), 2);
        assert_eq!(hist_bucket(65), 3);
        assert_eq!(hist_bucket(256), 3);
        assert_eq!(hist_bucket(257), 4);
        assert_eq!(hist_bucket(1024), 4);
        assert_eq!(hist_bucket(1025), 5);
        assert_eq!(hist_bucket(4096), 5);
        assert_eq!(hist_bucket(4097), 6);
        assert_eq!(hist_bucket(16384), 6);
        assert_eq!(hist_bucket(16385), 7);
        assert_eq!(hist_bucket(usize::MAX), 7);
    }

    /// Every door's slot is distinct and inside the table — the report's row order is
    /// [`ValveDoor::ALL`]'s.
    #[test]
    fn every_door_has_its_own_slot() {
        let mut seen = [false; DOORS];
        for door in ValveDoor::ALL {
            let index = door.index();
            assert!(index < DOORS);
            assert!(!seen[index], "{door:?} shares a slot");
            seen[index] = true;
        }
        assert!(seen.iter().all(|&used| used));
    }

    /// Every root family has its own slot inside the array.
    #[test]
    fn every_root_family_has_its_own_slot() {
        let families = [
            RootFamily::Registers,
            RootFamily::Mailbox,
            RootFamily::Dict,
            RootFamily::SavedArgs,
            RootFamily::Signals,
        ];
        let mut seen = [false; 5];
        for family in families {
            let slot = Counts::root_slot(family);
            assert!(!seen[slot], "{family:?} shares a slot");
            seen[slot] = true;
        }
        assert!(seen.iter().all(|&used| used));
    }

    /// The counts are a pure sum: a verdict folds into exactly one door, `foreign_free` is never
    /// inferred, a phase-1 decline never counts as a descent, and the two histograms separate.
    #[test]
    fn a_verdict_folds_into_its_own_door_only() {
        let before = snapshot();
        record_verdict(ValveDoor::EnsureHeapFull, true, true, false, false, 0);
        record_verdict(ValveDoor::EnsureHeapFull, true, false, false, true, 40);
        record_verdict(ValveDoor::AllocWordsRooted, false, false, false, false, 7);
        let after = snapshot();
        let full = after.doors[ValveDoor::EnsureHeapFull.index()];
        let baseline = before.doors[ValveDoor::EnsureHeapFull.index()];
        assert_eq!(full.calls, baseline.calls + 2);
        assert_eq!(full.descents, baseline.descents + 1);
        assert_eq!(full.declined_phase1, baseline.declined_phase1 + 1);
        assert_eq!(full.foreign_free, baseline.foreign_free + 1);
        assert_eq!(full.visited_sum, baseline.visited_sum + 40);
        assert_eq!(full.visited_peak.max(baseline.visited_peak), 40);
        let rooted = after.doors[ValveDoor::AllocWordsRooted.index()];
        let rooted_baseline = before.doors[ValveDoor::AllocWordsRooted.index()];
        assert_eq!(rooted.phase1_only, rooted_baseline.phase1_only + 1);
        assert_eq!(rooted.descents, rooted_baseline.descents);
        assert_eq!(rooted.declined(), rooted_baseline.declined());
        // Two calls landed in the "all calls" histogram, one of them in the descents' one.
        assert_eq!(
            after.hist_calls.iter().sum::<u64>(),
            before.hist_calls.iter().sum::<u64>() + 3
        );
        assert_eq!(
            after.hist_descents.iter().sum::<u64>(),
            before.hist_descents.iter().sum::<u64>() + 1
        );
        assert_eq!(after.hist_calls[hist_bucket(40)], before.hist_calls[hist_bucket(40)] + 1);
        assert_eq!(
            after.hist_descents[hist_bucket(40)],
            before.hist_descents[hist_bucket(40)] + 1
        );
    }

    /// The root tallies are per family and additive, and a duplicate insert is counted as a
    /// re-visit while still counting its probes.
    #[test]
    fn roots_and_inserts_are_additive_per_family() {
        let before = snapshot();
        record_roots(RootFamily::Registers, 11);
        record_roots(RootFamily::Signals, 3);
        record_insert(1, true);
        record_insert(3, false);
        record_grow();
        record_expand();
        let after = snapshot();
        assert_eq!(after.roots[0], before.roots[0] + 11);
        assert_eq!(after.roots[4], before.roots[4] + 3);
        assert_eq!(after.roots[1], before.roots[1]);
        assert_eq!(after.inserts, before.inserts + 2);
        assert_eq!(after.probes, before.probes + 4);
        assert_eq!(after.revisits, before.revisits + 1);
        assert_eq!(after.grows, before.grows + 1);
        assert_eq!(after.expands, before.expands + 1);
    }

    /// The two scanned-test tiers are complementary: a word lands in exactly one of them, so
    /// `tier1 + tier2` is the number of words the classifier looked at.
    #[test]
    fn the_scanned_test_tiers_partition_every_word() {
        let before = snapshot();
        record_tier1_foreign();
        record_tier2(false);
        record_tier2(true);
        let after = snapshot();
        assert_eq!(after.tier1_foreign, before.tier1_foreign + 1);
        assert_eq!(after.tier2_calls, before.tier2_calls + 2);
        assert_eq!(after.tier2_scanned, before.tier2_scanned + 1);
        assert_eq!(after.tier2_foreign, before.tier2_foreign + 1);
    }

    /// The ratio helper never divides by zero.
    #[test]
    fn a_ratio_with_no_denominator_is_zero() {
        assert_eq!(ratio(7, 0), 0.0);
        assert!((ratio(1, 4) - 0.25).abs() < f64::EPSILON);
    }
}
