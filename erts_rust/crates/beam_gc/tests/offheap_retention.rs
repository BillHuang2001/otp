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

//! **Off-heap retention is flat in the number of calls** — the reclamation contract of an
//! off-heap-heavy process, pinned end to end.
//!
//! The workload shape is the real one: a `bs_init_writable` accumulator (`binary_append`-ish
//! `<< <<C:8,C:8,C:8,C:8>> || <<C:8>> <= Bin >>`), i.e. **one writable off-heap payload per
//! call**, filled and published as a 64 KiB binary. Its payload is 64 KiB of off-heap bytes
//! charged to the process's off-heap overhead (`MSO(p).overhead`), plus the frozen 8-word
//! `[BinRef][ErlSubBits]` handle pair on the process heap. The fresh pair becomes the process's
//! only live reference on the next iteration, so every payload but the current one is garbage —
//! and garbage off-heap bytes are reclaimed **only** by a collection whose off-heap sweep drops
//! the roster entries no reachable `BinRef` names (`erts/emulator/beam/erl_gc.c:2996`
//! `sweep_off_heap`; the Rust half is [`beam_gc::sweep_unreachable_binaries`]).
//!
//! Collecting that garbage needs a *trigger*: growing the **heap** never signals that off-heap
//! bytes are piling up, which is exactly why ERTS keeps a second budget for them and collects
//! when the process's off-heap overhead passes it (`erts/emulator/beam/erl_gc.h:132-135`
//! `ERTS_IS_GC_DESIRED_INTERNAL`, the `off_heap.overhead > bin_vheap_sz` disjunct, re-sized
//! after each collection by `next_vheap_size`, `erl_gc.c:2955`). The Rust half is
//! [`beam_gc::BinVheap`].
//!
//! Both halves are this crate's to prove, and they are proven as a **counter** contract, never
//! a wall clock one:
//!
//! * `a_binary_building_loop_stays_flat_in_the_number_of_calls` — with the trigger + sweep the
//!   allocator's `Binary` counters and the roster's live length are the **same small band** at
//!   N = 0/250/500/1000 and *identical* between N = 500 and N = 1000. Flat-in-N is the
//!   assertion; the numbers are printed, not timed.
//! * `without_the_reachability_sweep_the_roster_retains_one_payload_per_call` — the mechanism's
//!   regression witness: omit only the sweep and the identical loop retains ≈64 KiB and one
//!   roster entry **per call** (`>= N * 64 KiB` live bytes, roster length `== N`). This is the
//!   retention the fix removes; if this test ever goes flat, the workload has stopped modelling
//!   the bug and the first test proves nothing.
//! * `the_trigger_collects_and_reclaims_without_per_call_work` — the chain: trigger ⇒ collection
//!   ⇒ reclaim, at a collection count far below N (so the hot path pays no per-call
//!   `O(retained)` work) while retention stays bounded.
//!
//! **Why the counters are read the way they are.** `beam_alloc`'s registry is process-global, so
//! the three tests (which run as threads of one test binary) serialise on [`SERIAL`] and every
//! figure is a **delta** against the value captured when that ladder started — which also makes
//! the swept figures directly comparable between N values. Each ladder drops its collector and
//! roster at the end, so the next ladder starts from the same baseline (a dropped roster frees
//! the payload it still owned: asserted, not assumed).
//!
//! `unsafe` below only writes the 8-word handle pair into a nursery block the test reserved
//! itself; the block carries a `// SAFETY:` rationale. `VmHWM` is printed (Linux only) for the
//! record and never asserted; no clock is read for an assertion anywhere in this file.

use core::mem::size_of;
use std::sync::{Mutex, MutexGuard};

use beam_alloc::RefcBinary;
use beam_gc::prelude::{
    for_each_reachable_binary_payload, sweep_unreachable_binaries, AnyCollector, BinVheap,
    HeapObservable, HeapPolicy, OffHeap,
};
use beam_term::prelude::{make_small, write_refc_binary, Eterm, ERL_REFC_BITS_SIZE};

