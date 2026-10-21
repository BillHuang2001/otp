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

//! **MEASURED — the runtime-shaped GC workload under `Copying` and under the mark-region
//! (`Immix`) mature generation.**
//!
//! This is the acceptance measurement for the batch that made the runtime's mature-generation
//! strategy reachable from its own configuration (`HeapOptions::with_initial_mature_words` →
//! the automatic rule of [`beam_gc::select_mature`]; the default-OFF `BEAM_RT_GC_STRATEGY=immix`
//! host opt-in applied once in `Runtime::new`). The *strategy selection* is proved by
//! `tests/gc_strategy.rs`; this file measures what the two strategies actually **do** to a
//! process the runtime spawned itself.
//!
//! # The workload (identical for both strategies)
//!
//! The committed fixture `tests/fixtures/gc_payload.beam`:
//!
//! * a **long-lived payload** of 64 tuples `{K, K + 1, [K, K + 2, K + 3]}` (~768 words), built
//!   once and kept reachable (an X/Y register, i.e. inside the Erlang stack the collector
//!   relocates) across **every** round — so it is promoted to the mature generation by the
//!   first collection and is the live set the mature strategy works on;
//! * **short-lived churn**: each round allocates a fresh 64-element list of small tuples
//!   (~576 words, far above the 233-word default nursery), which dies before the next
//!   collection — the allocation pressure that makes the runtime collect at all.
//!
//! `ROUNDS = 2000` rounds, `fullsweep_after = 2` (two minors, then a fullsweep — the cadence of
//! `../beam_gc/MEASURED.md`'s allocation-heavy workload, and enough for hundreds of fullsweeps).
//! Both runs go through the crate's **own** path: `Runtime::new(RuntimeConfig)` → `boot` →
//! `load_module` → `spawn_process` → a `run_one_slice` loop. No hand-built collector and no
//! hand-built process appears anywhere.
//!
//! # The two configurations
//!
//! The runtime cannot pin a mature strategy (`ProcessTable::spawn` takes only a `HeapPolicy`),
//! so the strategy is a consequence of the configured mature generation and the automatic rule
//! selects it at `>= IMMIX_BREAK_EVEN_WORDS` (2048 words). The two runs therefore differ by
//! **one word** of configured mature generation:
//!
//! | run | `initial_mature_words` | rule's verdict |
//! |---|---|---|
//! | `Copying` | `IMMIX_BREAK_EVEN_WORDS - 1` = 2047 | `MatureKind::Copying` |
//! | `Immix` | `IMMIX_BREAK_EVEN_WORDS` = 2048 | `MatureKind::Immix` |
//!
//! both at `fullsweep_after = 2`. Pinning the sizes one word apart is the closest the runtime's
//! own configuration can come to a controlled, single-variable comparison (and it exercises the
//! boundary of the rule); `../beam_gc/MEASURED.md` used one identical pinned policy for both
//! strategies, which the process path here cannot express.
//!
//! # What is asserted (counts only — never time)
//!
//! Both runs: the payload came back **intact** element by element (the witness that the
//! collections the runtime's own doors drove did not relocate the heap out from under the live
//! terms); `gc_stats().collections > 0`, `gc_stats().major > 0` and the process's own collector
//! performed fullsweeps; and every failure counter (`errors`/`halted`/`valve_trips`/`disabled`)
//! is clean. Each strategy is run **twice on the same runtime** and the two runs must report
//! **identical** load-invariant counts — the load-invariance assertion (the host is heavily
//! loaded, so wall-clock is meaningless and is printed, never asserted). Across the two
//! strategies: the two runs must **account for the same young words** — the product
//! `collections × mean nursery capacity`, which is what the run's own heap doors admitted (see
//! [the cadence is an accounting identity](#the-cadence-is-an-accounting-identity-not-a-tolerance)),
//! the mark-region run must
//! reach strictly more fullsweeps (its would-not-fit escalation), and its mature occupancy and
//! reported process memory must be strictly larger (the line-granular price). The **copy-work
//! direction is deliberately not asserted** — its sign is regime-dependent *and* the two runs'
//! per-collection volume is not comparable, because a collection copies the live set through a
//! nursery each strategy sizes from its own collection mix (see
//! [the measured verdict](#the-measured-verdict)); it is printed instead.
//!
//! # The cadence is an accounting identity, not a tolerance
//!
//! The two collection **counts** are deliberately not compared to each other. A percentage band
//! on them is a number fitted to the data, and it moves whenever a `beam_gc` sizing change shifts
//! one run by a single collection — which is exactly what happened when the minor/major decision
//! stopped using the dead-inclusive nursery fill: the copying run moved 587 → 588 collections and
//! a 5 % band, fitted with no headroom, tipped over. What the mechanism actually fixes is a
//! **product**. Every collection is triggered by a heap-admission door refusing the next request
//! because the *current* young space has no room left (`src/context/gc.rs::ensure_heap_full`'s
//! `should_collect(heap_need)` → `beam_gc`'s `collect`), so the words one collection accounts for
//! are its capacity minus the words the failing request could not fit. Summed over a run:
//!
//! ```text
//! allocated young words  =  Σ_collections (capacity − free_at_admission)
//!                        =  collections × mean_capacity − Σ free_at_admission
//! ```
//!
//! The left-hand side is **strategy-independent**: the fixture allocates exactly the same terms in
//! the same order under either mature strategy (nothing it does consults the collector), and the
//! mature generation's own capacity is not part of the young allocation at all. The right-hand side
//! is what each run's samples measure: the mean capacity is taken over the per-slice samples, and a
//! slice is one fixed reduction budget, so in this allocation-dominated workload every sample
//! stands for the same number of allocated words — the weight a `Σ over collections` needs. (The
//! *current* space is sampled, never `heap_size_words()`'s chain sum: a growth retires a space no
//! collection drains.)
//!
//! Two derived residuals are the only terms that can make the two products differ, and both are
//! bounded rather than fitted:
//!
//! * `Σ free_at_admission` — each collection's interval is short by the request that did not fit.
//!   The fixture's rounds allocate one `churn/1` step at a time (a 3-tuple, a 3-element list and a
//!   cons = 12 words), so the residual is `< collections × 12` ≈ **0.4 %** of the ~1.85 M words a
//!   run accounts for;
//! * the per-slice sampling of the mean — one sample per `collections / samples` ≈ 6-collection
//!   slice, and the capacity moves in grown steps of at most `peak − settled` = 1 599 words, so a
//!   mis-attributed sample moves the mean by `1 599 / 97` ≈ 16 words, i.e. `collections × 16` on
//!   the product ≈ **0.5 %**.
//!
//! The asserted tolerance is **1 %** — the sum of those two terms, five times *tighter* than the
//! 5 % band on the raw counts it replaces, on a quantity that is not fitted to the data. What
//! still fails: a run whose doors admitted the same words under a smaller nursery (collecting
//! without the allocation to justify it), a run that stopped collecting, and either residual
//! growing (a larger per-request overshoot, or a coarser sampling of a wider capacity span).
//!
//! # The measured verdict
//!
//! Measured on this host with the shipped configuration (`ROUNDS = 2000`,
//! `fullsweep_after = 2`); the test prints this table under `-- --nocapture`. Every figure is a
//! **count**, identical on every run of the binary (checked across OS processes as well as
//! within one), so all of it is load-invariant — the host being heavily loaded does not move a
//! single one of these numbers.
//!
//! | metric | Copying (2047) | Immix (2048) | Immix ÷ Copying |
//! |---|---|---|---|
//! | runtime `gc_stats().collections` | 588 | 558 | 0.949× |
//! | runtime `gc_stats().major` | 196 | 291 | 1.485× |
//! | process `GcStats::words_copied` | 376 610 | 201 540 | 0.535× |
//! | process `GcStats::objects_copied` | 153 150 | 80 681 | 0.527× |
//! | process `GcStats::words_reclaimed` | 1 270 560 | 2 353 776 | 1.853× (unit caveat below) |
//! | process `GcStats::minor_collections` | 391 | 266 | 0.680× |
//! | process `GcStats::major_collections` | 195 | 289 | 1.482× |
//! | `collector().mature_used_words()` | 1 748 | 7 808 | **4.467×** |
//! | `collector().minor_gcs()` (last sample) | 1 | 0 | — (see below) |
//! | mean sampled nursery capacity | 3 148 | 3 303 | 1.049× |
//! | settled (last sampled) nursery capacity | 2 586 | 2 586 | 1.000× |
//! | peak nursery capacity | 4 185 | 4 185 | 1.000× |
//! | `collections × mean nursery` (young words admitted) | 1 851 024 | 1 843 074 | **0.996×** |
//! | `erlang:memory(processes)` | 175 864 B | 176 288 B | 1.002× |
//! | `erlang:memory(processes_used)` | 34 944 B | 92 624 B | 2.651× |
//! | wall clock (printed, never asserted) | 30–48 ms | 38–43 ms | not reproducible |
//!
//! The nursery rows are the identity's own inputs. Both runs install the **same two capacities**
//! (2 586 and 4 185 — consecutive steps of the curated growth table) because they share the sizing
//! rules; they differ in how much time they spend in each, and that is what makes one run collect
//! 30 times fewer while admitting the same allocation: 3 303 vs 3 148 words per collection. The
//! products land **0.43 %** apart, inside the 1 % bound derived above.
//!
//! `minor_gcs` is a **last-sample** figure and is part of the determinism comparison only: it is
//! `minor_since_major`, which a fullsweep resets, so where the last observed slice falls in the
//! `fullsweep_after = 2` cadence decides it (Copying's last sample sits one minor past a
//! fullsweep, Immix's zero). The cumulative `minor_collections`/`major_collections` rows above are
//! the informative figures.
//!
//! **The footprint price is the strict, load-invariant result.** The mark-region run pays
//! **4.47× the mature occupancy** and **2.65× the reported used process bytes** for the identical
//! workload, and reaches a fullsweep far more often (291 of 558 collections against 196 of 588): a
//! mark-region minor whose promotion would not fit the mature space's free lines escalates to a
//! major (`immix_collector.rs::collect`'s `tight` arm), so the same cadence is split into more
//! fullsweeps and fewer minors (289/266 against 195/391). `erlang:memory(processes)` as a whole is
//! only **1.002×** larger (176 288 B against 175 864 B), which the assertion pins as strictly
//! larger, and the line-granular cost lands in the **used** half of it (2.65×).
//!
//! **The copy volume is a printed measurement, not a direction.** Each collection copies the live
//! set and the mutator then fills a nursery, so the volume depends on how many collections of each
//! kind a run performs and on the nursery the responsible rule installed. Both strategies feed the
//! *same* quantity into `HeapPolicy::young_after_collection` — the minor's exact promoted words,
//! `GcStats::words_copied`, counted in object words by both `collector.rs::collect_minor` and
//! `immix_collector.rs::run_minor` — and they diverge only through the minor/major mix: a minor
//! targets the young live set once over (`young_minor_target_words`) while a fullsweep targets the
//! whole-heap live set twice over (`young_target_words`), and a mark-region fullsweep reclaims the
//! mature generation in place and promotes only the nursery, where a copying fullsweep relocates
//! the whole heap. On this workload the mark-region run copies 0.535× the words. The copy axis is
//! therefore reported and never asserted — it is a function of the nursery each run's mix installs,
//! which is why the two runs are compared through the allocation product above rather than through
//! their raw collection counts.
//!
//! `MEASURED.md`'s headline — on a long, allocation-driven run of 8 000 rounds with a
//! **2 560-word long-lived set of line-sized (16-word) objects** in a **pinned 128-block space**
//! the mark-region strategy moved **12 290 560 words against Copying's 19 115 520** = **0.643×**
//! (−35.7 %; objects 4 096 160 vs 4 522 720 = 0.906×), for a **3.79×** peak mature occupancy and
//! a **2.35×** peak resident, with pause medians **54–57 µs (Copying)** against **95–126 µs
//! (Immix)** — is **not** the same measurement: there the win came from *pinning a large,
//! line-sized live set instead of re-copying it at each fullsweep*, and it was deliberately given
//! room for its line-rounded occupancy (32 768 words = 128 blocks, a `mature_after_fullsweep`
//! fixed point so a fullsweep reclaims in place and nothing relocates). Here the live set is
//! ~768 words — **3 whole `BLOCK_WORDS`-sized blocks**, and of **4- and 6-word** objects, so a
//! 16-word line holds one small object at 2.7–4× its payload — inside a space that merely
//! *starts* at the rule's break-even (2 048 words = 8 blocks) and is then grown by the policy,
//! never pinned at a fixed point; the line-granular occupancy therefore keeps the free-line
//! pressure high (4.47× the copying space's occupancy) and the would-not-fit escalation keeps
//! firing (291 fullsweeps against 196). This is `MEASURED.md`'s own description of the loss
//! regime — the small-object shape ("below one whole block Immix is never fewer and is strictly
//! more for the small-object shape"; line rounding charges a 3-word tuple a whole 16-word line =
//! 5.3×) and the high-churn shape. The saving `MEASURED.md` measures is therefore a function of
//! the *shape* — a large, line-sized, long-lived set with room to pin it — not of the strategy;
//! on the runtime's own default-scale live set the mark-region space pays the **footprint** axis
//! and the escalation cadence, while the copy axis follows the two nurseries' relative sizes and
//! is left to the printed measurement.
//!
//! **So the measurement confirms rather than overturns the standing decision:** Copying stays the
//! right default (`tests/gc_strategy.rs` pins that the default policy still resolves to
//! `MatureKind::Copying`), the break-even rule's switch point is not lowered, and the mark-region
//! mature generation remains the large-live-set option it is selected for. `MEASURED.md`'s
//! copy-work-versus-footprint trade and its pause signal (a single-threaded in-process median,
//! printed and never asserted) are unchanged by this file.
//!
//! Two honest caveats on the figures. (a) `words_reclaimed` is **not unit-comparable**: the
//! copying figure counts exact object words, the mark-region figure whole 16-word lines
//! (`MEASURED.md`'s own caveat) — the 1.85× is mostly that unit, not 1.85× more garbage.
//! (b) The process's own `GcStats` is a **floor**: the scheduler removes a terminated process at
//! once, so the terminal slice's collections are not observable from the table. The runtime's own
//! `gc_stats()` delta is the authoritative schedule and the shortfall is **2 of 588** collections
//! for Copying (586 observable process collections) and **3 of 558** for Immix (555), i.e. under
//! one percent — each run loses only the collections of the slice in which its process
//! terminated, so the comparison is like for like.

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::time::{Duration, Instant};

