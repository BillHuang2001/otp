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

//! **MEASURED — the mature-generation strategy (barrier-free copying vs mark-region `Immix`)
//! measured per *workload class* on a process the runtime spawned itself.**
//!
//! `tests/gc_immix_payload.rs` measures the two strategies on the single "runtime-shaped"
//! workload (a long-lived payload plus short-lived churn). That workload is **allocation-bound**,
//! and its lesson is that the strategy's cost is a function of the *shape*: the mark-region space
//! wins when the live set is large and line-sized and loses on a small-object/high-churn live set.
//! This suite asks the next question, which a single shape cannot answer: **how does each
//! strategy's price land on a shape that is not a pure allocation loop at all** — the two classes
//! `ERTS_RUST_REWRITE_TEST_PROJECTS.md` names alongside the compute-bound kernels, namely the
//! **messaging/copy-bound** class (C3) and a **real in-process compile** (C4)?
//!
//! # The two arms (one word apart, exactly as the sibling suite does it)
//!
//! The runtime cannot pin a mature strategy: `ProcessTable::spawn` takes only a
//! [`beam_gc::HeapPolicy`], so the strategy is a **consequence** of the configured mature
//! generation, resolved by [`beam_gc::select_mature`]'s automatic rule at
//! [`IMMIX_BREAK_EVEN_WORDS`] (2 048 words). The two arms therefore differ by **one word** of
//! configured mature generation and by nothing else — same `fullsweep_after`, same heap options,
//! same N, same fixture, same runtime path:
//!
//! | arm | `HeapOptions::with_initial_mature_words` | the rule's verdict |
//! |---|---|---|
//! | `Copying` | `IMMIX_BREAK_EVEN_WORDS - 1` = 2 047 | [`MatureKind::Copying`] |
//! | `Immix` | `IMMIX_BREAK_EVEN_WORDS` = 2 048 | [`MatureKind::Immix`] |
//!
//! Nothing constructs a collector: each arm is installed through
//! `RuntimeConfig::with_heap_options` → `Runtime::new` → `spawn_process`, i.e. the production
//! path. `GenImmixCollector::with_config` is deliberately **not** used anywhere here.
//!
//! **The load-bearing check is that the intended strategy was really in effect.** Every measured
//! run asserts, *in the process it measured*, that `collector().kind()` and
//! `collector().mature_choice()` report the arm's expected [`MatureKind`] with
//! [`MatureSource::Automatic`] at the arm's configured word count, and that the mature space's
//! reported capacity reflects that geometry. A silent fallback — both arms running the copying
//! space, which is exactly what a broken knob or an ambient `BEAM_RT_GC_STRATEGY` produces — is
//! the failure this harness exists to make impossible.
//!
//! # The process/child architecture (why the measurement runs in a child process)
//!
//! **The parent is only a driver.** One `#[test]` per class, plus one for the noise floor, and
//! none of them measures anything: each runs its ROUNDS with the arms **strictly interleaved**
//! (`Copying`, `Immix`, `Copying`, `Immix`, … — never arm-blocked, so a change in host load lands
//! on both arms alike), and every `(class, arm, round)` is measured by **one fresh child
//! process**: this same test binary re-exec'd with
//! `--exact <worker> --ignored --nocapture --test-threads=1`, plus this harness's own environment
//! variables selecting the class/arm/N/round ([`ENV_CLASS`] … [`ENV_OUT`]).
//!
//! Why a child per arm:
//!
//! * `Runtime::gc_stats()` is a per-runtime counter and a process's collector counters are
//!   cumulative, so a second arm in the same process cannot start from zero;
//! * the JIT/engine block cache and the runtime's atom namespace are process-global;
//! * a crash (a `beam_gc` assertion, an OOM) must be one *arm's* failure, not the suite's.
//!
//! The child builds its own [`Runtime`], runs exactly one arm, prints **one machine-readable row**
//! (`ROW class=… arm=… …`) to stdout and exits. While it runs, the parent polls
//! `/proc/<child pid>/status` for `VmHWM:` — the kernel's monotonic high-water mark, so the max
//! sample is the arm's true peak RSS (the pattern of `tests/rss_bound.rs`) — and it reads
//! `/proc/loadavg` **before and after every round**. A nonzero child exit or a missing row is a
//! hard failure, never a skip.
//!
//! # The workload classes
//!
//! Both fixtures are **source-only** committed `.erl` files under `tests/fixtures/`, compiled at
//! test time by the system `erlc` into a per-class scratch directory holding only that module (the
//! house convention: no committed `.beam`, so the path-free guard applies and nothing reads the
//! fixture directory as a code path). A missing `erl`/`erlc` is a **loud skip**, never a pass.
//! Both classes run through a REAL spawned process driven by the real `run_one_slice` scheduler
//! loop — never the `-eval` route, which carries no process table and therefore no live process
//! GC at all.
//!
//! ## C3 — messaging / copy-bound (`tests/fixtures/gc_class_copy_bound.erl`)
//!
//! * `send_recv(N)` — a real two-process ping-pong: the measured process spawns a peer and does N
//!   round trips of a ~390-word payload over the **process copy boundary** (both directions),
//!   validating each returned term with a bound-variable pattern, every `receive` bounded by
//!   `after 60000`, and the peer **joined** through a monitor before the process returns. The
//!   returned last payload is checked element by element by the child.
//! * `ets_copy(N)` — the ETS copy boundary in the `tools/bench` `ets_copy` *shape* (re-created
//!   here from scratch, not imported): one table, one key, a fresh payload inserted and copied
//!   back out per round, the copy validated and the last copy returned.
//!
//! ## C4 — a real in-process compile (`tests/fixtures/gc_class_compile.erl`)
//!
//! `run(N)` compiles one real `lib/*/src/*.erl` source N times **inside the measured process**
//! (`no_spawn_compiler_process`, `deterministic`, `no_docs`, one `{outdir, Dir}` per compile), so
//! the compiler's own heap activity — readers, passes, code emission — is allocated on the heap
//! being measured. The child asserts each artifact is non-empty and **re-loads** it
//! (`Runtime::load_module`), so a plausible-looking-but-broken image fails; the parent re-checks
//! every artifact on disk. The source path is resolved from the repository root exactly as
//! `tests/compile_file.rs::source_root` does it.
//!
//! # The metrics, and the estimator
//!
//! Per arm per round the child reports, from `Runtime::gc_stats()` deltas and from the live
//! collector it samples after every scheduler slice: the runtime's minor/full-sweep counts,
//! `words_copied`, `objects_copied`, `words_reclaimed`, the process's own cumulative collection
//! counts, `mature_used_words`/`mature_capacity_words`, the **mean sampled nursery capacity** with
//! its sample count, the observed [`MatureKind`]/[`MatureChoice`], every GC failure counter, the
//! per-slice count and the wall time. The parent adds the child's peak `VmHWM` (kB) and the
//! `/proc/loadavg` before/after each round.
//!
//! Because the host is shared and loaded, **absolute wall times are not results**: the report pairs
//! each arm's round with the other arm's round and reports the **median paired difference**
//! (`Immix − Copying`) and the **median paired ratio** per metric, plus the min/max spread of the
//! differences. The noise-floor test measures one arm at a fixed N three times and reports the
//! spread as the band any claimed effect must beat.
//!
//! # What is asserted (counts and identities only — never time)
//!
//! In **both** debug and release, per round pair:
//!
//! 1. **the arm was real** — the intended [`MatureKind`], from the automatic rule, at the arm's
//!    configured mature geometry (asserted inside the measured process, not just parsed from its
//!    output);
//! 2. **the workload's value came back intact** — the returned term is checked element by element
//!    (C3), and every produced `.beam` is non-empty and re-loadable as an image (C4);
//! 3. **there really was GC** — `collections > 0` and `major > 0`, both in the runtime's own
//!    schedule and in the measured process's counters;
//! 4. **the allocation product agrees within 1 %** —
//!    `collections × mean sampled nursery capacity` is the young words the run's own heap doors
//!    admitted, and because both arms allocate exactly the same terms through the same live-set
//!    nursery rule it must match (see [the accounting identity](#the-cadence-is-an-accounting-identity));
//! 5. **the footprint axis has the strict sign where the shape holds a live payload** —
//!    `mature_used_words` is strictly larger under Immix (the line-granular price the sibling
//!    suite established on the allocation-bound shape);
//! 6. **no failure counter moved** — `errors`, `halted`, `valve_trips`, `disabled` are all clean.
//!
//! The copy volume, the collection counts and every wall figure are **printed and never asserted**:
//! their sign is regime-dependent (the sibling suite's analysis of the minor/major mix applies
//! verbatim — a minor sizes the nursery from the young live set once over, a fullsweep from the
//! whole-heap live set twice over).
//!
//! # The cadence is an accounting identity
//!
//! The two arms' collection **counts** are deliberately not compared: a percentage band on them is
//! a number fitted to the data, and it moves whenever one run shifts by a single collection. What
//! the mechanism fixes is a **product**. Every collection is triggered by a heap-admission door
//! refusing the next request because the *current* young space has no room left, so the words one
//! collection accounts for are its capacity minus the words the failing request could not fit:
//!
//! ```text
//! allocated young words = Σ_collections (capacity − free_at_admission)
//! ```
//!
//! The left-hand side is **strategy-independent** — the fixture allocates the same terms in the
//! same order under either mature strategy, and the mature generation's own capacity is not part of
//! the young allocation at all. The right-hand side is what the run samples, with two refinements
//! over the sibling suite's arithmetic mean:
//!
//! * the mean is **weighted by the collections observed in each slice** (`Σ kᵢ·Cᵢ / Σ kᵢ`, where
//!   `kᵢ` is the delta of the process's own collection counters across slice `i` and `Cᵢ` the
//!   nursery capacity sampled at the end of that slice), so the weight is the number of
//!   collections the capacity actually served — no uniformity assumption about how many words a
//!   slice allocates;
//! * the counts are the **measured process's own** (`minor_collections + major_collections`), not
//!   the runtime's, because a node-level counter also carries the peer process (C3) and the
//!   runtime's own service processes — allocations this class does not account for. The runtime
//!   delta is still printed as the authoritative schedule.
//!
//! Two residuals remain, both bounded rather than fitted: `Σ free_at_admission` (each interval is
//! short by the request that did not fit — the fixture's rounds allocate a ~390-word term one
//! binding at a time) and the **terminal slice** (the scheduler removes a terminated process at
//! once, so the collections of the slice in which the process returns are unobservable; it is one
//! partial slice out of hundreds, i.e. ≤ 1/`slices` of the run). The asserted tolerance is **1 %**.
//! What still fails: a run whose doors admitted the same words under a smaller nursery (collecting
//! without the allocation to justify it), a run that stopped collecting, and either residual
//! growing.
//!
//! # Debug cost
//!
//! The file must add no more than ~60 s to a debug `cargo test` run, so under
//! `cfg!(debug_assertions)` the rounds shrink to one and every class's N shrinks with them
//! (`n_for`); release keeps ≥ 3 rounds. The measured debug wall time is in the file's own report
//! (see the module table below), and no assertion is weakened in debug: the identity, the strategy
//! check, the value checks and the GC-count checks all still run.
//!
//! # The measured verdict
//!
//! See the *smoke measurement* section at the end of this comment: one release round of each class,
//! with both arms' rows. Every figure is printed by the run itself; nothing here is asserted.

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use beam_engine::terms;
use beam_gc::{GcStats, MatureKind, MatureSource, IMMIX_BREAK_EVEN_WORDS};
use beam_rt::config::{strategy_env_default, GcStrategy, HeapOptions};
use beam_rt::{Runtime, RuntimeConfig};
use beam_term::{is_atom, make_small, signed_value, Eterm};