/// The published binary's size — the 64 KiB accumulator result of the real workload.
const PAYLOAD_BYTES: usize = 65_536;
/// One append step of the accumulator. Sixteen of these fill [`PAYLOAD_BYTES`], mirroring the
/// real workload's 16 384 four-byte `private_append`s without paying for them per call.
const APPEND_CHUNK: usize = 4_096;
/// A term word, in bytes — the unit `bin_vheap_sz` is accounted in (the C `overhead /
/// sizeof(Eterm)`, `erl_gc.c:3051`).
const WORD: usize = size_of::<Eterm>();
/// The policy's heap ceiling. Bounded on purpose: the assertion `capacity_words() <=
/// max_heap_words` is only meaningful on a policy that has a ceiling, and the workload's live
/// set is eight words, so a 64 Ki-word heap is generous.
const MAX_HEAP_WORDS: usize = 1 << 16;
/// The largest roster length (live entries) the *bounded* variants may reach. It is a constant
/// in N: the trigger fires on payload volume, not on call count, so the roster never holds more
/// than one collection interval's worth of payloads.
const ROSTER_BAND: usize = 16;

/// The three tests share one process-global allocator registry, so they measure one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// Takes the serialisation lock, ignoring a previous test's panic (a poisoned lock must not turn
/// a failure into a cascade of spurious ones).
fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// The allocator's structural counters for `binary_alloc`
// ---------------------------------------------------------------------------

/// The bytes `AllocType::Binary` currently has live, summed over the type's own counters.
///
/// This is the allocator's structural counter for off-heap payload bytes — the quantity the
/// retention shows up in — not a clock, an RSS estimate or a guess.
fn binary_live_bytes() -> usize {
    beam_alloc::all_stats()
        .into_iter()
        .filter(|(ty, _)| *ty == beam_alloc::AllocType::Binary)
        .map(|(_, stats)| stats.live_bytes)
        .sum()
}

/// The bytes currently held in the `binary_alloc` carriers (a **high-water** figure: a carrier
/// is unmapped lazily, so this tracks the peak footprint rather than the live one).
fn binary_carrier_bytes() -> usize {
    beam_alloc::all_stats()
        .into_iter()
        .filter(|(ty, _)| *ty == beam_alloc::AllocType::Binary)
        .map(|(_, stats)| stats.carrier_bytes)
        .sum()
}

/// `VmHWM` (the peak resident set) from `/proc/self/status`, in KiB — **printed only, never
/// asserted** (a shared, loaded host makes RSS meaningless as a gate).
#[cfg(target_os = "linux")]
fn vm_hwm_kb() -> Option<usize> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// No `/proc` elsewhere; the record simply has no RSS figure.
#[cfg(not(target_os = "linux"))]
fn vm_hwm_kb() -> Option<usize> {
    None
}

// ---------------------------------------------------------------------------
// The workload ladder
// ---------------------------------------------------------------------------

/// One run of the workload shape and everything observable about it.
struct Report {
    /// The number of calls the ladder made.
    n: usize,
    /// Whether the off-heap sweep ran at the collection doors.
    swept: bool,
    /// `AllocType::Binary` **live** bytes with the roster still holding its survivors, as a
    /// delta against the ladder's baseline.
    settled_live_bytes: usize,
    /// The same delta measured **after** dropping the roster: every payload the roster owned is
    /// gone, so a swept ladder must report `0`.
    dropped_live_bytes: usize,
    /// `binary_alloc` carrier bytes, as a delta against the ladder's baseline.
    settled_carrier_bytes: usize,
    /// The roster's live entry count once the ladder settled (after the final collection and
    /// sweep).
    roster_len_settled: usize,
    /// The largest live entry count the roster ever reached during the ladder.
    peak_roster_len: usize,
    /// How many entries the sweeps released in total.
    released: usize,
    /// How many collections ran (both doors).
    collections: usize,
    /// The allocator charge of exactly one settled payload — the ladder's own unit, so the
    /// assertions are expressed in payloads rather than in magic byte counts.
    one_payload_bytes: usize,
    /// The nursery chain's summed capacity, in words.
    heap_size_words: usize,
    /// The nursery chain's length.
    young_space_count: usize,
    /// Whether the chain carried a retired space after the final collection.
    collection_debt: bool,
    /// `VmHWM` before / after the ladder, in KiB (Linux only; printed, never asserted).
    hwm_kb: (Option<usize>, Option<usize>),
}

impl Report {
    /// One line of the record: the counters, never a timing.
    fn describe(&self) -> String {
        format!(
            "{} N={:<5} binary live={:>10} B carrier={:>10} B roster={} (peak {}) \
             released={} collections={} one_payload={} B heap={} words spaces={} debt={} \
             VmHWM {}->{} KiB",
            if self.swept { "swept  " } else { "unswept" },
            self.n,
            self.settled_live_bytes,
            self.settled_carrier_bytes,
            self.roster_len_settled,
            self.peak_roster_len,
            self.released,
            self.collections,
            self.one_payload_bytes,
            self.heap_size_words,
            self.young_space_count,
            self.collection_debt,
            self.hwm_kb
                .0
                .map_or_else(|| "?".to_string(), |k| k.to_string()),
            self.hwm_kb
                .1
                .map_or_else(|| "?".to_string(), |k| k.to_string()),
        )
    }
}