use beam_engine::ExecutionContext;
use beam_gc::{GcStats, MatureKind, IMMIX_BREAK_EVEN_WORDS};
use beam_rt::config::{strategy_env_default, GcStrategy, HeapOptions};
use beam_rt::{Runtime, RuntimeConfig, RuntimeContext};
use beam_term::{make_small, signed_value};

/// The runtime-shaped workload: `run(N)` builds a 64-element long-lived payload, churns N
/// rounds of short-lived garbage while keeping it live, and returns the payload.
const GC_PAYLOAD_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/gc_payload.beam"
);

/// [`GC_PAYLOAD_BEAM`]'s payload element count (`gc_payload:payload_length/0`).
const PAYLOAD_LENGTH: i64 = 64;

/// The churn rounds per measured run.
const ROUNDS: i64 = 2000;

/// The fullsweep cadence of both measured runs: two minors, then a fullsweep.
const FULLSWEEP_AFTER: u32 = 2;

/// How long one workload run may take before the test declares the runtime hung — a generous
/// safety net for a heavily loaded host, never a performance assertion.
const RUN_DEADLINE: Duration = Duration::from_secs(600);

/// A runtime with `config` and the workload fixture loaded, or `None` after a loud skip.
///
/// The runtime is built through the crate's **own** path (`Runtime::new` → `boot` →
/// `load_module`), so what is measured is what the real runtime does.
fn runtime_with(config: RuntimeConfig) -> Option<Runtime> {
    support::require_oracle("gc_immix_payload")?;
    let mut rt = Runtime::new(config).expect("the runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(GC_PAYLOAD_BEAM).expect("the fixture loads");
    Some(rt)
}

/// Everything one measured run reports. Every field is a **load-invariant count** (nothing here
/// is a duration), so two runs of the same strategy must produce the same value — which is what
/// the determinism assertion checks.
///
/// [`Measured::elapsed`] is deliberately **excluded** from `PartialEq` (it is context, and the
/// host is heavily loaded), so `==` compares exactly the load-invariant counts.
#[derive(Debug, Clone, Copy)]
struct Measured {
    /// The mature strategy the process's own collector ran.
    kind: MatureKind,
    /// The process's own cumulative collection counters (last sample while it lived).
    stats: GcStats,
    /// The process's mature-generation occupancy in words at the last sample.
    mature_used_words: usize,
    /// The process's `minor_gcs` (minors since the last fullsweep) at the last sample.
    minor_gcs: u32,
    /// The **allocation-weighted mean** capacity, in words, of the young space the run's own
    /// collections drained: the arithmetic mean of the per-slice `young().capacity_words()`
    /// samples. A slice is one fixed reduction budget, so in this allocation-dominated workload
    /// every sample stands for the same number of allocated words — the weighting the accounting
    /// identity `collections × mean capacity = allocated words` needs (see
    /// [the file docs](#the-cadence-is-an-accounting-identity-not-a-tolerance)).
    ///
    /// Deliberately the *current* space, never `heap_size_words()`'s chain sum: a
    /// [`Process::grow_nursery`] retires a space no collection ever drains, so a chain sum would
    /// count capacity that no collection accounts for. (On this workload the two agree at every
    /// sample — the chain is always collapsed.)
    nursery_words: usize,
    /// The **last** sampled nursery capacity, in words — where the run came to rest.
    settled_nursery_words: usize,
    /// The largest current-space nursery capacity observed at any slice boundary — the figure a
    /// fullsweep's `young_target_words(live)` (twice the whole-heap live set) installs and a
    /// minor's `young_minor_target_words(live)` (once the young live set) does not.
    peak_nursery_words: usize,
    /// How many slice boundaries the mean above was taken over.
    nursery_samples: u64,    /// Collections the **runtime** ran during the run (its `gc_stats()` delta).
    collections: u64,
    /// Fullsweeps among them.
    major: u64,
    /// The run's peak `erlang:memory(processes)`, in bytes, sampled while the process lived.
    peak_processes_bytes: i64,
    /// The run's peak `erlang:memory(processes_used)`, in bytes, likewise.
    peak_processes_used_bytes: i64,
    /// The payload's element count as the workload returned it (the structural check's summary).
    payload_length: usize,
    /// How long the run took — printed for context, never asserted, and not compared.
    elapsed: Duration,
}

impl PartialEq for Measured {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            kind,
            stats,
            mature_used_words,
            minor_gcs,
            nursery_words,
            settled_nursery_words,
            peak_nursery_words,
            nursery_samples,
            collections,
            major,
            peak_processes_bytes,
            peak_processes_used_bytes,
            payload_length,
            // Context only: a duration can never be load-invariant.
            elapsed: _,
        } = *self;
        kind == other.kind
            && stats == other.stats
            && mature_used_words == other.mature_used_words
            && minor_gcs == other.minor_gcs
            && nursery_words == other.nursery_words
            && settled_nursery_words == other.settled_nursery_words
            && peak_nursery_words == other.peak_nursery_words
            && nursery_samples == other.nursery_samples
            && collections == other.collections
            && major == other.major
            && peak_processes_bytes == other.peak_processes_bytes
            && peak_processes_used_bytes == other.peak_processes_used_bytes
            && payload_length == other.payload_length
    }
}