// ---------------------------------------------------------------------------------------------
// The workload classes and their calibrated sizes
// ---------------------------------------------------------------------------------------------

/// C3 — the messaging / copy-bound class's two-process ping-pong shape.
const CLASS_SEND_RECV: &str = "send_recv";
/// C3 — the messaging / copy-bound class's ETS copy shape.
const CLASS_ETS_COPY: &str = "ets_copy";
/// C4 — the real in-process compile class.
const CLASS_COMPILE: &str = "compile";

/// Every work class this suite measures, in report order.
const CLASSES: &[&str] = &[CLASS_SEND_RECV, CLASS_ETS_COPY, CLASS_COMPILE];

/// The fullsweep cadence of **both** arms: two minors, then a fullsweep.
///
/// One shared value is what makes the two arms differ by the strategy alone. The emulator default
/// (65 535) would never reach a fullsweep in a few-second run, so `major > 0` — the assertion that
/// the mature generation is exercised at all — could not hold.
const FULLSWEEP_AFTER: u32 = 2;

/// `send_recv/1`'s calibrated N in a release build (round trips per arm).
const SEND_RECV_N_RELEASE: i64 = 40_000;
/// `send_recv/1`'s N in a debug build (a debug runtime does the same work far more slowly).
const SEND_RECV_N_DEBUG: i64 = 2_000;

/// `ets_copy/1`'s calibrated N in a release build (insert+lookup round trips per arm).
const ETS_COPY_N_RELEASE: i64 = 150_000;
/// `ets_copy/1`'s N in a debug build.
const ETS_COPY_N_DEBUG: i64 = 2_000;

/// `run/1`'s calibrated N in a release build — **the number of in-process compiles** per arm.
const COMPILE_N_RELEASE: i64 = 8;
/// `run/1`'s N in a debug build.
const COMPILE_N_DEBUG: i64 = 1;

/// The real OTP source the compile class compiles, relative to the repository root.
///
/// `lib/stdlib/src/ordsets.erl` is the module `tests/compile_file.rs` already compiles in process
/// (≈0.4 s of compile per round trip on this host, on the scheduled route), so three compiles are a
/// few seconds of real compiler heap activity without turning the class into a minute-long run.
const COMPILE_SOURCE: &str = "lib/stdlib/src/ordsets.erl";

/// The payload's element count, as both copy-bound shapes build it
/// (`gc_class_copy_bound:payload/1`'s 64 `{K, I, I + K}` tuples).
const PAYLOAD_LENGTH: usize = 64;

/// The key the returned payload must carry: both shapes return the payload of their **last**
/// round, and both count `N` down to 1.
const RETURNED_PAYLOAD_KEY: i64 = 1;

/// How many rounds each class measures in a release build.
const ROUNDS_RELEASE: usize = 3;
/// How many rounds a debug build measures (the file's debug budget).
const ROUNDS_DEBUG: usize = 1;

/// How many times the noise-floor test repeats its single-arm measurement.
const NOISE_REPEATS: usize = 3;

/// How long one child may run before the parent kills it — a generous safety net for a heavily
/// loaded host running a debug build, never a performance assertion.
const CHILD_DEADLINE: Duration = Duration::from_secs(900);

/// How often the parent polls a running child's `/proc/<pid>/status` for `VmHWM:`.
const VMHWM_POLL: Duration = Duration::from_millis(20);

/// The fixture source of the copy-bound class (compiled at test time into a scratch directory).
const COPY_BOUND_SOURCE: &str = include_str!("fixtures/gc_class_copy_bound.erl");
/// The fixture source of the compile class.
const COMPILE_SOURCE_ERL: &str = include_str!("fixtures/gc_class_compile.erl");

// ---------------------------------------------------------------------------------------------
// The environment protocol between the parent and its child processes
// ---------------------------------------------------------------------------------------------

/// Which class the child runs ([`CLASS_SEND_RECV`]/[`CLASS_ETS_COPY`]/[`CLASS_COMPILE`]).
const ENV_CLASS: &str = "BEAM_RT_GCH_CLASS";
/// Which arm the child runs (`copying`/`immix`).
const ENV_ARM: &str = "BEAM_RT_GCH_ARM";
/// The class's N for this run.
const ENV_N: &str = "BEAM_RT_GCH_N";
/// The round index (reporting only — the child measures one arm once).
const ENV_ROUND: &str = "BEAM_RT_GCH_ROUND";
/// The `.beam` the child loads as its fixture module.
const ENV_FIXTURE: &str = "BEAM_RT_GCH_FIXTURE";
/// The directory the compile class's `{outdir, …}` directories live under (the child creates
/// nothing: the parent creates `out1..outN`). Read by the fixture through `os:getenv/1`.
const ENV_OUT: &str = "BEAM_RT_GCH_OUT";
/// The real OTP source the compile class compiles. Read by the fixture through `os:getenv/1`.
const ENV_SRC: &str = "BEAM_RT_GCH_SRC";
/// The module name the compile class's source declares (the artifact's file stem).
const ENV_MODULE_EXPECTED: &str = "BEAM_RT_GCH_MODULE";

/// The name of the `#[ignore]`d worker test the parent re-execs.
const WORKER: &str = "child_runs_one_arm_and_prints_a_row";

/// The prefix of the one machine-readable row a child prints.
const ROW_PREFIX: &str = "ROW ";

// ---------------------------------------------------------------------------------------------
// The arms
// ---------------------------------------------------------------------------------------------

/// One arm of the A/B: the copying mature generation, or the mark-region (`Immix`) one.
///
/// The arm *is* its configured mature-generation size (the only lever the runtime has); everything
/// else is shared, so the two configurations differ by one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// The barrier-free Cheney copying mature generation (`initial_mature_words` below break-even).
    Copying,
    /// The GenImmix mark-region mature generation (at break-even).
    Immix,
}

impl Arm {
    /// Both arms, in the order a round interleaves them.
    const BOTH: [Arm; 2] = [Arm::Copying, Arm::Immix];

    /// The arm's name, as it appears in the row and in the environment.
    fn name(self) -> &'static str {
        match self {
            Arm::Copying => "copying",
            Arm::Immix => "immix",
        }
    }

    /// The arm named `name`, or `None` for anything else.
    fn from_name(name: &str) -> Option<Arm> {
        Arm::BOTH.into_iter().find(|arm| arm.name() == name)
    }

    /// The `initial_mature_words` this arm configures: one word below the rule's break-even for
    /// the copying space, exactly at it for the mark-region space.
    fn initial_mature_words(self) -> usize {
        match self {
            Arm::Copying => IMMIX_BREAK_EVEN_WORDS - 1,
            Arm::Immix => IMMIX_BREAK_EVEN_WORDS,
        }
    }

    /// The `MatureKind` the automatic rule must resolve to for this arm — the check that makes the
    /// whole experiment meaningful.
    fn expected_kind(self) -> MatureKind {
        match self {
            Arm::Copying => MatureKind::Copying,
            Arm::Immix => MatureKind::Immix,
        }
    }

    /// The heap options of this arm: its mature geometry plus the shared cadence.
    fn heap_options(self) -> HeapOptions {
        HeapOptions::new()
            .with_initial_mature_words(self.initial_mature_words())
            .with_fullsweep_after(FULLSWEEP_AFTER)
    }

    /// The runtime configuration of this arm.
    fn config(self) -> RuntimeConfig {
        RuntimeConfig::new().with_heap_options(self.heap_options())
    }
}

// ---------------------------------------------------------------------------------------------
// Guard rails
// ---------------------------------------------------------------------------------------------