/// The ladder's heap policy: bounded (so `max_heap_words` is a real ceiling) with the binary
/// virtual heap floored at `min_bin_vheap_words`.
fn bounded_policy(min_bin_vheap_words: usize) -> HeapPolicy {
    HeapPolicy {
        min_heap_words: beam_gc::policy::DEFAULT_MIN_HEAP_WORDS,
        max_heap_words: MAX_HEAP_WORDS,
        min_bin_vheap_words,
        ..HeapPolicy::default()
    }
}

/// Writes the frozen 8-word `[BinRef][ErlSubBits]` pair naming `payload`'s bytes at the
/// collector's nursery top and returns its root (the `ErlSubBits`) — exactly what
/// `bs_init_writable` + the accumulator's final `bin` produces.
fn write_refc(collector: &mut AnyCollector, payload: &RefcBinary) -> Eterm {
    let size_bits = payload.len() * 8;
    // SAFETY: `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved in the nursery;
    // `payload.data_ptr()` is a live, word-aligned pointer to `size_bits` bits of off-heap
    // storage kept alive by the roster (or by this scope), and `null` is the empty off-heap
    // chain link.
    unsafe {
        let hp = collector
            .young_mut()
            .heap_mut()
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the handle pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            size_bits,
        )
    }
}

/// Runs `n` calls of the workload shape and reports the observables.
///
/// Each call, in the order the runtime does it:
///
/// 1. **heap-admission door** — the 8-word pair needs nursery room, so collect when it has none
///    (or when the chain carries a retired space);
/// 2. allocate a fresh 64 KiB writable payload (`bs_init_writable`) and fill it;
/// 3. write the `[BinRef][ErlSubBits]` pair naming it into the nursery and **root** it (the live
///    register a real process holds), which is what makes the *previous* call's payload garbage;
/// 4. register the owning handle in the process's off-heap roster — the analogue of
///    `beam_engine`'s `Machine::refc_binaries`;
/// 5. **binary-virtual-heap door** — when the roster's overhead passes `BinVheap`'s limit,
///    collect, (optionally) sweep the roster, and re-size the limit from what survived.
///
/// `sweep = false` omits only step 5's sweep, which is the ablation that reproduces the
/// retention.
fn run_ladder(n: usize, sweep: bool, min_bin_vheap_words: Option<usize>) -> Report {
    let base_live = binary_live_bytes();
    let base_carrier = binary_carrier_bytes();
    let hwm_before = vm_hwm_kb();

    let policy = bounded_policy(
        min_bin_vheap_words.unwrap_or_else(|| HeapPolicy::default().min_bin_vheap_words),
    );
    let mut collector = AnyCollector::new(policy.clone()).expect("collector construction");
    let mut roster = OffHeap::new();
    let mut vheap = BinVheap::new(&policy);
    let mut roots = vec![make_small(0)];
    let chunk = [0x41u8; APPEND_CHUNK];

    let mut one_payload_bytes = 0usize;
    let mut collections = 0usize;
    let mut released = 0usize;
    let mut peak_roster_len = 0usize;

    for call in 0..n {
        // (1) The heap-admission door.
        if collector.should_collect(ERL_REFC_BITS_SIZE) || collector.collection_debt() {
            collector.collect(&mut roots).expect("heap-door collection");
            collections += 1;
        }

        // (2) A fresh writable accumulator, filled to exactly 64 KiB.
        let mut payload = RefcBinary::with_capacity(PAYLOAD_BYTES).expect("payload");
        for _ in 0..(PAYLOAD_BYTES / APPEND_CHUNK) {
            payload.append(&chunk).expect("append");
        }
        assert_eq!(
            payload.len(),
            PAYLOAD_BYTES,
            "the accumulator is filled to the published size"
        );

        // (3) The handle pair, rooted — the only live reference to this payload from now on.
        roots[0] = write_refc(&mut collector, &payload);

        // (4) The owning handle in the roster.
        roster.push(payload);
        // The roster's high-water mark is sampled *before* the trigger, so it is the retention
        // the process really holds between two collections.
        peak_roster_len = peak_roster_len.max(roster.len());
        // (5) The binary-virtual-heap door.
        if vheap.triggered_for(&roster) {
            collector
                .collect(&mut roots)
                .expect("vheap-door collection");
            collections += 1;
            if sweep {
                released += sweep_unreachable_binaries(&mut roster, &collector, &roots)
                    .expect("the off-heap sweep completes");
            }
            // The C's post-collection bookkeeping (`erl_gc.c:3288-3289`): re-size the budget
            // from the overhead that survived, in words.
            vheap.record_collection(&policy, roster.overhead_bytes() / WORD);
        }

        peak_roster_len = peak_roster_len.max(roster.len());
        if call == 0 {
            // The ladder's own unit: what one settled payload costs `binary_alloc`.
            one_payload_bytes = binary_live_bytes() - base_live;
        }
    }

    // Settle: one final collection (and sweep), so the ladder ends in the steady state a real
    // process would present to `process_info/2`.
    collector.collect(&mut roots).expect("final collection");
    collections += 1;
    if sweep {
        released += sweep_unreachable_binaries(&mut roster, &collector, &roots)
            .expect("the final sweep completes");
    }

    let settled_live_bytes = binary_live_bytes() - base_live;
    let settled_carrier_bytes = binary_carrier_bytes() - base_carrier;
    let roster_len_settled = roster.len();
    let heap_size_words = collector.heap_size_words();
    let young_space_count = collector.young_space_count();
    let collection_debt = collector.collection_debt();

    // Dropping the roster frees every payload it still owned (the refcount reaches zero), which
    // is what returns the allocator's `Binary` counters to the baseline. `roots` is dropped
    // first, so no on-heap handle outlives it.
    drop(roots);
    drop(collector);
    drop(roster);
    let dropped_live_bytes = binary_live_bytes() - base_live;
    let hwm_after = vm_hwm_kb();

    Report {
        n,
        swept: sweep,
        settled_live_bytes,
        dropped_live_bytes,
        settled_carrier_bytes,
        roster_len_settled,
        peak_roster_len,
        released,
        collections,
        one_payload_bytes,
        heap_size_words,
        young_space_count,
        collection_debt,
        hwm_kb: (hwm_before, hwm_after),
    }
}