impl Eq for Measured {}

/// `erlang:memory(Key)` through the **real** BIF on the runtime's own context (so the figure is
/// this runtime's accounting, not a re-derived formula), as an integer byte count.
fn memory_key(rt: &mut Runtime, key: beam_term::Eterm) -> i64 {
    let mut ctx = RuntimeContext::new(rt);
    let term = ctx
        .call_bif("erlang", "memory", 1, &[key])
        .expect("erlang:memory/1 answers");
    signed_value(term)
}

/// Runs `gc_payload:run(rounds)` as a real scheduled process (one runner slice at a time,
/// bounded by [`RUN_DEADLINE`]) and reports everything [`Measured`] holds.
///
/// The process's own collector figures are sampled **while the process is in the table** — the
/// scheduler removes a terminated process at once — and its counters are cumulative, so the last
/// sample covers the run **except the final slice**, in which the process terminates and leaves
/// the table before the loop can sample it again. They are therefore a *floor*: the runtime's own
/// `gc_stats()` delta ([`Measured::collections`]/[`Measured::major`]) counts every collection the
/// runtime's doors ran and is the authoritative schedule, and the difference between the two is
/// exactly the collections that terminal slice performed. Both strategies lose the same kind of
/// tail, so the comparison is like for like (measured: 586 observable process collections
/// against 588 runtime ones for Copying, 555 against 558 for Immix — a tail of 2 and 3, under
/// 1 %). `erlang:memory(processes)`/`(processes_used)` are sampled every slice and the
/// peak is reported, so the figures include the workload process itself.
fn run_measured(rt: &mut Runtime, rounds: i64) -> Measured {
    let processes_key = rt.intern("processes");
    let used_key = rt.intern("processes_used");
    let before = rt.gc_stats();
    let pid = rt
        .spawn_process("gc_payload", "run", vec![make_small(rounds)])
        .expect("the workload is spawned");

    let started = Instant::now();
    let mut kind = None;
    let mut stats = GcStats::default();
    let mut mature_used_words = 0usize;
    let mut minor_gcs = 0u32;
    let mut settled_nursery_words = 0usize;
    let mut peak_nursery_words = 0usize;
    let mut nursery_capacity_sum = 0u64;
    let mut nursery_samples = 0u64;
    let mut peak_processes_bytes = i64::MIN;
    let mut peak_processes_used_bytes = i64::MIN;
    let deadline = started + RUN_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "gc_payload:run/1 with N={rounds} did not terminate within {}s",
            RUN_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        if let Some(process) = rt.processes().get(pid) {
            let collector = process.collector();
            kind = Some(collector.kind());
            stats = collector.stats();
            mature_used_words = collector.mature_used_words();
            minor_gcs = collector.minor_gcs();
            settled_nursery_words = collector.young().capacity_words();
            peak_nursery_words = peak_nursery_words.max(settled_nursery_words);
            nursery_capacity_sum += settled_nursery_words as u64;
            nursery_samples += 1;
        }
        let processes = memory_key(rt, processes_key);
        let processes_used = memory_key(rt, used_key);
        peak_processes_bytes = peak_processes_bytes.max(processes);
        peak_processes_used_bytes = peak_processes_used_bytes.max(processes_used);
    }
    let elapsed = started.elapsed();

    // The payload must come back intact — the acceptance evidence that the collections the run
    // drove did not relocate the heap out from under the live terms.
    let payload = rt
        .process_result(pid)
        .expect("the workload returned its payload");
    support::check_gc_payload(payload, PAYLOAD_LENGTH);

    assert!(
        nursery_samples > 0,
        "the process was sampled at least once while it lived"
    );
    let after = rt.gc_stats();
    Measured {
        kind: kind.expect("the process was observed while it lived"),
        stats,
        mature_used_words,
        minor_gcs,
        nursery_words: (nursery_capacity_sum / nursery_samples) as usize,
        settled_nursery_words,
        peak_nursery_words,
        nursery_samples,
        collections: after.collections - before.collections,
        major: after.major - before.major,
        peak_processes_bytes,
        peak_processes_used_bytes,
        payload_length: PAYLOAD_LENGTH as usize,
        elapsed,
    }
}