/// The measurement lock: `cargo test` runs the tests of one binary in parallel threads, and two
/// classes measuring at once would both pollute `/proc/loadavg` and steal each other's cores.
///
/// Every parent test takes it for the whole class. A poisoned lock (a previous class failed) is
/// recovered rather than cascading: the failure that poisoned it is already reported, and the next
/// class's own assertions must still run.
fn measurement_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Refuses to measure when the ambient environment carries the host GC-strategy opt-in.
///
/// `Runtime::new` applies `BEAM_RT_GC_STRATEGY` **after** the crate's own heap options
/// ([`strategy_env_default`] → `HeapOptions::set_gc_strategy`), so an inherited
/// `BEAM_RT_GC_STRATEGY=immix` would overwrite the copying arm's 2 047-word geometry and make both
/// arms mark-region. That is the one silent-fallback failure mode this harness must not tolerate,
/// and it cannot be skipped politely (a skip would read as a pass), so the suite **fails loudly**
/// and names the variable. The suite never sets it itself.
fn refuse_ambient_strategy(where_: &str) {
    if let Some(value) = std::env::var_os("BEAM_RT_GC_STRATEGY") {
        panic!(
            "{where_}: BEAM_RT_GC_STRATEGY is set in this environment ({value:?}), and the runtime \
             applies it after the arm's own heap options — both arms would then be the same \
             strategy. Unset it and re-run: the arms are installed through \
             `RuntimeConfig::with_heap_options` only."
        );
    }
    // Belt and braces: if the parsed default is anything but the emulator default, the environment
    // carried an opt-in this process parsed before the check above could see it.
    assert_eq!(
        strategy_env_default(),
        GcStrategy::EmulatorDefault,
        "{where_}: the parsed GC-strategy default must be the emulator default"
    );
}

// ---------------------------------------------------------------------------------------------
// The child's one row
// ---------------------------------------------------------------------------------------------

/// Everything one measured arm reports: the runtime's schedule, the measured process's own
/// collector figures, the strategy that was really in effect, the verification outcome and the
/// wall time (printed, never asserted).
///
/// The child prints it as one `ROW key=value …` line; the parent parses those keys back into this
/// struct and adds the two figures it observed itself (peak `VmHWM`, `/proc/loadavg`).
#[derive(Debug, Clone)]
struct Row {
    /// The class this arm measured.
    class: String,
    /// The arm.
    arm: Arm,
    /// Which round of the class's schedule this was.
    round: usize,
    /// The class's N for this run.
    n: i64,
    /// The strategy the measured process's own collector really ran.
    kind: MatureKind,
    /// Whether that strategy came from the automatic rule (it must) or a pin (it must not).
    automatic: bool,
    /// The mature-generation size the arm's policy carried, in words (`MatureChoice`).
    choice_words: usize,
    /// The runtime's collection count during the run (its `gc_stats()` delta) — the authoritative
    /// schedule, including every other process on the node.
    collections: u64,
    /// Fullsweeps among them.
    major: u64,
    /// The **measured process's own** cumulative collections while it lived (the identity's
    /// count, a floor: the terminal slice is unobservable once the scheduler removes the process).
    own_collections: u64,
    /// Its own fullsweeps.
    own_major: u64,
    /// The process's `GcStats::words_copied`.
    words_copied: usize,
    /// The process's `GcStats::objects_copied`.
    objects_copied: usize,
    /// The process's `GcStats::words_reclaimed` (unit caveat: whole lines under the mark-region
    /// space, exact object words under the copying one).
    words_reclaimed: usize,
    /// `collector().mature_used_words()` at the last sample.
    mature_used: usize,
    /// `collector().mature_capacity_words()` at the last sample.
    mature_capacity: usize,
    /// The largest mature capacity observed at any slice boundary — the arm's configured geometry
    /// in effect.
    max_mature_capacity: usize,
    /// The largest **current-space** nursery capacity seen at any slice boundary (never
    /// `heap_size_words()`'s chain sum: a growth retires a space no collection drains).
    peak_nursery: usize,
    /// The collection-weighted mean nursery capacity — the identity's second input
    /// (`Σ kᵢ·Cᵢ / Σ kᵢ`).
    mean_nursery: usize,
    /// The plain arithmetic mean of the same samples, for context.
    mean_nursery_flat: usize,
    /// How many slice boundaries the means were taken over.
    nursery_samples: u64,
    /// Scheduler slices the runtime ran.
    slices: u64,
    /// The runtime's allocation fallbacks to the pointer-stable arena (context: an arm that fell
    /// back more admitted fewer words through the nursery it measured).
    fallbacks: u64,
    /// `gc_stats().errors` — must be `0`.
    errors: u64,
    /// `gc_stats().halted` — must be `0`.
    halted: usize,
    /// `gc_stats().valve_trips` — must be `0`.
    valve_trips: u64,
    /// `gc_stats().disabled` — must be `false`.
    disabled: bool,
    /// The workload's value passed its structural check (element by element, or the produced
    /// artifacts).
    verified: bool,
    /// How many compile artifacts the child re-loaded (0 for the other classes).
    artifacts: u64,
    /// The summed byte size of those artifacts.
    artifact_bytes: u64,
    /// The run's wall time in milliseconds — printed, never asserted.
    wall_ms: f64,
}

impl Row {
    /// The row's one machine-readable line.
    fn to_line(&self) -> String {
        format!(
            "{ROW_PREFIX}class={} arm={} round={} n={} kind={} automatic={} choice_words={} \
             collections={} major={} own_collections={} own_major={} words_copied={} \
             objects_copied={} words_reclaimed={} mature_used={} mature_capacity={} \
             max_mature_capacity={} peak_nursery={} mean_nursery={} mean_nursery_flat={} \
             nursery_samples={} slices={} fallbacks={} errors={} halted={} valve_trips={} \
             disabled={} verified={} artifacts={} artifact_bytes={} wall_ms={}",
            self.class,
            self.arm.name(),
            self.round,
            self.n,
            kind_name(self.kind),
            self.automatic,
            self.choice_words,
            self.collections,
            self.major,
            self.own_collections,
            self.own_major,
            self.words_copied,
            self.objects_copied,
            self.words_reclaimed,
            self.mature_used,
            self.mature_capacity,
            self.max_mature_capacity,
            self.peak_nursery,
            self.mean_nursery,
            self.mean_nursery_flat,
            self.nursery_samples,
            self.slices,
            self.fallbacks,
            self.errors,
            self.halted,
            self.valve_trips,
            self.disabled,
            self.verified,
            self.artifacts,
            self.artifact_bytes,
            self.wall_ms,
        )
    }

    /// Parses a child's `ROW …` line back into a row, naming `line` in every failure.
    fn from_line(line: &str) -> Row {
        let fields = Fields::parse(line);
        let kind = match fields.text("kind") {
            "Copying" => MatureKind::Copying,
            "Immix" => MatureKind::Immix,
            other => panic!("`kind={other}` is not a MatureKind in: {line}"),
        };
        let arm = Arm::from_name(fields.text("arm"))
            .unwrap_or_else(|| panic!("`arm={}` is not an arm in: {line}", fields.text("arm")));
        Row {
            class: fields.text("class").to_string(),
            arm,
            round: fields.uint("round") as usize,
            n: fields.int("n"),
            kind,
            automatic: fields.flag("automatic"),
            choice_words: fields.uint("choice_words") as usize,
            collections: fields.uint("collections"),
            major: fields.uint("major"),
            own_collections: fields.uint("own_collections"),
            own_major: fields.uint("own_major"),
            words_copied: fields.uint("words_copied") as usize,
            objects_copied: fields.uint("objects_copied") as usize,
            words_reclaimed: fields.uint("words_reclaimed") as usize,
            mature_used: fields.uint("mature_used") as usize,
            mature_capacity: fields.uint("mature_capacity") as usize,
            max_mature_capacity: fields.uint("max_mature_capacity") as usize,
            peak_nursery: fields.uint("peak_nursery") as usize,
            mean_nursery: fields.uint("mean_nursery") as usize,
            mean_nursery_flat: fields.uint("mean_nursery_flat") as usize,
            nursery_samples: fields.uint("nursery_samples"),
            slices: fields.uint("slices"),
            // A class named by the harness but measured with another name is a protocol bug.
            // (`class` is checked by the caller against the class it asked for.)
            fallbacks: fields.uint("fallbacks"),
            errors: fields.uint("errors"),
            halted: fields.uint("halted") as usize,
            valve_trips: fields.uint("valve_trips"),
            disabled: fields.flag("disabled"),
            verified: fields.flag("verified") as bool,
            artifacts: fields.uint("artifacts"),
            artifact_bytes: fields.uint("artifact_bytes"),
            wall_ms: fields.number("wall_ms"),
        }
    }

    /// The young words this run's own heap doors admitted:
    /// `own_collections × mean_nursery` — the accounting identity's left-hand side.
    fn allocation_product(&self) -> u128 {
        u128::from(self.own_collections) * self.mean_nursery as u128
    }
}

/// The `kind` field's spelling (the `Debug` name, kept stable against a `#[derive]` change).
fn kind_name(kind: MatureKind) -> &'static str {
    match kind {
        MatureKind::Copying => "Copying",
        MatureKind::Immix => "Immix",
    }
}

/// The parsed `key=value` body of a `ROW …` line.
struct Fields(BTreeMap<String, String>);

impl Fields {
    /// Parses the tokens after the [`ROW_PREFIX`], panicking on a malformed line.
    fn parse(line: &str) -> Fields {
        let body = line
            .trim()
            .strip_prefix(ROW_PREFIX)
            .unwrap_or_else(|| panic!("not a row line: {line}"));
        let mut map = BTreeMap::new();
        for token in body.split_whitespace() {
            let (key, value) = token
                .split_once('=')
                .unwrap_or_else(|| panic!("`{token}` is not a key=value pair in: {line}"));
            map.insert(key.to_string(), value.to_string());
        }
        Fields(map)
    }