// ---------------------------------------------------------------------------
// 1. Flat in N
// ---------------------------------------------------------------------------

/// **The fix's contract**: a loop that publishes a 64 KiB off-heap binary per call keeps a
/// *constant* off-heap footprint and a constant roster length, whatever N is.
///
/// Every N builds its own collector, so the four ladders are independent measurements of the
/// same shape. The assertion is **flat-in-N on the allocator's own structural counters**
/// (`AllocType::Binary`'s live bytes, its carrier bytes and the roster's live length), plus the
/// constructor invariants the bounded policy promises. Nothing here is a timing.
#[test]
fn a_binary_building_loop_stays_flat_in_the_number_of_calls() {
    let _guard = serial();

    let reports: Vec<Report> = [0usize, 250, 500, 1_000]
        .into_iter()
        .map(|n| run_ladder(n, true, None))
        .collect();
    for report in &reports {
        println!("{}", report.describe());
    }

    let by_n = |n: usize| -> &Report {
        reports
            .iter()
            .find(|r| r.n == n)
            .unwrap_or_else(|| panic!("no ladder for N={n}"))
    };

    assert_eq!(
        by_n(0).one_payload_bytes,
        0,
        "an empty loop allocates no payload"
    );
    assert_eq!(
        by_n(0).settled_live_bytes,
        0,
        "an empty loop retains nothing"
    );

    for n in [250usize, 500, 1_000] {
        let report = by_n(n);
        let unit = report.one_payload_bytes;
        assert!(
            unit >= PAYLOAD_BYTES,
            "N={n}: one settled payload must cost at least its {} bytes, measured {unit}",
            PAYLOAD_BYTES
        );
        // The load-bearing assertion: a few payloads' worth, never N's. `N * PAYLOAD_BYTES`
        // would be 16 MiB at N=250 — two orders of magnitude away from this band.
        assert!(
            report.settled_live_bytes <= 3 * unit,
            "N={n}: binary live bytes {} must stay within 3 payloads ({} B), not grow with N \
             (which would be {} B)",
            report.settled_live_bytes,
            3 * unit,
            n * PAYLOAD_BYTES
        );
        assert!(
            report.settled_carrier_bytes <= ROSTER_BAND * unit,
            "N={n}: binary carrier bytes {} must stay within {} payloads' worth ({} B) — the \
             carriers cannot have grown with N",
            report.settled_carrier_bytes,
            ROSTER_BAND,
            ROSTER_BAND * unit
        );
        assert!(
            report.roster_len_settled <= 3,
            "N={n}: the roster must settle at the handful of payloads still referenced, got {}",
            report.roster_len_settled
        );
        assert!(
            report.peak_roster_len <= ROSTER_BAND,
            "N={n}: the roster peaked at {} live entries; the trigger bounds it by one \
             collection interval, never by N",
            report.peak_roster_len
        );
        assert_eq!(
            report.dropped_live_bytes, 0,
            "N={n}: dropping the roster must free every payload it owned, leaving {} bytes live",
            report.dropped_live_bytes
        );

        // The constructor/nursery invariants the bounded policy promises.
        assert!(
            report.heap_size_words <= MAX_HEAP_WORDS,
            "N={n}: the nursery chain's summed capacity {} exceeded the policy's ceiling {}",
            report.heap_size_words,
            MAX_HEAP_WORDS
        );
        assert!(
            !report.collection_debt,
            "N={n}: the final collection must collapse the nursery chain"
        );
        assert!(
            report.young_space_count <= 2,
            "N={n}: the chain must be collapsed, got {} spaces",
            report.young_space_count
        );
    }

    // Flat-in-N, stated as equality rather than as a band: the settled figure is one payload's
    // charge in both cases, bit for bit.
    let five_hundred = by_n(500);
    let thousand = by_n(1_000);
    assert_eq!(
        five_hundred.settled_live_bytes, thousand.settled_live_bytes,
        "the settled binary live bytes must be identical at N=500 and N=1000"
    );
    assert_eq!(
        five_hundred.roster_len_settled, thousand.roster_len_settled,
        "the settled roster length must be identical at N=500 and N=1000"
    );
    assert_eq!(
        five_hundred.young_space_count, thousand.young_space_count,
        "and so must the nursery chain's shape"
    );
}