/// Skips (loudly) the measurement when a host-level `BEAM_RT_GC_STRATEGY=immix` would make the
/// "Copying" arm an Immix run too, so no comparison could be made.
fn host_opted_into_immix() -> bool {
    if strategy_env_default() == GcStrategy::Immix {
        eprintln!();
        eprintln!("=============== beam_rt gc_immix_payload SKIPPED ===============");
        eprintln!(
            "BEAM_RT_GC_STRATEGY=immix is set in this environment, so the copying arm of the \
             comparison would be an Immix run by construction."
        );
        eprintln!("Unset it to run the measurement. NOT a pass.");
        eprintln!("==============================================================");
        eprintln!();
        return true;
    }
    false
}

/// Prints one run's row of the measurement table.
fn print_row(label: &str, m: &Measured) {
    println!(
        "{label:<8} kind={:<8} collections={:<4} major={:<3} words_copied={:<7} \
         words_reclaimed={:<7} objects_copied={:<6} minor_collections={:<4} \
         major_collections={:<3} mature_used_words={:<5} minor_gcs={:<2} \
         memory(processes)={:<6} memory(processes_used)={:<5} mean_nursery={:<5} \
         settled_nursery={:<5} peak_nursery={:<5} samples={:<3} payload_len={}",
        format!("{:?}", m.kind),
        m.collections,
        m.major,
        m.stats.words_copied,
        m.stats.words_reclaimed,
        m.stats.objects_copied,
        m.stats.minor_collections,
        m.stats.major_collections,
        m.mature_used_words,
        m.minor_gcs,
        m.peak_processes_bytes,
        m.peak_processes_used_bytes,
        m.nursery_words,
        m.settled_nursery_words,
        m.peak_nursery_words,
        m.nursery_samples,
        m.payload_length,
    );
    // The accounting identity this file asserts, printed per run so the two rows can be read
    // against each other: `collections × mean nursery capacity` is the young words the run's own
    // heap doors admitted (up to the free words each failing request left behind).
    println!(
        "         → collections × mean_nursery = {} × {} = {} young words",
        m.collections,
        m.nursery_words,
        u128::from(m.collections) * m.nursery_words as u128,
    );
    // Timing is context only: the host is heavily loaded, so wall-clock is not a result.
    eprintln!(
        "[timing, never asserted] {label}: {:?} for {ROUNDS} rounds",
        m.elapsed
    );
}