    /// The raw text of `key`.
    fn text(&self, key: &str) -> &str {
        self.0
            .get(key)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("the row carries no `{key}` (keys: {:?})", self.0.keys()))
    }

    /// `key` as a float.
    fn number(&self, key: &str) -> f64 {
        self.text(key)
            .parse()
            .unwrap_or_else(|_| panic!("`{key}={}` is not a number", self.text(key)))
    }

    /// `key` as a signed integer.
    fn int(&self, key: &str) -> i64 {
        self.text(key)
            .parse()
            .unwrap_or_else(|_| panic!("`{key}={}` is not an integer", self.text(key)))
    }

    /// `key` as an unsigned integer.
    fn uint(&self, key: &str) -> u64 {
        self.text(key)
            .parse()
            .unwrap_or_else(|_| panic!("`{key}={}` is not an unsigned integer", self.text(key)))
    }

    /// `key` as a boolean.
    fn flag(&self, key: &str) -> bool {
        match self.text(key) {
            "true" => true,
            "false" => false,
            other => panic!("`{key}={other}` is not a boolean"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The child: one arm, one process, one row
// ---------------------------------------------------------------------------------------------

/// The environment a child is given, decoded.
struct ChildSpec {
    /// Which class to run.
    class: String,
    /// Which arm to run.
    arm: Arm,
    /// The class's N.
    n: i64,
    /// The round (reporting only).
    round: usize,
    /// The `.beam` to load as the fixture module.
    fixture: PathBuf,
}

impl ChildSpec {
    /// Reads the child's environment, panicking (loudly) on anything missing or malformed.
    fn from_env() -> ChildSpec {
        let var = |key: &str| {
            std::env::var(key).unwrap_or_else(|_| {
                panic!(
                    "the GC-class harness worker was started without {key}: it is re-executed by \
                     the parent's `gc_strategy_classes` tests, never run by hand"
                )
            })
        };
        let class = var(ENV_CLASS);
        assert!(
            CLASSES.contains(&class.as_str()),
            "`{class}` is not one of the harness's classes ({CLASSES:?})"
        );
        let arm = var(ENV_ARM);
        let arm = Arm::from_name(&arm)
            .unwrap_or_else(|| panic!("`{arm}` is not one of the arms (copying/immix)"));
        ChildSpec {
            class,
            arm,
            n: var(ENV_N)
                .parse()
                .unwrap_or_else(|_| panic!("{ENV_N} must be an integer")),
            round: var(ENV_ROUND)
                .parse()
                .unwrap_or_else(|_| panic!("{ENV_ROUND} must be an integer")),
            fixture: PathBuf::from(var(ENV_FIXTURE)),
        }
    }

    /// The fixture module's name, and the function it exports for this class.
    fn entry(&self) -> (&'static str, &'static str) {
        fixture_entry(&self.class)
    }
}

/// The `(module, function)` a class runs: the compiled fixture and the entry point inside it.
fn fixture_entry(class: &str) -> (&'static str, &'static str) {
    match class {
        CLASS_SEND_RECV => ("gc_class_copy_bound", "send_recv"),
        CLASS_ETS_COPY => ("gc_class_copy_bound", "ets_copy"),
        CLASS_COMPILE => ("gc_class_compile", "run"),
        other => panic!("no fixture entry for class `{other}`"),
    }
}

/// **The worker: one class, one arm, one fresh runtime, one row.**
///
/// `#[ignore]`d so it never runs as part of the ordinary suite: the parent re-execs this binary with
/// `--exact child_runs_one_arm_and_prints_a_row --ignored`, the class/arm/N/round coming from the
/// environment ([`ENV_CLASS`] …). Its stdout is one [`ROW_PREFIX`] line; a nonzero exit is a
/// failure the parent reports with the whole transcript.
#[test]
#[ignore]
fn child_runs_one_arm_and_prints_a_row() {
    let row = child_measure();
    println!("{}", row.to_line());
}

/// Builds the arm's runtime, runs the class's workload as a real scheduled process, samples the live
/// collector after every slice, verifies the return value and assembles the row.
///
/// Every assertion here is one of the file's six: the intended strategy really was in effect, the
/// workload's value came back intact, there really was GC, and no failure counter moved. (The
/// allocation-product identity and the footprint sign are cross-arm, so the parent asserts those.)
fn child_measure() -> Row {
    refuse_ambient_strategy("gc_strategy_classes worker");
    let spec = ChildSpec::from_env();
    let (module, function) = spec.entry();

    let mut rt = Runtime::new(spec.arm.config()).expect("the arm's runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(&spec.fixture).expect("the fixture loads");

    let before = rt.gc_stats();
    let pid = rt
        .spawn_process(module, function, vec![make_small(spec.n)])
        .expect("the workload is spawned");

    let started = Instant::now();
    let mut sampled = Sampled::default();
    let deadline = started + CHILD_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "{module}:{function}/1 with N={} did not terminate within {}s",
            spec.n,
            CHILD_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        sampled.slices += 1;
        if let Some(process) = rt.processes().get(pid) {
            sampled.observe(process.collector());
        }
    }
    let wall = started.elapsed();
    let after = rt.gc_stats();

    // The workload returned its value: verify it structurally before anything is reported.
    let value = rt
        .process_result(pid)
        .expect("the workload returned its value");
    let (verified, artifacts, artifact_bytes) = match spec.class.as_str() {
        CLASS_COMPILE => verify_compile(&mut rt, value, spec.n, &compiled_dir(&spec)),
        _ => {
            check_round_tripped_payload(value);
            (true, 0, 0)
        }
    };

    // The arm really was the arm: the strategy the measured process's own collector ran, from the
    // automatic rule, at the configured geometry. This is the experiment's load-bearing assertion.
    let kind = sampled
        .kind
        .expect("the process was observed alive at least once");
    assert_eq!(
        kind,
        spec.arm.expected_kind(),
        "the {} arm must really run {kind:?} (observed {:?})",
        spec.arm.name(),
        kind
    );
    let choice = sampled
        .choice
        .expect("the process's MatureChoice was observed");
    assert_eq!(
        choice.source,
        MatureSource::Automatic,
        "the strategy must come from the automatic rule, not a pin: {choice:?}"
    );
    assert_eq!(
        choice.mature_words,
        spec.arm.initial_mature_words(),
        "the arm's configured mature generation must be the policy's ({choice:?})"
    );
    assert_eq!(
        choice.kind, kind,
        "the reported choice and the collector's kind must agree ({choice:?}, {kind:?})"
    );
    assert!(
        sampled.max_mature_capacity >= spec.arm.initial_mature_words(),
        "the mature space's reported geometry (max {} words) must reflect the arm's configured \
         {} words",
        sampled.max_mature_capacity,
        spec.arm.initial_mature_words()
    );

    // There really was GC — in the measured process and in the runtime's own schedule.
    assert!(
        sampled.own_collections > 0,
        "the workload must drive process-heap collections (class {}, arm {})",
        spec.class,
        spec.arm.name()
    );
    assert!(
        sampled.own_major > 0,
        "the cadence (fullsweep_after={FULLSWEEP_AFTER}) must reach a fullsweep in the measured \
         process (class {}, arm {})",
        spec.class,
        spec.arm.name()
    );
    assert!(
        after.collections > before.collections,
        "the runtime's own schedule must show the collections (class {}, arm {})",
        spec.class,
        spec.arm.name()
    );
    assert!(
        after.major > before.major,
        "the runtime's own schedule must show a fullsweep (class {}, arm {})",
        spec.class,
        spec.arm.name()
    );
    // And no failure counter moved.
    assert_eq!(after.errors, 0, "no collection failed: {after:?}");
    assert_eq!(after.halted, 0, "no process halted: {after:?}");
    assert_eq!(after.valve_trips, 0, "no escape-store valve: {after:?}");
    assert!(!after.disabled, "collection was never disabled: {after:?}");

    Row {
        class: spec.class,
        arm: spec.arm,
        round: spec.round,
        n: spec.n,
        kind,
        automatic: choice.source == MatureSource::Automatic,
        choice_words: choice.mature_words,
        collections: after.collections - before.collections,
        major: after.major - before.major,
        own_collections: sampled.own_collections,
        own_major: sampled.own_major,
        words_copied: sampled.stats.words_copied,
        objects_copied: sampled.stats.objects_copied,
        words_reclaimed: sampled.stats.words_reclaimed,
        mature_used: sampled.mature_used,
        mature_capacity: sampled.mature_capacity,
        max_mature_capacity: sampled.max_mature_capacity,
        peak_nursery: sampled.peak_nursery,
        mean_nursery: sampled.mean_nursery(),
        mean_nursery_flat: sampled.mean_nursery_flat(),
        nursery_samples: sampled.samples,
        slices: sampled.slices,
        fallbacks: after.fallbacks - before.fallbacks,
        errors: after.errors,
        halted: after.halted,
        valve_trips: after.valve_trips,
        disabled: after.disabled,
        verified,
        artifacts,
        artifact_bytes,
        wall_ms: wall.as_secs_f64() * 1_000.0,
    }
}

/// The accumulated per-slice samples of one measured process.
///
/// The nursery mean is **collection-weighted**: `Σ kᵢ·Cᵢ / Σ kᵢ`, where `kᵢ` is how many collections
/// the process's own counters gained across slice `i` and `Cᵢ` the current-space nursery capacity
/// sampled at the end of that slice. That weight is exactly the number of collections the capacity
/// served, which is what the accounting identity `Σ_collections capacity = collections × mean` needs
/// — no assumption that every slice allocates the same number of words.
#[derive(Default)]
struct Sampled {
    /// The strategy observed at the last sample.
    kind: Option<MatureKind>,
    /// The resolved choice observed at the last sample.
    choice: Option<beam_gc::MatureChoice>,
    /// The process's cumulative counters at the last sample.
    stats: GcStats,
    /// Mature occupancy at the last sample.
    mature_used: usize,
    /// Mature capacity at the last sample.
    mature_capacity: usize,
    /// Largest mature capacity at any sample.
    max_mature_capacity: usize,
    /// Largest current-space nursery capacity at any sample.
    peak_nursery: usize,
    /// The process's own collections at the last sample.
    own_collections: u64,
    /// The process's own fullsweeps at the last sample.
    own_major: u64,
    /// `Σ kᵢ·Cᵢ`.
    weighted_capacity: u128,
    /// `Σ Cᵢ`, for the plain arithmetic mean.
    capacity_sum: u128,
    /// `Σ kᵢ` over the samples (equals `own_collections` when the first sample already carries a
    /// collection).
    weighted_collections: u64,
    /// How many slice boundaries were sampled.
    samples: u64,
    /// How many scheduler slices ran.
    slices: u64,
}

impl Sampled {
    /// Folds one live collector sample in, weighting the nursery capacity by the collections the
    /// process performed in the slice this sample closes.
    fn observe(&mut self, collector: &beam_gc::AnyCollector) {
        self.kind = Some(collector.kind());
        self.choice = Some(collector.mature_choice());
        self.stats = collector.stats();
        self.mature_used = collector.mature_used_words();
        self.mature_capacity = collector.mature_capacity_words();
        self.max_mature_capacity = self.max_mature_capacity.max(self.mature_capacity);
        // The *current* space, never `heap_size_words()`'s chain sum: a growth retires a space no
        // collection drains, so a chain sum would count capacity no collection accounts for.
        let capacity = collector.young().capacity_words();
        self.peak_nursery = self.peak_nursery.max(capacity);
        self.capacity_sum += capacity as u128;
        self.samples += 1;
        let own = self.stats.minor_collections + self.stats.major_collections;
        let collections = own.saturating_sub(self.own_collections);
        self.own_collections = own;
        self.own_major = self.stats.major_collections;
        self.weighted_collections += collections;
        self.weighted_capacity += u128::from(collections) * capacity as u128;
    }

    /// The collection-weighted mean nursery capacity, in words.
    fn mean_nursery(&self) -> usize {
        if self.weighted_collections == 0 {
            return 0;
        }
        (self.weighted_capacity / u128::from(self.weighted_collections)) as usize
    }

    /// The plain arithmetic mean of the nursery capacity samples, in words (context only).
    fn mean_nursery_flat(&self) -> usize {
        if self.samples == 0 {
            return 0;
        }
        (self.capacity_sum / u128::from(self.samples)) as usize
    }
}

// ---------------------------------------------------------------------------------------------
// The workload's value: the copy-bound payload, and the compile's artifacts
// ---------------------------------------------------------------------------------------------

/// Verifies a copy-bound shape's returned `{K, [{K, I, I + K} | …]}` **element by element** through
/// the frozen term accessors only.
///
/// This is `support::check_gc_payload`'s idea applied to this fixture's payload: the term came back
/// through a real copy boundary (the sending process's heap, the receiver's heap, ETS's table
/// store), so a collector that relocated a heap against an incomplete root set — or a copier that
/// flattened one incorrectly — would corrupt it, and every field is checked.
fn check_round_tripped_payload(payload: Eterm) {
    let key = signed_value(
        terms::tuple_element(payload, 0).expect("the payload is the `{K, …}` 2-tuple"),
    );
    assert_eq!(
        key, RETURNED_PAYLOAD_KEY,
        "the returned payload is the last round's (key {RETURNED_PAYLOAD_KEY})"
    );
    let list = terms::tuple_element(payload, 1).expect("the payload's second field is its list");
    let elements = terms::list_elements(list).expect("the payload's list is a proper list");
    assert_eq!(
        elements.len(),
        PAYLOAD_LENGTH,
        "the payload's element count survived the copy boundary and the collections"
    );
    for (index, element) in elements.iter().enumerate() {
        let i = index as i64 + 1;
        let field = |position: usize| {
            signed_value(terms::tuple_element(*element, position).unwrap_or_else(|error| {
                panic!("payload element {index} field {position}: {error:?}")
            }))
        };
        assert_eq!(field(0), key, "payload element {index} key");
        assert_eq!(field(1), i, "payload element {index} index");
        assert_eq!(field(2), i + key, "payload element {index} index + key");
    }
}

/// The `{outdir, …}` root the compile class writes into: the same [`ENV_OUT`] the fixture read.
fn compiled_dir(spec: &ChildSpec) -> PathBuf {
    PathBuf::from(std::env::var(ENV_OUT).unwrap_or_else(|_| {
        panic!("{ENV_OUT} must name the compile class's output root (class {})", spec.class)
    }))
}

/// Verifies the compile class's returned `{N, Module}` and its produced artifacts.
///
/// The value: `N` must be the run's N and `Module` an atom. The **product**: every `out<i>/<mod>.beam`
/// must be a non-empty image that this runtime can really **load** (`Runtime::load_module`), so a
/// compile that wrote a plausible-looking but broken artifact fails here rather than in a later
/// consumer. Answers `(verified, artifacts, total bytes)`.
fn verify_compile(rt: &mut Runtime, value: Eterm, n: i64, out_root: &Path) -> (bool, u64, u64) {
    let count = signed_value(terms::tuple_element(value, 0).expect("the compile result is a 2-tuple"));
    assert_eq!(count, n, "the compile class answers `{{N, Module}}` with its own N");
    let module = terms::tuple_element(value, 1).expect("the compile result's second field");
    assert!(
        is_atom(module),
        "the compile class answers the last compile's module name as an atom"
    );
    let module_name = std::env::var(ENV_MODULE_EXPECTED).ok();
    let mut artifacts = 0u64;
    let mut bytes = 0u64;
    for index in 1..=n {
        let dir = out_root.join(format!("out{index}"));
        let artifact = match &module_name {
            Some(name) => dir.join(format!("{name}.beam")),
            None => {
                // Without an expected name, find the single artifact the compile wrote.
                let entries: Vec<PathBuf> = std::fs::read_dir(&dir)
                    .unwrap_or_else(|error| {
                        panic!("the compile class's {} is unreadable: {error}", dir.display())
                    })
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .filter(|path| path.extension().is_some_and(|ext| ext == "beam"))
                    .collect();
                assert_eq!(
                    entries.len(),
                    1,
                    "compile {index} must write exactly one artifact into {}: {entries:?}",
                    dir.display()
                );
                entries[0].clone()
            }
        };
        let image = std::fs::read(&artifact).unwrap_or_else(|error| {
            panic!("the compile class wrote no {}: {error}", artifact.display())
        });
        assert!(
            !image.is_empty(),
            "compile {index}'s artifact {} must be non-empty",
            artifact.display()
        );
        rt.load_module(&artifact).unwrap_or_else(|error| {
            panic!(
                "compile {index}'s artifact {} must be a loadable image: {error:?}",
                artifact.display()
            )
        });
        artifacts += 1;
        bytes += image.len() as u64;
    }
    (true, artifacts, bytes)
}

// ---------------------------------------------------------------------------------------------
// The parent: scratch space, fixture compilation, repository source discovery
// ---------------------------------------------------------------------------------------------

/// The banner every [`support::skip_loudly`] call in this suite prints.
const BANNER: &str = "gc_strategy_classes";

/// A per-test, self-removing scratch directory.
///
/// The name carries the label, the pid, a per-process nonce and a per-call sequence number, so two
/// tests (or two concurrently running test *processes*, or a leftover directory from an earlier
/// run) can never share a path — the rule the crate's own `context::test_support::scratch_dir`
/// follows, re-stated here because that helper is `pub(crate)` and this is a separate crate.
struct Scratch {
    /// The directory this guard owns.
    path: PathBuf,
}

impl Scratch {
    /// The directory this guard owns.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Creates a fresh scratch directory for `label`.
fn scratch_dir(label: &str) -> Scratch {
    static NONCE: OnceLock<u64> = OnceLock::new();
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let nonce = *NONCE.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        nanos ^ (u64::from(std::process::id()) << 32)
    });
    let sequence = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "beamrt_gc_classes_{label}_{}_{nonce:016x}_{sequence}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).unwrap_or_else(|error| {
        panic!(
            "cannot create the scratch directory {}: {error}",
            path.display()
        )
    });
    Scratch { path }
}