// ---------------------------------------------------------------------------
// 2. The mechanism's regression witness
// ---------------------------------------------------------------------------

/// **The retention the fix removes**, pinned as a number: with the reachability sweep omitted
/// (only the sweep — the trigger still fires and collections still run) the identical loop
/// retains ≈64 KiB and one roster entry per call.
///
/// This is the ablation that makes the first test mean something: it proves the workload really
/// does produce one garbage payload per call, so a flat result is the sweep's doing and not an
/// accident of the shape.
#[test]
fn without_the_reachability_sweep_the_roster_retains_one_payload_per_call() {
    let _guard = serial();

    const N: usize = 250;
    let report = run_ladder(N, false, None);
    println!("{}", report.describe());

    assert!(
        report.collections > 0,
        "the trigger must still fire without the sweep (otherwise the ladder never collects and \
         proves less than it claims)"
    );
    assert_eq!(
        report.released, 0,
        "without the sweep nothing is ever released"
    );
    assert_eq!(
        report.roster_len_settled, N,
        "the roster must hold one entry per call — the append-only owning roster shape"
    );
    assert_eq!(
        report.peak_roster_len, N,
        "and its peak is the call count, not a band"
    );
    // ≈64 KiB per call: the payload's own bytes, plus the allocator's reservation slack.
    assert!(
        report.settled_live_bytes >= N * PAYLOAD_BYTES,
        "{} live bytes must cover {} calls x {} bytes = {} by a wide margin (the reservation \
         alone multiplies it)",
        report.settled_live_bytes,
        N,
        PAYLOAD_BYTES,
        N * PAYLOAD_BYTES
    );
    // Nothing was freed when the roster went away either: the payloads died with it, so *this*
    // figure is the proof that the leak lives in the roster's ownership, not in the heap.
    assert_eq!(
        report.dropped_live_bytes, 0,
        "the retained payloads must be released when their owning roster drops"
    );
}

// ---------------------------------------------------------------------------
// 3. The trigger ⇒ collection ⇒ reclaim chain
// ---------------------------------------------------------------------------