/// Asserts one run is clean and really collected, whatever strategy it ran.
fn assert_measured(m: &Measured, expected: MatureKind) {
    assert_eq!(m.kind, expected, "the run's strategy is the configured one");
    assert_eq!(m.payload_length, PAYLOAD_LENGTH as usize);
    assert!(m.collections > 0, "the workload must collect; {m:?}");
    assert!(m.major > 0, "the cadence must reach a fullsweep; {m:?}");
    assert!(
        m.stats.major_collections > 0,
        "the process's own collector performed fullsweeps; {m:?}"
    );
    assert!(
        m.peak_processes_bytes > 0 && m.peak_processes_used_bytes > 0,
        "the runtime's memory accounting is live; {m:?}"
    );
    assert!(
        m.nursery_samples > 0 && m.nursery_words > 0 && m.settled_nursery_words > 0,
        "the process's nursery was observable while it lived; {m:?}"
    );
    // The nursery really cycles: a fullsweep installs `young_target_words(live)` — twice the
    // whole-heap live set — while a minor installs `young_minor_target_words(live)` — once the
    // young live set — so a run that reaches a fullsweep (the assertion above) must at some point
    // hold a capacity larger than the one it came to rest on.
    assert!(
        m.peak_nursery_words > m.settled_nursery_words,
        "the fullsweep target must exceed the minor target this run settled on ({m:?})"
    );
}