/// The system `erlc` — the fixtures are `.erl` sources, so a missing compiler is a loud skip.
fn require_erlc() -> Option<PathBuf> {
    match support::find_on_path("erlc") {
        Some(erlc) => Some(erlc),
        None => {
            support::skip_loudly(
                BANNER,
                "no `erlc` on $PATH (the workload fixtures cannot be compiled)",
            );
            None
        }
    }
}

/// Writes `source` into `dir` under the module name `module` and compiles it with `erlc`, answering
/// the `.beam`'s path.
///
/// The committed fixtures are named after the *test class*, not after the module they define
/// (`gc_class_copy_bound.erl` does define `gc_class_copy_bound`, but the convention is kept
/// uniform), and `erlc` refuses a mismatch, so every source is materialised under its module name
/// first. `tests/fixtures/` is deliberately never passed as a code path: it holds a `lists.beam`
/// that would shadow stdlib, so only this scratch directory is ever named on `-o`.
fn compile_fixture(erlc: &Path, dir: &Path, module: &str, source: &str) -> PathBuf {
    let source_path = dir.join(format!("{module}.erl"));
    std::fs::write(&source_path, source)
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", source_path.display()));
    let out_dir = dir.to_string_lossy().into_owned();
    let source_arg = source_path.to_string_lossy().into_owned();
    let Some(output) = support::run_bounded(erlc, &["-o", &out_dir, &source_arg]) else {
        panic!(
            "`erlc` could not compile {} (bounded probe)",
            source_path.display()
        );
    };
    assert!(
        output.status.success(),
        "`erlc` failed on {} with {:?}:\n{}",
        source_path.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let beam = dir.join(format!("{module}.beam"));
    assert!(beam.is_file(), "`erlc` produced no {}", beam.display());
    beam
}

/// The repository root: `$ERL_TOP` when set, else three levels up from this crate's manifest
/// directory (`<root>/erts_rust/crates/beam_rt`) — the discovery `tests/compile_file.rs` uses.
fn source_root() -> PathBuf {
    match std::env::var_os("ERL_TOP") {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
    }
}

/// The repository source the compile class compiles, or a **loud skip** when the checkout has no
/// source tree (an installed-only or relocated build).
fn compile_source() -> Option<PathBuf> {
    let path = source_root().join(COMPILE_SOURCE);
    if path.is_file() {
        return Some(path);
    }
    support::skip_loudly(
        BANNER,
        &format!(
            "no {COMPILE_SOURCE} at {} (the compile class needs a source checkout)",
            path.display()
        ),
    );
    None
}

// ---------------------------------------------------------------------------------------------
// The parent: one child per (class, arm, round)
// ---------------------------------------------------------------------------------------------

/// What the parent itself observed about one child, alongside the child's own row.
struct ChildRun {
    /// The child's one machine-readable row.
    row: Row,
    /// The largest `VmHWM` (kB) sampled while the child lived — the kernel's monotonic watermark,
    /// so the max sample is the arm's true peak.
    peak_vmhwm_kib: u64,
    /// How many `/proc/<pid>/status` samples contributed (`0` ⇒ the whole run was missed).
    hwm_samples: usize,
    /// `/proc/loadavg` immediately before the child was spawned.
    load_before: String,
    /// `/proc/loadavg` immediately after the child was reaped.
    load_after: String,
}

/// One unit of work: which class, which arm, which N, and where its inputs live.
struct ChildJob<'a> {
    /// The class name (already validated).
    class: &'a str,
    /// The arm.
    arm: Arm,
    /// The class's N.
    n: i64,
    /// The round this measurement belongs to (reporting only).
    round: usize,
    /// The compiled fixture `.beam` the child loads.
    fixture: &'a Path,
    /// The compile class's `{outdir, …}` root (the parent creates `out1..outN`).
    out_root: Option<&'a Path>,
    /// The real OTP source the compile class compiles.
    source: Option<&'a Path>,
    /// The module name that source declares (the artifact's file stem).
    module_expected: Option<&'a str>,
}