/// **No per-call work on the hot path**: at the default `min_bin_vheap_words` the loop collects
/// far fewer times than it makes calls, yet retention stays in the constant band — the trigger
/// is what bounds the roster, so the mutator pays a constant-time check per call instead of an
/// `O(retained)` walk.
///
/// The three facts asserted are the chain in order: the trigger fires (collections `> 0`), it
/// fires on payload volume rather than on call count (collections well below N, retention
/// bounded), and every payload it fired on is actually reclaimed (`released` covers all but the
/// settled survivor).
#[test]
fn the_trigger_collects_and_reclaims_without_per_call_work() {
    let _guard = serial();

    const N: usize = 1_000;
    let report = run_ladder(N, true, None);
    println!("{}", report.describe());

    // The trigger fired.
    assert!(
        report.collections > 0,
        "the binary virtual heap must want a collection over {N} calls"
    );
    // ... on payload volume, not on call count: a third of N at the very worst, and in practice
    // once per collection interval (~N/6 at the default 46422-word budget).
    assert!(
        report.collections * 3 <= N,
        "{} collections over {N} calls is too close to per-call work",
        report.collections
    );
    // ... and the retention it bounds is constant, not N.
    assert!(
        report.peak_roster_len <= ROSTER_BAND,
        "the roster peaked at {} live entries over {N} calls",
        report.peak_roster_len
    );
    assert_eq!(
        report.roster_len_settled, 1,
        "only the call's own payload is still referenced at the end"
    );
    assert!(
        report.settled_live_bytes <= 3 * report.one_payload_bytes,
        "{} settled bytes must stay within three payloads' worth ({} B)",
        report.settled_live_bytes,
        3 * report.one_payload_bytes
    );
    // Every payload but the survivor was reclaimed — the sweep's work, summed.
    assert!(
        report.released + 2 >= N,
        "the sweeps released {} entries for {N} calls; every garbage payload must be reclaimed",
        report.released
    );
    assert!(
        !report.collection_debt,
        "the final collection must collapse the nursery chain"
    );
}

// ---------------------------------------------------------------------------
// A cross-check that the two halves agree on the same payload set
// ---------------------------------------------------------------------------

/// The two halves are one seam: the payload set
/// [`for_each_reachable_binary_payload`] reports is exactly the set
/// [`sweep_unreachable_binaries`] keeps. Building three payloads, rooting two of them and
/// sweeping must keep exactly those two — the trigger half is never allowed to disagree with the
/// reclaim half about what is live.
#[test]
fn the_walk_and_the_sweep_agree_on_which_payloads_are_live() {
    let _guard = serial();

    let policy = bounded_policy(HeapPolicy::default().min_bin_vheap_words);
    let mut collector = AnyCollector::new(policy).expect("collector");
    let mut roster = OffHeap::new();

    let mut payloads = Vec::new();
    for fill in [1u8, 2, 3] {
        let mut payload = RefcBinary::with_capacity(PAYLOAD_BYTES).expect("payload");
        payload
            .append(&vec![fill; PAYLOAD_BYTES])
            .expect("fill the accumulator");
        payloads.push(payload);
    }
    // Two roots; the third payload stays unrooted (it is the garbage case).
    let mut roots = vec![
        write_refc(&mut collector, &payloads[0]),
        write_refc(&mut collector, &payloads[1]),
    ];
    for payload in &payloads {
        roster.push(payload.clone());
    }

    let mut walked = Vec::new();
    for_each_reachable_binary_payload(&collector, &roots, |payload| walked.push(payload))
        .expect("the walk completes");
    walked.sort_unstable();
    assert_eq!(
        walked,
        vec![
            payloads[0].data_ptr() as usize,
            payloads[1].data_ptr() as usize
        ],
        "the walk reports exactly the payloads a reachable BinRef names"
    );

    let released =
        sweep_unreachable_binaries(&mut roster, &collector, &roots).expect("the sweep completes");
    assert_eq!(released, 1, "only the unrooted payload is released");
    assert_eq!(roster.len(), 2);
    let surviving: Vec<usize> = roster
        .iter()
        .map(|entry| {
            entry
                .as_binary()
                .expect("the roster holds binaries only")
                .data_ptr() as usize
        })
        .collect();
    assert_eq!(
        surviving, walked,
        "the survivors are exactly the walked set, in slot order"
    );

    // After a collection the pair moved but the payload pointers did not; the walk still agrees.
    let before = walked.clone();
    collector.collect(&mut roots).expect("collection");
    let mut after = Vec::new();
    for_each_reachable_binary_payload(&collector, &roots, |payload| after.push(payload))
        .expect("the walk completes");
    after.sort_unstable();
    assert_eq!(
        after, before,
        "a collection relocates the handles and leaves the off-heap pointers byte-identical"
    );
    assert_eq!(
        sweep_unreachable_binaries(&mut roster, &collector, &roots).expect("the sweep"),
        0,
        "and nothing becomes garbage merely by moving"
    );
}