/// Asserts the runtime's "something went wrong" counters are all clean after a run.
fn assert_no_gc_failures(rt: &Runtime, m: &Measured) {
    let stats = rt.gc_stats();
    assert_eq!(stats.errors, 0, "no collection failed; {stats:?} {m:?}");
    assert_eq!(stats.halted, 0, "no process halted; {stats:?} {m:?}");
    assert_eq!(stats.valve_trips, 0, "no escape-store valve; {stats:?} {m:?}");
    assert!(!stats.disabled, "collection was never disabled; {stats:?} {m:?}");
}

/// **The measured verdict — the same runtime-shaped workload under both mature strategies.**
///
/// See the file doc comment for the workload, the two configurations, the measured figures and
/// the cross-reference to `../beam_gc/MEASURED.md`. Each strategy runs **twice** on the same
/// runtime, and the two runs must agree **exactly** on every load-invariant count — the
/// load-invariance assertion (wall-clock is printed and never asserted; the host is under heavy
/// load).
#[test]
fn the_runtime_shaped_workload_under_both_strategies() {
    if host_opted_into_immix() {
        return;
    }
    // One word below the break-even: the automatic rule keeps the copying mature generation.
    let copying_config = RuntimeConfig::new().with_heap_options(
        HeapOptions::new()
            .with_initial_mature_words(IMMIX_BREAK_EVEN_WORDS - 1)
            .with_fullsweep_after(FULLSWEEP_AFTER),
    );
    // At the break-even: the automatic rule selects the mark-region space.
    let immix_config = RuntimeConfig::new().with_heap_options(
        HeapOptions::new()
            .with_initial_mature_words(IMMIX_BREAK_EVEN_WORDS)
            .with_fullsweep_after(FULLSWEEP_AFTER),
    );

    println!(
        "gc_payload:run/{ROUNDS} — long-lived payload ({PAYLOAD_LENGTH} elements, ~768 words) \
         + 64-element churn per round; fullsweep_after={FULLSWEEP_AFTER}"
    );

    // --- Copying -------------------------------------------------------------------------
    let Some(mut copying_rt) = runtime_with(copying_config) else {
        return;
    };
    let copying = run_measured(&mut copying_rt, ROUNDS);
    print_row("Copying", &copying);
    assert_measured(&copying, MatureKind::Copying);
    assert_no_gc_failures(&copying_rt, &copying);
    let copying_again = run_measured(&mut copying_rt, ROUNDS);
    print_row("Copying2", &copying_again);
    assert_eq!(
        copying, copying_again,
        "the copying run must be load-invariant: the same workload twice is identical"
    );

    // --- Immix (mark-region) -------------------------------------------------------------
    let Some(mut immix_rt) = runtime_with(immix_config) else {
        return;
    };
    let immix = run_measured(&mut immix_rt, ROUNDS);
    print_row("Immix", &immix);
    assert_measured(&immix, MatureKind::Immix);
    assert_no_gc_failures(&immix_rt, &immix);
    let immix_again = run_measured(&mut immix_rt, ROUNDS);
    print_row("Immix2", &immix_again);
    assert_eq!(
        immix, immix_again,
        "the mark-region run must be load-invariant: the same workload twice is identical"
    );

    // --- The comparison ------------------------------------------------------------------
    // Both runs drive the same allocation pattern through the same live-set nursery rule
    // (`HeapPolicy::young_after_collection`/`young_after_fullsweep`) and feed it the same
    // **quantity**: a minor's exact promoted word count, `GcStats::words_copied`
    // (`collector.rs::collect_minor`'s copier and `immix_collector.rs::run_minor`'s atomic
    // promotion both count object words, never lines).
    // Their collection **counts** differ, and the cause is the minor/major *mix*, because the two
    // rules behind the mix are not the same rule: a minor sizes the nursery from the young live
    // set once over (`young_minor_target_words`), a fullsweep from the whole-heap live set twice
    // over (`young_target_words`), so a run that reaches more fullsweeps settles on a larger
    // nursery and collects less often. The mature generation reaches the nursery's sizing only
    // through that escalation decision.
    //
    // So the two counts are *not* compared to each other: a percentage band on them was a number
    // fitted to the data, and it moves whenever a `beam_gc` sizing change shifts one run by a
    // single collection. What the mechanism fixes is the **product** — the words the run's own
    // heap doors admitted:
    //
    //     allocated young words = Σ_collections (capacity − free_at_admission)
    //                           = collections × mean capacity − Σ free_at_admission
    //
    // (`src/context/gc.rs::ensure_heap_full` collects when `should_collect(heap_need)` — the
    // current young space no longer holds the next request — so one collection accounts for one
    // nursery capacity less the words the failing request could not fit; the mean capacity is the
    // per-slice sample mean, and a slice is one fixed reduction budget, so in this
    // allocation-dominated workload every sample stands for the same number of allocated words).
    // The left-hand side is strategy-independent: the fixture allocates exactly the same terms in
    // the same order under either mature strategy. The two residuals that can make the two
    // products differ are both derived, and both are summed into the tolerance below — see
    // [the file docs](#the-cadence-is-an-accounting-identity-not-a-tolerance): the
    // Σ free_at_admission term (`< collections × 12 words`, the fixture's own `churn/1` step
    // size) ≈ 0.4 %, and the per-slice sampling of the mean (`(peak − settled) / samples` ≈ 16
    // words per sample, so `collections × 16`) ≈ 0.5 %.
    let copying_allocation = u128::from(copying.collections) * copying.nursery_words as u128;
    let immix_allocation = u128::from(immix.collections) * immix.nursery_words as u128;
    let allocation_lo = copying_allocation.min(immix_allocation);
    let allocation_hi = copying_allocation.max(immix_allocation);
    assert!(
        allocation_lo * 100 >= allocation_hi * 99,
        "the two strategies must account for the same young words to within 1 %: both run the \
         same allocation pattern through the same live-set nursery rule, so \
         `collections × mean nursery capacity` — the words the run's heap doors admitted, up to \
         the free words each failing request left — has to match, and only the minor/major mix \
         (which sizes the nursery, not the allocation) may differ (Copying {} × {} = {}, Immix \
         {} × {} = {})",
        copying.collections,
        copying.nursery_words,
        copying_allocation,
        immix.collections,
        immix.nursery_words,
        immix_allocation
    );
    // The minor/major **split** is *not* strategy-independent: a mark-region minor whose
    // promotion would not fit the mature space's free lines escalates to a major
    // (`immix_collector.rs::collect`'s `tight` arm), so the mark-region run reaches a fullsweep
    // strictly more often while running strictly fewer minors.
    assert!(
        immix.major > copying.major,
        "the mark-region run escalates to a fullsweep more often (Immix {} vs Copying {})",
        immix.major,
        copying.major
    );
    println!(
        "→ words_copied: Immix {} vs Copying {} ({:.3}×) — the measured direction (never \
         asserted, see the file doc comment); objects_copied: Immix {} vs Copying {} ({:.3}×); \
         words_reclaimed: Immix {} vs Copying {} ({:.3}×, not unit-comparable: line-words)",
        immix.stats.words_copied,
        copying.stats.words_copied,
        immix.stats.words_copied as f64 / copying.stats.words_copied as f64,
        immix.stats.objects_copied,
        copying.stats.objects_copied,
        immix.stats.objects_copied as f64 / copying.stats.objects_copied as f64,
        immix.stats.words_reclaimed,
        copying.stats.words_reclaimed,
        immix.stats.words_reclaimed as f64 / copying.stats.words_reclaimed as f64,
    );
    // What the measurement *does* establish as a strict, load-invariant difference is the
    // price side of the trade: the mark-region space's line-granular geometry on 3..6-word
    // objects costs a strictly larger mature occupancy and a strictly larger reported
    // `erlang:memory(processes)` for the identical workload.
    println!(
        "→ mature_used_words: Immix {} vs Copying {} ({:.3}×); memory(processes): Immix {} vs \
         Copying {} ({:.3}×); memory(processes_used): Immix {} vs Copying {} ({:.3}×)",
        immix.mature_used_words,
        copying.mature_used_words,
        immix.mature_used_words as f64 / copying.mature_used_words as f64,
        immix.peak_processes_bytes,
        copying.peak_processes_bytes,
        immix.peak_processes_bytes as f64 / copying.peak_processes_bytes as f64,
        immix.peak_processes_used_bytes,
        copying.peak_processes_used_bytes,
        immix.peak_processes_used_bytes as f64 / copying.peak_processes_used_bytes as f64,
    );
    assert!(
        immix.mature_used_words > copying.mature_used_words,
        "the mark-region mature generation must be strictly larger on this workload \
         (Immix {} vs Copying {})",
        immix.mature_used_words,
        copying.mature_used_words
    );
    assert!(
        immix.peak_processes_bytes > copying.peak_processes_bytes,
        "and the runtime's reported process memory with it (Immix {} vs Copying {})",
        immix.peak_processes_bytes,
        copying.peak_processes_bytes
    );
}