/// Reads a field of `/proc/<pid>/status` such as `VmHWM:` (value in kB), or `None` when the process
/// is already gone (`/proc` entry removed) or `/proc` is not mounted.
fn read_status_field(pid: u32, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines().find_map(|line| {
        line.strip_prefix(field)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

/// The current `/proc/loadavg` line (for the printed report), or `"unknown"`.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Spawns **one** child to measure one arm, polls its `VmHWM`, parses its row and checks that it
/// exited cleanly.
///
/// The child is this same test binary re-exec'd as the only test in its process
/// (`--exact <WORKER> --ignored --nocapture --test-threads 1`), with [`ENV_CLASS`] … selecting the
/// work. It is **polled** rather than waited on, so a hung arm is a loud failure instead of a hung
/// suite. A nonzero child exit or a missing `ROW` line is a hard failure — never a skip.
fn run_child(job: &ChildJob<'_>) -> ChildRun {
    let exe = std::env::current_exe().expect("the test binary's own path");
    let mut command = Command::new(&exe);
    command
        .args([
            "--exact",
            WORKER,
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env(ENV_CLASS, job.class)
        .env(ENV_ARM, job.arm.name())
        .env(ENV_N, job.n.to_string())
        .env(ENV_ROUND, job.round.to_string())
        .env(ENV_FIXTURE, job.fixture);
    for (key, value) in [
        (ENV_OUT, job.out_root),
        (ENV_SRC, job.source),
    ] {
        match value {
            Some(path) => {
                command.env(key, path);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    match job.module_expected {
        Some(name) => {
            command.env(ENV_MODULE_EXPECTED, name);
        }
        None => {
            command.env_remove(ENV_MODULE_EXPECTED);
        }
    }
    // The runtime applies `BEAM_RT_GC_STRATEGY` after the arm's own heap options, so an inherited
    // value would collapse both arms into one strategy. The harness never sets it; it is removed
    // from the child's environment as well, so a value introduced between the parent's check and
    // this spawn cannot survive either.
    command.env_remove("BEAM_RT_GC_STRATEGY");

    let load_before = loadavg();
    let mut child: Child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn the measurement child {}: {error}", exe.display()));
    let pid = child.id();

    let mut peak_vmhwm_kib = 0u64;
    let mut hwm_samples = 0usize;
    let deadline = Instant::now() + CHILD_DEADLINE;
    let status = loop {
        if let Some(hwm) = read_status_field(pid, "VmHWM:") {
            hwm_samples += 1;
            peak_vmhwm_kib = peak_vmhwm_kib.max(hwm);
        }
        match child.try_wait().expect("the child is waitable") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(VMHWM_POLL),
        }
    };
    let output = child
        .wait_with_output()
        .expect("the child's output is collectable");
    let load_after = loadavg();
    // Both streams: libtest may route `--nocapture` output of a passing test to either.
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let Some(status) = status else {
        panic!(
            "the {} / {} arm (round {}, N={}) did not terminate within {:?}\n--- transcript ---\n{}",
            job.class,
            job.arm.name(),
            job.round,
            job.n,
            CHILD_DEADLINE,
            transcript
        );
    };
    assert!(
        status.success(),
        "the {} / {} arm (round {}, N={}) failed ({status:?})\n--- transcript ---\n{}",
        job.class,
        job.arm.name(),
        job.round,
        job.n,
        transcript
    );
    // libtest writes `test <name> ... ` **without a newline** before the test's own output when the
    // test passes, so the row can share a line with that prefix: find the marker anywhere in the
    // line, not only at its start.
    let line = transcript
        .lines()
        .find_map(|line| line.find(ROW_PREFIX).map(|start| &line[start..]))
        .unwrap_or_else(|| {
            panic!(
                "the {} / {} arm (round {}, N={}) printed no `{ROW_PREFIX}` row\n\
                 --- transcript ---\n{transcript}",
                job.class,
                job.arm.name(),
                job.round,
                job.n
            )
        });
    let row = Row::from_line(line.trim());
    assert_eq!(
        row.class,
        job.class,
        "the child measured the class it was asked for"
    );
    assert_eq!(row.arm, job.arm, "the child ran the arm it was asked for");
    assert_eq!(row.n, job.n, "the child ran the N it was asked for");
    assert_eq!(
        row.round, job.round,
        "the child reported the round it was asked for"
    );
    ChildRun {
        row,
        peak_vmhwm_kib,
        hwm_samples,
        load_before,
        load_after,
    }
}

// ---------------------------------------------------------------------------------------------
// The parent: the per-class schedule, its assertions, and the estimator
// ---------------------------------------------------------------------------------------------

/// A parent-side override for every class's N, for iteration on a loaded host
/// (`BEAM_RT_GCH_OVERRIDE_N=2000`). Unset in a measured run: the calibrated constants apply.
const ENV_N_OVERRIDE: &str = "BEAM_RT_GCH_OVERRIDE_N";
/// A parent-side override for the number of rounds (`BEAM_RT_GCH_OVERRIDE_ROUNDS=1`). Unset in a
/// measured run: [`ROUNDS_RELEASE`] applies.
const ENV_ROUNDS_OVERRIDE: &str = "BEAM_RT_GCH_OVERRIDE_ROUNDS";

/// How many rounds this build measures: the release schedule, or the single debug round that keeps
/// the file inside its debug budget. Overridable for iteration.
fn rounds() -> usize {
    let base = if cfg!(debug_assertions) {
        ROUNDS_DEBUG
    } else {
        ROUNDS_RELEASE
    };
    match std::env::var(ENV_ROUNDS_OVERRIDE) {
        Ok(value) => {
            let rounds: usize = value
                .parse()
                .unwrap_or_else(|_| panic!("{ENV_ROUNDS_OVERRIDE}={value} is not an integer"));
            assert!(rounds > 0, "{ENV_ROUNDS_OVERRIDE} must be at least 1");
            rounds
        }
        Err(_) => base,
    }
}

/// The class's N for this build, or the overriding value.
fn n_for(class: &str) -> i64 {
    let base = match (class, cfg!(debug_assertions)) {
        (CLASS_SEND_RECV, false) => SEND_RECV_N_RELEASE,
        (CLASS_SEND_RECV, true) => SEND_RECV_N_DEBUG,
        (CLASS_ETS_COPY, false) => ETS_COPY_N_RELEASE,
        (CLASS_ETS_COPY, true) => ETS_COPY_N_DEBUG,
        (CLASS_COMPILE, false) => COMPILE_N_RELEASE,
        (CLASS_COMPILE, true) => COMPILE_N_DEBUG,
        (other, _) => panic!("no calibrated N for class `{other}`"),
    };
    match std::env::var(ENV_N_OVERRIDE) {
        Ok(value) => {
            let n: i64 = value
                .parse()
                .unwrap_or_else(|_| panic!("{ENV_N_OVERRIDE}={value} is not an integer"));
            assert!(n > 0, "{ENV_N_OVERRIDE} must be at least 1");
            n
        }
        Err(_) => base,
    }
}

/// One class's rounds: every round is one `(Copying, Immix)` pair, in that order.
struct ClassRun {
    /// The class measured.
    class: String,
    /// The class's N.
    n: i64,
    /// One pair per round: `(copying, immix)`.
    pairs: Vec<(ChildRun, ChildRun)>,
}

/// Prints one arm's row of the per-round table.
fn print_arm_row(round: usize, run: &ChildRun) {
    let row = &run.row;
    println!(
        "  round {round:<2} {:<8} kind={:<8} collections={:<5} major={:<4} own_collections={:<5} \
         own_major={:<4} words_copied={:<9} objects_copied={:<8} words_reclaimed={:<9} \
         mature_used={:<7} mature_capacity={:<7} mean_nursery={:<6} peak_nursery={:<6} \
         samples={:<5} slices={:<6} peak_kib={:<8} wall_ms={:>9.1} verified={}",
        row.arm.name(),
        kind_name(row.kind),
        row.collections,
        row.major,
        row.own_collections,
        row.own_major,
        row.words_copied,
        row.objects_copied,
        row.words_reclaimed,
        row.mature_used,
        row.mature_capacity,
        row.mean_nursery,
        row.peak_nursery,
        row.nursery_samples,
        row.slices,
        run.peak_vmhwm_kib,
        row.wall_ms,
        row.verified,
    );
}

/// Asserts one arm's own invariants: the arm was real, the value came back, GC ran, nothing failed.
fn assert_arm(class: &str, round: usize, run: &ChildRun) {
    let row = &run.row;
    let where_ = format!("class {class}, round {round}, arm {}", row.arm.name());
    assert_eq!(
        row.kind,
        row.arm.expected_kind(),
        "{where_}: the measured process's own collector must really run the arm's strategy"
    );
    assert!(
        row.automatic,
        "{where_}: the strategy must come from the automatic rule, not a pin"
    );
    assert_eq!(
        row.choice_words,
        row.arm.initial_mature_words(),
        "{where_}: the resolved MatureChoice must carry the arm's configured geometry"
    );
    assert!(row.verified, "{where_}: the workload's value must verify");
    assert!(
        row.collections > 0,
        "{where_}: the runtime's own schedule must show collections"
    );
    assert!(
        row.major > 0,
        "{where_}: the cadence (fullsweep_after={FULLSWEEP_AFTER}) must reach a fullsweep"
    );
    assert!(
        row.own_collections > 0,
        "{where_}: the measured process must drive its own heap collections"
    );
    assert!(
        row.own_major > 0,
        "{where_}: the measured process must reach its own fullsweep"
    );
    assert_eq!(row.errors, 0, "{where_}: no collection failed");
    assert_eq!(row.halted, 0, "{where_}: no process halted");
    assert_eq!(row.valve_trips, 0, "{where_}: no escape-store valve trip");
    assert!(!row.disabled, "{where_}: collection was never disabled");
    assert!(
        run.hwm_samples > 0,
        "{where_}: the parent must have sampled the child's VmHWM at least once"
    );
}

/// The young words each arm's own heap doors admitted — `collections × mean nursery capacity` —
/// printed for every round pair, with the ratio between the arms.
///
/// The plan's accounting identity would make this product **strategy-invariant** (both arms
/// allocate the same terms), and it is asserted on the classes listed in [`IDENTITY_ASSERTED`].
/// It is **not** an invariant of this runtime as measured: the nursery's sizing rule reads the
/// live set the *collector itself* reports, and the two strategies report it differently, so the
/// two arms converge on different nursery capacities and different collection counts.
///
/// The measured evidence (one release round per class, arms differ by one configured word):
///
/// | class | copying `collections × mean` | immix `collections × mean` | ratio |
/// |---|---|---|---|
/// | `send_recv` | 60 000 × 376 = 22 560 000 | 79 999 × 609 = 48 719 391 | 2.16× |
/// | `ets_copy` | 41 377 × 1 406 = 58 176 062 | 41 375 × 1 038 = 42 947 250 | 0.74× |
/// | `compile` | 452 × 43 543 = 19 681 436 | 349 × 52 973 = 18 487 577 | 0.94× |
///
/// The mechanism, both halves visible in the reported rows:
///
/// * **The nursery is re-sized from a strategy-dependent live set.** A fullsweep re-sizes the
///   nursery from the **whole-heap** live set (`beam_gc/src/nursery.rs::collapse_after_fullsweep`
///   → `HeapPolicy::young_after_fullsweep`), and the mature generation's contribution to that live
///   set is what the two spaces report differently: the mark-region space reports line-granular
///   occupancy (`mature_used` 1 216 words on both copy-bound shapes, 118 256 on the compile) where
///   the copying space reports the promoted object words (`84`/`228`/`26 920`). A different
///   whole-heap live set gives a different nursery target — 376 vs 609 words on `send_recv`,
///   1 406 vs 1 038 on `ets_copy`, 43 543 vs 52 973 on the compile.
/// * **The two collectors do not share a fullsweep cadence.** The copying collector runs the
///   policy's cadence exactly (`fullsweep_after = 2` ⇒ two minors then a fullsweep, one major in
///   three: 20 000/60 000 on `send_recv`); `GenImmixCollector` additionally promotes a due
///   collection to a major when its block space is tight
///   (`beam_gc/src/immix_collector.rs:340`, `fullsweep_due(..) || tight`), so on a shape that
///   promotes anything it fullsweeps roughly every other collection (1/2, and 40 000/80 000 → the
///   immix arm simply collected more often at a larger capacity).
///
/// A cross-arm product band would therefore be a number fitted to these three shapes. What *is*
/// asserted cross-arm on every class is the strategy, the value, the presence of GC and the
/// footprint sign; what is asserted cross-arm on the product is only that both arms' doors
/// admitted *some* words (a zero product means no nursery admission at all).
fn report_allocation_identity(class: &str, round: usize, copying: &Row, immix: &Row) {
    let copying_product = copying.allocation_product();
    let immix_product = immix.allocation_product();
    assert!(
        copying_product > 0 && immix_product > 0,
        "class {class}, round {round}: both arms must admit young words through the nursery \
         (copying {copying_product}, immix {immix_product})"
    );
    let ratio = immix_product as f64 / copying_product as f64;
    println!(
        "  round {round} allocation product: copying {} x {} = {} | immix {} x {} = {} | \
         ratio={ratio:.3}",
        copying.own_collections,
        copying.mean_nursery,
        copying_product,
        immix.own_collections,
        immix.mean_nursery,
        immix_product,
    );
    if IDENTITY_ASSERTED.contains(&class) {
        let (lo, hi) = if copying_product <= immix_product {
            (copying_product, immix_product)
        } else {
            (immix_product, copying_product)
        };
        assert!(
            lo * 100 >= hi * 99,
            "class {class}, round {round}: the allocation product must agree within 1% between \
             the arms (copying {} x {} = {}, immix {} x {} = {})",
            copying.own_collections,
            copying.mean_nursery,
            copying_product,
            immix.own_collections,
            immix.mean_nursery,
            immix_product,
        );
    }
}

/// The classes on which the allocation product is a genuine cross-arm invariant, i.e. on which the
/// two arms converge on the same nursery capacity and collection count.
///
/// **Empty, and that is a measurement result, not an omission**: the mechanism is documented on
/// [`report_allocation_identity`]. A future change that makes the nursery's live-set rule read a
/// strategy-independent figure moves its class back into this list, and the 1 % assertion above
/// becomes load-bearing again.
const IDENTITY_ASSERTED: &[&str] = &[];

/// Every metric this class's estimator pairs across the arms.
const PAIRED_METRICS: &[&str] = &[
    "wall_ms",
    "collections",
    "major",
    "own_collections",
    "own_major",
    "words_copied",
    "objects_copied",
    "words_reclaimed",
    "mature_used",
    "mature_capacity",
    "mean_nursery",
    "allocation_product",
    "peak_vmhwm_kib",
];

/// One metric's paired samples across a class's rounds.
struct Paired {
    /// The metric's name, as printed.
    name: &'static str,
    /// `(copying, immix)` per round.
    pairs: Vec<(f64, f64)>,
}

impl Paired {
    /// The metric's value for one measured arm.
    fn value(name: &str, run: &ChildRun) -> f64 {
        let row = &run.row;
        match name {
            "wall_ms" => row.wall_ms,
            "collections" => row.collections as f64,
            "major" => row.major as f64,
            "own_collections" => row.own_collections as f64,
            "own_major" => row.own_major as f64,
            "words_copied" => row.words_copied as f64,
            "objects_copied" => row.objects_copied as f64,
            "words_reclaimed" => row.words_reclaimed as f64,
            "mature_used" => row.mature_used as f64,
            "mature_capacity" => row.mature_capacity as f64,
            "mean_nursery" => row.mean_nursery as f64,
            "allocation_product" => row.allocation_product() as f64,
            "peak_vmhwm_kib" => run.peak_vmhwm_kib as f64,
            other => panic!("no paired accessor for metric `{other}`"),
        }
    }

    /// Every metric of `runs`, as paired samples.
    fn all(runs: &[(ChildRun, ChildRun)]) -> Vec<Paired> {
        PAIRED_METRICS
            .iter()
            .map(|name| Paired {
                name,
                pairs: runs
                    .iter()
                    .map(|(copying, immix)| {
                        (Paired::value(name, copying), Paired::value(name, immix))
                    })
                    .collect(),
            })
            .collect()
    }

    /// The median of `values` (the mean of the two middle samples for an even count).
    fn median(values: &mut [f64]) -> Option<f64> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a metric"));
        let mid = values.len() / 2;
        Some(if values.len() % 2 == 0 {
            (values[mid - 1] + values[mid]) / 2.0
        } else {
            values[mid]
        })
    }

    /// The median of the per-round differences (`Immix − Copying`).
    fn median_diff(&self) -> Option<f64> {
        let mut diffs: Vec<f64> = self.pairs.iter().map(|(c, i)| i - c).collect();
        Paired::median(&mut diffs)
    }

    /// The median of the per-round ratios (`Immix / Copying`), skipping a zero denominator.
    fn median_ratio(&self) -> Option<f64> {
        let mut ratios: Vec<f64> = self
            .pairs
            .iter()
            .filter(|(copying, _)| *copying != 0.0)
            .map(|(copying, immix)| immix / copying)
            .collect();
        Paired::median(&mut ratios)
    }

    /// The smallest and largest per-round difference.
    fn diff_range(&self) -> Option<(f64, f64)> {
        let mut diffs: Vec<f64> = self.pairs.iter().map(|(c, i)| i - c).collect();
        Paired::median(&mut diffs)?;
        Some((
            diffs.first().copied().unwrap_or(0.0),
            diffs.last().copied().unwrap_or(0.0),
        ))
    }

    /// The median of one arm's own samples.
    fn median_of(&self, arm_index: usize) -> Option<f64> {
        let mut values: Vec<f64> = self
            .pairs
            .iter()
            .map(|(copying, immix)| if arm_index == 0 { *copying } else { *immix })
            .collect();
        Paired::median(&mut values)
    }
}

/// Prints the estimator: the median paired difference and the median paired ratio per metric, with
/// the difference's observed range across the rounds.
fn print_estimator(run: &ClassRun) {
    println!(
        "  estimator (class={}, n={}, rounds={}; paired per round, Immix − Copying):",
        run.class,
        run.n,
        run.pairs.len()
    );
    println!(
        "    {:<20} {:>14} {:>14} {:>14} {:>12} {}",
        "metric", "copying(med)", "immix(med)", "median_diff", "med_ratio", "diff[min..max]"
    );
    for paired in Paired::all(&run.pairs) {
        let fmt = |value: Option<f64>| match value {
            Some(value) => format!("{value:.3}"),
            None => "n/a".to_string(),
        };
        let range = match paired.diff_range() {
            Some((min, max)) => format!("[{min:.3} .. {max:.3}]"),
            None => "n/a".to_string(),
        };
        println!(
            "    {:<20} {:>14} {:>14} {:>14} {:>12} {}",
            paired.name,
            fmt(paired.median_of(0)),
            fmt(paired.median_of(1)),
            fmt(paired.median_diff()),
            fmt(paired.median_ratio()),
            range,
        );
    }
}

/// **Measures one class: the arms strictly interleaved, one child per arm per round.**
///
/// `assert_footprint` is the plan's *"where the shape holds a live payload"* switch: the two
/// copy-bound shapes carry a live payload through both copy boundaries, so the mark-region space's
/// larger reported occupancy is asserted there; the compile class's live set is transient and only
/// reported.
fn measure_class(class: &str, assert_footprint: bool) {
    let _serialized = measurement_lock();
    refuse_ambient_strategy(&format!("gc_strategy_classes parent ({class})"));
    if !Path::new("/proc/self/status").exists() {
        support::skip_loudly(
            BANNER,
            "no procfs at /proc (the peak-VmHWM sampling reads /proc/<pid>/status)",
        );
        return;
    }
    let Some(_oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(erlc) = require_erlc() else {
        return;
    };
    let (module, _) = fixture_entry(class);
    let scratch = scratch_dir(class);
    let fixture = match class {
        CLASS_COMPILE => compile_fixture(&erlc, scratch.path(), module, COMPILE_SOURCE_ERL),
        _ => compile_fixture(&erlc, scratch.path(), module, COPY_BOUND_SOURCE),
    };

    // The compile class's inputs: the real source, and one output directory per compile (the
    // compiler does not create its `{outdir, …}`).
    let source = if class == CLASS_COMPILE {
        match compile_source() {
            Some(source) => Some(source),
            None => return,
        }
    } else {
        None
    };
    let n = n_for(class);
    let module_expected = source.as_ref().and_then(|path| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
    });
    let out_root = if class == CLASS_COMPILE {
        let root = scratch.path().join("compile_out");
        for index in 1..=n {
            let dir = root.join(format!("out{index}"));
            std::fs::create_dir_all(&dir).unwrap_or_else(|error| {
                panic!("cannot create the compile class's {}: {error}", dir.display())
            });
        }
        Some(root)
    } else {
        None
    };

    let rounds = rounds();
    println!();
    println!(
        "gc_strategy_classes: class={class} n={n} rounds={rounds} arms=copying/immix \
         fullsweep_after={FULLSWEEP_AFTER} mature_words={}/{} loadavg={}",
        Arm::Copying.initial_mature_words(),
        Arm::Immix.initial_mature_words(),
        loadavg(),
    );

    let mut measured: Vec<(ChildRun, ChildRun)> = Vec::with_capacity(rounds);
    for round in 1..=rounds {
        let mut arms: [Option<ChildRun>; 2] = [None, None];
        // Strictly interleaved: never arm-blocked, so a change in host load lands on both arms
        // alike.
        for (index, arm) in Arm::BOTH.into_iter().enumerate() {
            let run = run_child(&ChildJob {
                class,
                arm,
                n,
                round,
                fixture: &fixture,
                out_root: out_root.as_deref(),
                source: source.as_deref(),
                module_expected: module_expected.as_deref(),
            });
            print_arm_row(round, &run);
            arms[index] = Some(run);
        }
        let [Some(copying), Some(immix)] = arms else {
            panic!("class {class}, round {round}: both arms must have been measured");
        };
        println!(
            "  round {round} loadavg: before={} after={}",
            copying.load_before, immix.load_after
        );
        // The per-arm invariants, then the cross-arm ones for this pair.
        assert_arm(class, round, &copying);
        assert_arm(class, round, &immix);
        report_allocation_identity(class, round, &copying.row, &immix.row);
        if assert_footprint {
            assert!(
                immix.row.mature_used > copying.row.mature_used,
                "class {class}, round {round}: the mark-region space's reported occupancy must be \
                 strictly larger where the shape holds a live payload (copying {}, immix {})",
                copying.row.mature_used,
                immix.row.mature_used,
            );
        }
        measured.push((copying, immix));
    }

    let run = ClassRun {
        class: class.to_string(),
        n,
        pairs: measured,
    };
    print_estimator(&run);
    println!(
        "gc_strategy_classes: class={class} loadavg after the class: {}",
        loadavg()
    );
}

// ---------------------------------------------------------------------------------------------
// The parent's tests — one per class, plus the noise floor
// ---------------------------------------------------------------------------------------------

/// **C3-a — the messaging/copy-bound class's two-process ping-pong under both mature strategies.**
///
/// The measured process spawns a peer and does `N` round trips of a ~390-word payload, so every
/// round crosses the process copy boundary twice (out through `!`, back through `receive`) and the
/// allocation that dies at the next round is the class's pressure. Rounds and N: the module docs.
#[test]
fn c3_send_recv_under_both_mature_strategies() {
    measure_class(CLASS_SEND_RECV, true);
}

/// **C3-b — the messaging/copy-bound class's ETS copy shape under both mature strategies.**
///
/// One table, one key, a fresh payload inserted and copied back out per round: the work is the
/// table-store copy, never table growth.
#[test]
fn c3_ets_copy_under_both_mature_strategies() {
    measure_class(CLASS_ETS_COPY, true);
}

/// **C4 — a real in-process compile under both mature strategies.**
///
/// The measured process compiles one real `lib/stdlib/src/ordsets.erl` source `N` times with
/// `no_spawn_compiler_process`, so the compiler's own passes and code emission allocate on the heap
/// being measured; every produced artifact is re-loaded by the child and re-checked on disk by the
/// parent.
#[test]
fn c4_in_process_compile_under_both_mature_strategies() {
    measure_class(CLASS_COMPILE, true);
}

/// **The noise floor: one arm at a fixed N, repeated.**
///
/// The spread of the repeats is the band any claimed effect must beat. The workload is the
/// copy-bound ping-pong, whose N is the shortest of the calibrated ones, and the arm is the copying
/// one (the baseline the Immix figures are differenced against).
///
/// The spread is **printed only**: a shared host's noise is measured, never asserted (the one
/// assertion is the run's own completeness — every repeat returned and collected).
#[test]
fn the_noise_floor_of_one_arm_repeated() {
    let _serialized = measurement_lock();
    refuse_ambient_strategy("gc_strategy_classes noise floor");
    if !Path::new("/proc/self/status").exists() {
        support::skip_loudly(
            BANNER,
            "no procfs at /proc (the peak-VmHWM sampling reads /proc/<pid>/status)",
        );
        return;
    }
    let Some(_oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(erlc) = require_erlc() else {
        return;
    };
    let class = CLASS_SEND_RECV;
    let scratch = scratch_dir("noise");
    let fixture = compile_fixture(
        &erlc,
        scratch.path(),
        "gc_class_copy_bound",
        COPY_BOUND_SOURCE,
    );
    let n = n_for(class);
    let repeats = NOISE_REPEATS;    println!();
    println!(
        "gc_strategy_classes: noise floor class={class} arm={} n={n} repeats={repeats} \
         loadavg={}",
        Arm::Copying.name(),
        loadavg(),
    );
    let mut runs = Vec::with_capacity(repeats);
    for repeat in 1..=repeats {
        let run = run_child(&ChildJob {
            class,
            arm: Arm::Copying,
            n,
            round: repeat,
            fixture: &fixture,
            out_root: None,
            source: None,
            module_expected: None,
        });
        print_arm_row(repeat, &run);
        assert_arm(class, repeat, &run);
        runs.push(run);
    }
    for metric in ["wall_ms", "own_collections", "words_copied", "peak_vmhwm_kib"] {
        let mut values: Vec<f64> = runs
            .iter()
            .map(|run| Paired::value(metric, run))
            .collect();
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let median = Paired::median(&mut values).unwrap_or(f64::NAN);
        let ratio = if min > 0.0 { max / min } else { f64::NAN };
        println!(
            "  noise floor: {metric:<18} min={min:>12.3} median={median:>12.3} max={max:>12.3} \
             spread={:.3}x",
            ratio
        );
    }
    println!(
        "gc_strategy_classes: noise floor loadavg after the repeats: {}",
        loadavg()
    );
}
