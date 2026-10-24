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

//! **MEASURED — the barrier-free copying mature generation against the mark-region (`Immix`)
//! mature generation, measured per *workload class* at the collector level.**
//!
//! The sibling suite `../beam_rt/tests/gc_strategy_classes.rs` measures the two strategies on the
//! two classes that need a *runtime*: the messaging/copy-bound class (C3) and a real in-process
//! compile (C4), reported in `../beam_rt/GC_STRATEGY_MEASURED.md`.
//! This file is that measurement's **in-crate counterpart**: the same question asked of a bare
//! [`AnyCollector`](beam_gc::AnyCollector) — no runtime, no scheduler, no `erl`/`erlc`, no fixture
//! on disk — over the two classes a collector alone can express:
//!
//! | class | shape | what it stresses |
//! |---|---|---|
//! | `churn` (C1) | a **small bounded** live set under a large per-round volume of short-lived allocation | minor promotion into the mature generation, at a live set far below the churn |
//! | `growth` (C2) | a live set that **grows monotonically** (each round keeps its batch) | the mature generation's on-demand growth, at a live set far above the churn |
//! | `copy_out` (C3-shaped, optional) | a persistent payload structurally copied out once per round, nothing else | the `ets_copy` *allocation geometry* — a copy-out that dies immediately — with no ETS and no runtime |
//!
//! The class geometry lives in the `#[path]`-included `mature_strategy_sweep/fixtures.rs`, which
//! documents each shape and why its nursery is a fixed point of the policy's own sizing rules.
//!
//! # The two arms: one strategy apart, at identical geometry
//!
//! Both arms of a class are built **only** through
//! [`AnyCollector::from_config`](beam_gc::AnyCollector::from_config) with an **explicit pin** —
//! [`GcConfig::copying`](beam_gc::GcConfig::copying) or
//! [`GcConfig::immix`](beam_gc::GcConfig::immix) — over the **same**
//! [`HeapPolicy`](beam_gc::HeapPolicy), so `initial_young_words`, `initial_mature_words`,
//! `fullsweep_after` and every bound are identical for the two arms and the strategy is the *only*
//! difference.
//!
//! `GenImmixCollector::with_config` ignores `MatureKind`, so it is never used to build an arm: a
//! silent "both arms are copying" run is the failure this harness exists to make impossible, and
//! it is checked three ways — the live collector's
//! [`kind()`](beam_gc::AnyCollector::kind) must be the arm's `MatureKind`, the **config's**
//! [`mature_choice()`](beam_gc::GcConfig::mature_choice) must record `MatureSource::Explicit` with
//! that kind, and the child asserts both *inside the process that measured*.
//!
//! The config's choice is the authoritative pin record on purpose.
//! [`AnyCollector::mature_choice`] recovers the source by comparing the resolved kind with the
//! automatic rule's verdict (`beam_gc::selection::choice_for`), so an explicit pin of the rule's
//! *own* pick reports `Automatic` there — and every class configures a mature generation at or
//! above [`IMMIX_BREAK_EVEN_WORDS`](beam_gc::IMMIX_BREAK_EVEN_WORDS), which is exactly the rule's
//! `Immix` verdict. Both sources are therefore reported per row (`config_source` = the pin,
//! `live_source` = the rule-agreement view) and only the former is asserted.
//!
//! # The child-process architecture (why the measurement runs in a child)
//!
//! **The parent is only a driver.** One `#[test]` measures every class and the noise floor, and it
//! measures nothing itself: it runs its rounds with the arms **strictly interleaved**
//! (`Copying`, `Immix`, `Copying`, `Immix`, … — never arm-blocked, so a change in host load lands
//! on both arms alike), and every `(class, arm, round)` is measured by **one fresh child
//! process**: this same test binary re-exec'd with
//! `--exact <worker> --ignored --nocapture --test-threads=1` plus this harness's own environment
//! variables selecting class/arm/N/round ([`ENV_CLASS`] … [`ENV_ROUND`]).
//!
//! Why a child per arm:
//!
//! * a collector's [`GcStats`](beam_gc::GcStats) is **cumulative**, so a second arm in the same
//!   process could not start from zero;
//! * `VmHWM` in `/proc/self/status` is monotonic for the life of a process, so two arms in one
//!   process cannot each have a peak;
//! * a crash (a `beam_gc` assertion, an OOM) must be one *arm's* failure, not the suite's.
//!
//! The child builds its own `AnyCollector`, drives exactly one class for `N` rounds, prints **one
//! machine-readable row** (`ROW class=… arm=… …`) and exits. While it runs, the parent polls
//! `/proc/<child pid>/status` for `VmHWM:` every [`VMHWM_POLL`] and keeps the maximum — the
//! kernel's monotonic high-water mark (the pattern of `../beam_rt/tests/gc_strategy_classes.rs`
//! and, in this crate, `offheap_retention.rs` / `retained_vs_live.rs`) — and reads `/proc/loadavg`
//! **before and after every round**.
//! A nonzero child exit, a missing or unparsable row, or a child that overruns
//! [`CHILD_DEADLINE`] is a **hard failure**, never a skip.
//!
//! A child of one of these classes is **sub-millisecond**, so a 15 ms poll can only bracket its
//! start. The child reads its own `VmHWM` ([`Row::self_kb`]) — the same kernel counter, read
//! in-process (`offheap_retention.rs:132`) — but it reads it *before* it formats and prints its row
//! and before the test harness tears the process down, either of which can still fault a page in;
//! the parent's poll, by contrast, runs until the child is gone. The two figures are therefore the
//! **same monotonic quantity sampled at different instants**, and neither bounds the other: a
//! parent sample taken after the child's own read is legitimately *higher*, and a poll that
//! bracketed only the child's start is legitimately *lower*.
//! The **reported** per-arm peak RSS is the union of the two observations,
//! `peak_rss_kib = max(self_kb, polled peak)` ([`ChildRun::peak_rss_kib`]); that is the figure the
//! rows print and the estimator pairs, and only its *definition* is asserted (plus, when the parent
//! took at least one sample, that the sample is nonzero — a broken poll must not pass silently).
//! The honest caveat: at these `N`s a child's resident set is dominated by the test binary's own
//! image (~3.4–3.6 MB, identical for both arms) on the `churn` and `copy_out` classes, so there
//! the RSS column carries **no strategy signal**; on `growth` the heap is large enough for the
//! figure to be strategy-driven. The footprint axis for the small classes is
//! [`HeapFootprint`](beam_gc::HeapFootprint)'s reserved-capacity bytes, which is printed per row and
//! is exactly the two generations' words.
//! Without procfs the peak RSS is reported as `0`/`n/a` behind a loud warning; the deterministic
//! counters the assertions read are unaffected, so the sweep never degrades into a silent pass.
//!
//! There is no measurement lock: the sweep driver is this binary's **only** non-ignored test, so
//! two classes cannot measure at once inside it.
//!
//! # The estimator, and what the host's noise costs
//!
//! The host is shared and loaded, so **absolute wall times are not results**.
//! Each class's report pairs arm against arm *per round* and prints, per metric, the two arms'
//! medians, the **median paired difference** (`Immix − Copying`), the **median paired ratio**
//! (`Immix ÷ Copying`, skipping zero denominators) and the observed `[min .. max]` spread of the
//! per-round differences.
//! The noise floor then measures **one arm at one N three times** and prints the spread of its
//! peak RSS and wall time: that spread is the band any claimed effect must beat.
//!
//! # What is asserted (deterministic counts and identities only — never time, never memory)
//!
//! Per arm per round, all computed from the class's geometry and `N`:
//!
//! 1. **the arm was real** — `kind()` is the arm's `MatureKind` and the config's `mature_choice()`
//!    records `MatureSource::Explicit` with that kind;
//! 2. the driver drove exactly `N` collections and every one was accounted for —
//!    `minors + majors == collections == rounds == N`, and the same again from the collector's own
//!    cumulative `GcStats`;
//! 3. **there really was GC**, in both arms and every class — `words_copied > 0`,
//!    `objects_copied > 0`, `mature_peak_words > 0`;
//! 4. **the footprint is an exact reserved-capacity account** —
//!    `capacity_bytes() == (young_capacity + mature_capacity) × WORD_BYTES` over an **empty**
//!    off-heap roster (`off_heap_entries == 0`), and `capacity_bytes() >= used_bytes()`;
//! 5. the mature generation really holds the live set — `mature_capacity >= live_words` and
//!    `used_bytes() >= live_words × WORD_BYTES`;
//! 6. **the live set is intact** under both arms — `intact` is the child's
//!    `beam_term::compare`-equal check of every root against an independently built witness in an
//!    arena the collector never saw, and its `Σ size_object` footprint is the frozen-layout formula
//!    (`roots × (arity + 1)`), which the child asserts and the parent re-derives for the growth
//!    class, whose live set is `N × GROWTH_KEEP` tuples by construction;
//! 7. **the cadence is accounted for** — `majors >= collections ÷ (fullsweep_after + 1)` in both
//!    arms (the due-fullsweep rule's floor), and for the Copying arm on the classes whose live set
//!    cannot outgrow one mature generation, `majors × (fullsweep_after + 1) − collections <=
//!    fullsweep_after`, i.e. the cadence is exact to within one fullsweep cycle. The growth class
//!    and the Immix arm assert only the floor: both may **escalate** a due minor into a fullsweep
//!    (the copying collector when the live promotion does not fit the mature space, the mark-region
//!    one when the block space is tight), so neither cadence is exact;
//! 8. **the two arms are comparable** — same `N`, same `rounds`, same `collections`, same live-set
//!    cardinality and footprint;
//! 9. **the noise floor's deterministic counters are bit-identical** across its three repeats
//!    (whole-row equality over every counter, so no `1.000x` claim rests on rounding).
//!
//! Everything else — wall time, peak RSS, `words_copied`, `objects_copied`, occupancy, capacity —
//! is **printed only**: those are the measurements, not invariants. The peak RSS column has one
//! assertion of its own and it is only the *definition* of the composite (see the RSS paragraph
//! above) plus "a poll that sampled the child observed something"; no RSS figure or ratio is ever
//! bounded, compared or trended.
//!
//! `N` is **scaled on `cfg!(debug_assertions)`** ([`N_RELEASE`] / [`N_DEBUG`]) so the file fits its
//! budget in both profiles (≤ ~10 s release, ≤ ~60 s debug); every assertion is a function of the
//! geometry and `N`, so it holds at either size.
//!
//! # Reproduce
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --release -p beam_gc \
//!     --test mature_strategy_sweep -- --nocapture --test-threads=1
//! ```
//!
//! Environment: `BEAM_GC_SWEEP_ROUNDS` raises the per-class round count above its floor of
//! [`MIN_ROUNDS`], `BEAM_GC_SWEEP_N` overrides every class's `N`.

#[path = "mature_strategy_sweep/fixtures.rs"]
mod fixtures;

use beam_gc::prelude::{MatureKind, MatureSource, WORD_BYTES};
use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fixtures::{Tally, CLASSES, COPY_OUT, FULLSWEEP_AFTER, GROWTH, GROWTH_KEEP, GROWTH_KEEP_WORDS};

// ---------------------------------------------------------------------------------------------
// The knob scales (see the module docs: every assertion is a function of geometry and N)
// ---------------------------------------------------------------------------------------------

/// `N` — the rounds a child drives, i.e. the collections it performs — per class in a **release**
/// build.
///
/// One round is one collection by construction (see `fixtures::run`), so `N` *is* the class's
/// collection count: the cadence assertions and the live-set formula are both functions of it.
const N_RELEASE: [(&str, usize); 3] = [(fixtures::CHURN, 2048), (GROWTH, 1024), (COPY_OUT, 2048)];
/// The same, in a **debug** build: a quarter of the release size.
///
/// The scaling is what keeps the file inside its both-profile budget (≤ ~10 s release, ≤ ~60 s
/// debug); **nothing else depends on it** — every assertion and every class's live-set formula is
/// a function of `N` and the geometry, so both sizes are checked identically.
const N_DEBUG: [(&str, usize); 3] = [(fixtures::CHURN, 512), (GROWTH, 256), (COPY_OUT, 512)];

/// The per-class round floor: below three paired rounds a "median" is not an estimator.
const MIN_ROUNDS: usize = 3;
/// The round count a measured run uses (both profiles — the driver's work is tiny; the child
/// processes, not the collector, set the file's wall time).
const ROUNDS_DEFAULT: usize = MIN_ROUNDS;
/// How many times the noise floor repeats its one arm at its one `N`.
const NOISE_REPEATS: usize = 3;

/// How long one child may run before the parent kills it and fails the sweep.
const CHILD_DEADLINE: Duration = Duration::from_secs(900);
/// How often the parent samples a child's `VmHWM`.
const VMHWM_POLL: Duration = Duration::from_millis(15);

// ---------------------------------------------------------------------------------------------
// The child protocol
// ---------------------------------------------------------------------------------------------

/// The name of the `#[ignore]`d worker the parent re-execs.
const WORKER: &str = "child_runs_one_arm_and_prints_a_row";
/// The prefix of the one machine-readable row a child prints.
const ROW_PREFIX: &str = "ROW ";

/// The class a child measures.
const ENV_CLASS: &str = "BEAM_GC_SWEEP_CLASS";
/// The arm a child measures.
const ENV_ARM: &str = "BEAM_GC_SWEEP_ARM";
/// The rounds (`N`) a child drives.
const ENV_N: &str = "BEAM_GC_SWEEP_N";
/// Which round of the parent's interleaved schedule the child's row belongs to.
const ENV_ROUND: &str = "BEAM_GC_SWEEP_ROUND";
/// A parent-side override that may only **raise** the round count above [`MIN_ROUNDS`].
const ENV_ROUNDS_OVERRIDE: &str = "BEAM_GC_SWEEP_ROUNDS";
/// A parent-side override for every class's `N`, for iteration on a loaded host.
const ENV_N_OVERRIDE: &str = "BEAM_GC_SWEEP_N";

// ---------------------------------------------------------------------------------------------
// The arms
// ---------------------------------------------------------------------------------------------

/// One arm of the A/B: the barrier-free copying mature generation, or the mark-region (`Immix`)
/// one.
///
/// The arm *is* its [`MatureKind`]: the class's [`HeapPolicy`](beam_gc::HeapPolicy) is shared by
/// both arms, so the strategy is the only difference between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// The copying mature generation, explicitly pinned.
    Copying,
    /// The mark-region mature generation, explicitly pinned.
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

    /// The [`MatureKind`] this arm pins — and therefore the one the collector must report.
    fn expected_kind(self) -> MatureKind {
        match self {
            Arm::Copying => MatureKind::Copying,
            Arm::Immix => MatureKind::Immix,
        }
    }

    /// This arm's configuration of `class`: the class's policy with this arm's kind pinned.
    fn config(self, class: &str) -> beam_gc::GcConfig {
        fixtures::config(class, self.expected_kind())
    }
}

/// The [`MatureKind`]'s row spelling (kept stable against a `#[derive]` change).
fn kind_name(kind: MatureKind) -> &'static str {
    match kind {
        MatureKind::Copying => "Copying",
        MatureKind::Immix => "Immix",
    }
}

/// The [`MatureSource`]'s row spelling.
fn source_name(source: MatureSource) -> &'static str {
    match source {
        MatureSource::Automatic => "Automatic",
        MatureSource::Explicit => "Explicit",
    }
}

// ---------------------------------------------------------------------------------------------
// The child's one row
// ---------------------------------------------------------------------------------------------

/// Everything one measured arm reports: the collector's counters, the live-set verification, the
/// geometry the footprint accounts, the strategy that was really in effect, and the two figures
/// that are printed but never asserted (wall time, and the child's own `VmHWM` — one half of the
/// run's reported peak RSS).
///
/// The child prints it as one `ROW key=value …` line; the parent parses those keys back into this
/// struct and joins the figure it observed itself (its polled peak `VmHWM`) to make a
/// [`ChildRun`], whose `peak_rss_kib` is the larger of the two.
#[derive(Debug, Clone)]
struct Row {
    /// The class this arm measured.
    class: String,
    /// The arm.
    arm: Arm,
    /// Which round of the parent's schedule this was.
    round: usize,
    /// The class's `N` for this run.
    n: usize,
    /// The strategy the measured collector **really** ran
    /// ([`AnyCollector::kind`](beam_gc::AnyCollector::kind)).
    kind: MatureKind,
    /// The strategy the **configuration** resolved to (the explicit pin's record).
    config_kind: MatureKind,
    /// Whether the configuration recorded the strategy as pinned or rule-derived.
    config_source: MatureSource,
    /// The live collector's own rule-agreement view
    /// ([`AnyCollector::mature_choice`](beam_gc::AnyCollector::mature_choice)) — `Automatic` for a
    /// pin of the rule's own pick, which is why it is reported and not asserted.
    live_source: MatureSource,
    /// Rounds the child drove.
    rounds: usize,
    /// Collections the driver counted (`minors + majors`).
    collections: usize,
    /// Minor collections.
    minors: u64,
    /// Fullsweeps.
    majors: u64,
    /// The whole run's collections **as the collector's own cumulative `GcStats` reports them** —
    /// the authority the driver's `minors + majors` is checked against, so no collection can
    /// appear from nowhere.
    reported_collections: u64,
    /// `GcStats::words_copied`, summed over the per-collection stats.
    words_copied: usize,
    /// `GcStats::objects_copied`, summed the same way.
    objects_copied: usize,
    /// `GcStats::words_reclaimed`, summed the same way.
    words_reclaimed: usize,
    /// Peak mature occupancy **after** a collection, in the strategy's own unit.
    mature_peak_words: usize,
    /// The same peak in whole mark-region lines (`0` for the copying arm).
    mature_peak_lines: usize,
    /// The unit [`Row::mature_peak_words`] is in: `words` (exact) or `line-words` (line-rounded).
    mature_unit: String,
    /// The mature generation's capacity once the run ended.
    mature_capacity: usize,
    /// The nursery **chain-summed** capacity once the run ended.
    young_capacity: usize,
    /// The **current** nursery space's capacity once the run ended.
    young_current_capacity: usize,
    /// Peak nursery occupancy, sampled before each collection.
    young_peak_used: usize,
    /// [`HeapFootprint::capacity_bytes`](beam_gc::HeapFootprint::capacity_bytes) over an empty
    /// off-heap roster.
    footprint_capacity_bytes: usize,
    /// [`HeapFootprint::used_bytes`](beam_gc::HeapFootprint::used_bytes), likewise.
    footprint_used_bytes: usize,
    /// The off-heap roster's live entries (must be `0`: the sweep installs an empty roster).
    footprint_entries: usize,
    /// How many roots the live set ended with.
    live_roots: usize,
    /// Its independently measured frozen-layout footprint (`Σ size_object`).
    live_words: usize,
    /// Whether every live root matched its independently built witness.
    intact: bool,
    /// The child's measured wall time in milliseconds — printed, never asserted.
    wall_ms: f64,
    /// The child's own `VmHWM`, in kB, read in-process *before* it prints its row and exits.
    ///
    /// One half of the reported peak: [`ChildRun::peak_rss_kib`] is the larger of this and the
    /// parent's polled samples (see the module docs — the two are the same monotonic counter read at
    /// different instants, so neither bounds the other).
    self_kb: u64,
}

impl Row {
    /// Builds the row of one measured arm from its [`Tally`] and its two choice views.
    fn from_tally(
        class: &str,
        arm: Arm,
        round: usize,
        n: usize,
        config_choice: beam_gc::MatureChoice,
        live_choice: beam_gc::MatureChoice,
        tally: Tally,
        wall_ms: f64,
        self_kb: u64,
    ) -> Row {
        Row {
            class: class.to_string(),
            arm,
            round,
            n,
            kind: arm.expected_kind(),
            config_kind: config_choice.kind,
            config_source: config_choice.source,
            live_source: live_choice.source,
            rounds: tally.rounds,
            collections: tally.collections,
            minors: tally.minors,
            majors: tally.majors,
            reported_collections: tally.reported_collections(),
            words_copied: tally.copied_words,
            objects_copied: tally.copied_objects,
            words_reclaimed: tally.reclaimed_words,
            mature_peak_words: tally.mature_peak_words,
            mature_peak_lines: tally.mature_peak_lines,
            mature_unit: tally.mature_unit().to_string(),
            mature_capacity: tally.mature_capacity_words,
            young_capacity: tally.young_capacity_words,
            young_current_capacity: tally.young_current_capacity_words,
            young_peak_used: tally.young_peak_used_words,
            footprint_capacity_bytes: tally.footprint.capacity_bytes(),
            footprint_used_bytes: tally.footprint.used_bytes(),
            footprint_entries: tally.footprint.off_heap_entries,
            live_roots: tally.live_roots,
            live_words: tally.live_words,
            intact: tally.intact,
            wall_ms,
            self_kb,
        }
    }

    /// The row's one machine-readable line.
    fn to_line(&self) -> String {
        format!(
            "{ROW_PREFIX}class={} arm={} round={} n={} kind={} config_kind={} config_source={} \
             live_source={} rounds={} collections={} reported_collections={} minors={} majors={} \
             words_copied={} \
             objects_copied={} words_reclaimed={} mature_peak_words={} mature_peak_lines={} \
             mature_unit={} mature_capacity={} young_capacity={} young_current_capacity={} \
             young_peak_used={} footprint_capacity_bytes={} footprint_used_bytes={} \
             footprint_entries={} live_roots={} live_words={} intact={} wall_ms={:.3} self_kb={}",
            self.class,
            self.arm.name(),
            self.round,
            self.n,
            kind_name(self.kind),
            kind_name(self.config_kind),
            source_name(self.config_source),
            source_name(self.live_source),
            self.rounds,
            self.collections,
            self.reported_collections,
            self.minors,
            self.majors,
            self.words_copied,
            self.objects_copied,
            self.words_reclaimed,
            self.mature_peak_words,
            self.mature_peak_lines,
            self.mature_unit,
            self.mature_capacity,
            self.young_capacity,
            self.young_current_capacity,
            self.young_peak_used,
            self.footprint_capacity_bytes,
            self.footprint_used_bytes,
            self.footprint_entries,
            self.live_roots,
            self.live_words,
            self.intact,
            self.wall_ms,
            self.self_kb,
        )
    }

    /// Parses a child's `ROW …` line back into a row, naming `line` in every failure.
    fn from_line(line: &str) -> Row {
        let fields = Fields::parse(line);
        let kind = parse_kind(fields.text("kind"), line);
        let arm = Arm::from_name(fields.text("arm"))
            .unwrap_or_else(|| panic!("`arm={}` is not an arm in: {line}", fields.text("arm")));
        Row {
            class: fields.text("class").to_string(),
            arm,
            round: fields.uint("round") as usize,
            n: fields.uint("n") as usize,
            kind,
            config_kind: parse_kind(fields.text("config_kind"), line),
            config_source: parse_source(fields.text("config_source"), line),
            live_source: parse_source(fields.text("live_source"), line),
            rounds: fields.uint("rounds") as usize,
            collections: fields.uint("collections") as usize,
            reported_collections: fields.uint("reported_collections"),
            minors: fields.uint("minors"),
            majors: fields.uint("majors"),
            words_copied: fields.uint("words_copied") as usize,
            objects_copied: fields.uint("objects_copied") as usize,
            words_reclaimed: fields.uint("words_reclaimed") as usize,
            mature_peak_words: fields.uint("mature_peak_words") as usize,
            mature_peak_lines: fields.uint("mature_peak_lines") as usize,
            mature_unit: fields.text("mature_unit").to_string(),
            mature_capacity: fields.uint("mature_capacity") as usize,
            young_capacity: fields.uint("young_capacity") as usize,
            young_current_capacity: fields.uint("young_current_capacity") as usize,
            young_peak_used: fields.uint("young_peak_used") as usize,
            footprint_capacity_bytes: fields.uint("footprint_capacity_bytes") as usize,
            footprint_used_bytes: fields.uint("footprint_used_bytes") as usize,
            footprint_entries: fields.uint("footprint_entries") as usize,
            live_roots: fields.uint("live_roots") as usize,
            live_words: fields.uint("live_words") as usize,
            intact: fields.flag("intact"),
            wall_ms: fields.number("wall_ms"),
            self_kb: fields.uint("self_kb"),
        }
    }

    /// Every **deterministic** counter of the row, in report order — the set the noise floor
    /// requires to be bit-identical across its repeats, and the set a re-derivation compares.
    ///
    /// `wall_ms`, `self_kb` and the parent's polled peak are deliberately absent: they are the
    /// measurements, not the invariants.
    fn deterministic_fields(&self) -> Vec<(&'static str, u128)> {
        vec![
            ("n", self.n as u128),
            ("rounds", self.rounds as u128),
            ("collections", self.collections as u128),
            (
                "reported_collections",
                u128::from(self.reported_collections),
            ),
            ("minors", u128::from(self.minors)),
            ("majors", u128::from(self.majors)),
            ("words_copied", self.words_copied as u128),
            ("objects_copied", self.objects_copied as u128),
            ("words_reclaimed", self.words_reclaimed as u128),
            ("mature_peak_words", self.mature_peak_words as u128),
            ("mature_peak_lines", self.mature_peak_lines as u128),
            ("mature_capacity", self.mature_capacity as u128),
            ("young_capacity", self.young_capacity as u128),
            (
                "young_current_capacity",
                self.young_current_capacity as u128,
            ),
            ("young_peak_used", self.young_peak_used as u128),
            (
                "footprint_capacity_bytes",
                self.footprint_capacity_bytes as u128,
            ),
            ("footprint_used_bytes", self.footprint_used_bytes as u128),
            ("footprint_entries", self.footprint_entries as u128),
            ("live_roots", self.live_roots as u128),
            ("live_words", self.live_words as u128),
            ("intact", u128::from(self.intact)),
        ]
    }
}

/// Parses a [`MatureKind`] field, naming `line` on failure.
fn parse_kind(text: &str, line: &str) -> MatureKind {
    match text {
        "Copying" => MatureKind::Copying,
        "Immix" => MatureKind::Immix,
        other => panic!("`{other}` is not a MatureKind in: {line}"),
    }
}

/// Parses a [`MatureSource`] field, naming `line` on failure.
fn parse_source(text: &str, line: &str) -> MatureSource {
    match text {
        "Automatic" => MatureSource::Automatic,
        "Explicit" => MatureSource::Explicit,
        other => panic!("`{other}` is not a MatureSource in: {line}"),
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

/// The worker the parent re-execs — **one arm of one class in a fresh process**, printing one
/// `ROW …` line. Never run by hand: the parent supplies the class/arm/N/round through the
/// environment, and a missing variable is a hard failure rather than a default.
#[test]
#[ignore = "worker: re-exec'd by the sweep driver, one arm per fresh process"]
fn child_runs_one_arm_and_prints_a_row() {
    let row = child_measure();
    println!("{}", row.to_line());
}

/// The child's whole measurement.
fn child_measure() -> Row {
    let class = env_value(ENV_CLASS);
    let class = canonical_class(&class);
    let arm_name = env_value(ENV_ARM);
    let arm =
        Arm::from_name(&arm_name).unwrap_or_else(|| panic!("{ENV_ARM}={arm_name} is not an arm"));
    let n = parse_env_usize(ENV_N);
    let round = parse_env_usize(ENV_ROUND);

    let config = arm.config(class);
    let config_choice = config.mature_choice();
    // The pin is the experiment. A configuration that did not record an explicit pin of this
    // arm's kind is not the arm it says it is, and the run must not be reported.
    assert_eq!(
        config_choice.kind,
        arm.expected_kind(),
        "{} / {}: the configuration must resolve to the arm's own strategy",
        class,
        arm.name()
    );
    assert_eq!(
        config_choice.source,
        MatureSource::Explicit,
        "{} / {}: the configuration must record an explicit pin, not a rule-derived choice",
        class,
        arm.name()
    );

    let mut collector = beam_gc::AnyCollector::from_config(&config).unwrap_or_else(|error| {
        panic!(
            "{} / {}: the arm's collector cannot be built: {error:?}",
            class,
            arm.name()
        )
    });
    // The pin really took effect: the live collector runs the arm's strategy.
    assert_eq!(
        collector.kind(),
        arm.expected_kind(),
        "{} / {}: the live collector must run the arm's strategy, not another",
        class,
        arm.name()
    );
    let live_choice = collector.mature_choice();

    let started = Instant::now();
    let tally = fixtures::run(class, &mut collector, n);
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let self_kb = read_status_field(std::process::id(), "VmHWM:").unwrap_or(0);

    Row::from_tally(
        class,
        arm,
        round,
        n,
        config_choice,
        live_choice,
        tally,
        wall_ms,
        self_kb,
    )
}

/// The class named `name` as the `&'static str` the fixtures take, or a panic naming it.
fn canonical_class(name: &str) -> &'static str {
    if !fixtures::is_known_class(name) {
        panic!("class `{name}` is not one of this sweep's classes ({CLASSES:?})");
    }
    CLASSES
        .iter()
        .copied()
        .find(|class| *class == name)
        .expect("`is_known_class` listed it")
}

/// A required environment variable of the child protocol, or a panic naming it.
fn env_value(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        panic!("{key} is unset: this worker is only ever re-exec'd by the sweep driver")
    })
}

/// A required environment variable of the child protocol, parsed as a positive integer.
fn parse_env_usize(key: &str) -> usize {
    let text = env_value(key);
    let value: usize = text
        .parse()
        .unwrap_or_else(|_| panic!("{key}={text} is not an integer"));
    assert!(value > 0, "{key} must be at least 1");
    value
}

// ---------------------------------------------------------------------------------------------
// The parent: spawning, polling, parsing
// ---------------------------------------------------------------------------------------------

/// One child run: its row, the parent's own observation of its peak, and the host load.
struct ChildRun {
    /// The child's one machine-readable row.
    row: Row,
    /// The largest `VmHWM` (kB) sampled while the child lived — the kernel's monotonic watermark.
    ///
    /// The child of a class like these is **sub-millisecond**, so a polled sample set can bracket
    /// only its start and is often well below the child's own reading ([`Row::self_kb`]) — but the
    /// poll runs for as long as the child lives, so a sample taken after the child's own read is
    /// legitimately *higher*. Neither figure bounds the other (module docs).
    peak_kib: u64,
    /// The **reported** peak RSS of the arm, in kB: the union of the two observations,
    /// `max(row.self_kb, peak_kib)`.
    ///
    /// Computed here, where the run is assembled, so every consumer (the printed rows, the
    /// estimator, the noise floor) reads one figure and cannot accidentally re-derive a narrower
    /// one. It is a measurement, never an invariant: no assertion compares it with another arm's.
    peak_rss_kib: u64,
    /// How many `/proc/<pid>/status` samples contributed (`0` ⇒ no procfs, or the run was missed).
    hwm_samples: usize,
    /// `/proc/loadavg` immediately before the child was spawned.
    load_before: String,
    /// `/proc/loadavg` immediately after the child was reaped.
    load_after: String,
}

/// One unit of work: which class, which arm, which `N`, which round.
struct ChildJob<'a> {
    /// The class name (already validated).
    class: &'a str,
    /// The arm.
    arm: Arm,
    /// The class's `N`.
    n: usize,
    /// The round this measurement belongs to (reporting and pairing only).
    round: usize,
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

/// Whether this host can be polled for a child's peak `VmHWM`.
fn has_procfs() -> bool {
    std::path::Path::new("/proc/self/status").exists()
}

/// Spawns **one** child to measure one arm, polls its `VmHWM`, parses its row and checks that it
/// exited cleanly.
///
/// The child is this same test binary re-exec'd as the only test in its process
/// (`--exact <WORKER> --ignored --nocapture --test-threads 1`), with [`ENV_CLASS`] … selecting the
/// work. It is **polled** rather than waited on, so a hung arm is a loud failure instead of a hung
/// suite. A nonzero child exit, a missing `ROW` line or an overrun of [`CHILD_DEADLINE`] is a hard
/// failure — never a skip.
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
        .env(ENV_ROUND, job.round.to_string());
    let load_before = loadavg();
    let mut child: Child = command.spawn().unwrap_or_else(|error| {
        panic!(
            "cannot spawn the measurement child {}: {error}",
            exe.display()
        )
    });
    let pid = child.id();
    let mut peak_kib = 0u64;
    let mut hwm_samples = 0usize;
    let deadline = Instant::now() + CHILD_DEADLINE;
    let status = loop {
        if let Some(hwm) = read_status_field(pid, "VmHWM:") {
            hwm_samples += 1;
            peak_kib = peak_kib.max(hwm);
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
            "the {} / {} arm (round {}, N={}) did not terminate within {CHILD_DEADLINE:?}\n\
             --- transcript ---\n{transcript}",
            job.class,
            job.arm.name(),
            job.round,
            job.n
        );
    };
    assert!(
        status.success(),
        "the {} / {} arm (round {}, N={}) failed ({status:?})\n--- transcript ---\n{transcript}",
        job.class,
        job.arm.name(),
        job.round,
        job.n
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
        row.class, job.class,
        "the child measured the class it was asked for"
    );
    assert_eq!(row.arm, job.arm, "the child ran the arm it was asked for");
    assert_eq!(row.n, job.n, "the child ran the N it was asked for");
    assert_eq!(
        row.round, job.round,
        "the child reported the round it was asked for"
    );
    ChildRun {
        peak_rss_kib: row.self_kb.max(peak_kib),
        row,
        peak_kib,
        hwm_samples,
        load_before,
        load_after,
    }
}

// ---------------------------------------------------------------------------------------------
// The parent's schedule knobs
// ---------------------------------------------------------------------------------------------

/// How many rounds each class measures: [`ROUNDS_DEFAULT`], raisable by
/// [`ENV_ROUNDS_OVERRIDE`] and never below [`MIN_ROUNDS`] (the protocol's estimator floor).
fn rounds() -> usize {
    let value = match std::env::var(ENV_ROUNDS_OVERRIDE) {
        Ok(text) => text
            .parse()
            .unwrap_or_else(|_| panic!("{ENV_ROUNDS_OVERRIDE}={text} is not an integer")),
        Err(_) => ROUNDS_DEFAULT,
    };
    assert!(
        value >= MIN_ROUNDS,
        "{ENV_ROUNDS_OVERRIDE}={value} is below the protocol's floor of {MIN_ROUNDS} paired rounds"
    );
    value
}

/// The class's `N` for this build, or the overriding value.
fn n_for(class: &str) -> usize {
    let base = if cfg!(debug_assertions) {
        N_DEBUG
    } else {
        N_RELEASE
    };
    let default = base
        .iter()
        .find_map(|(name, n)| (*name == class).then_some(*n))
        .unwrap_or_else(|| panic!("no calibrated N for class `{class}`"));
    match std::env::var(ENV_N_OVERRIDE) {
        Ok(text) => {
            let n: usize = text
                .parse()
                .unwrap_or_else(|_| panic!("{ENV_N_OVERRIDE}={text} is not an integer"));
            assert!(n > 0, "{ENV_N_OVERRIDE} must be at least 1");
            n
        }
        Err(_) => default,
    }
}

// ---------------------------------------------------------------------------------------------
// The parent's per-arm assertions
// ---------------------------------------------------------------------------------------------

/// Whether `class`'s live set can outgrow one mature generation between two fullsweeps.
///
/// The growth class keeps every batch it allocates, so its live promotion eventually does not fit
/// the mature space the last fullsweep sized — the copying collector then **escalates** a due minor
/// into a fullsweep (`Collector::collect`'s `minor_promotion_fits` arm), and its cadence is
/// consequently not exact. The churn and copy-out classes have bounded, tiny live sets (a few
/// hundred words against a mature generation of [`fixtures::CHURN_MATURE_WORDS`]), so nothing but
/// the due rule can produce one of their fullsweeps.
fn cadence_is_exact(class: &str) -> bool {
    class != GROWTH
}

/// Asserts one arm's own deterministic invariants — everything that must hold whatever the
/// strategy costs. Nothing here is a measurement.
fn assert_arm(class: &str, round: usize, run: &ChildRun) {
    let row = &run.row;
    let where_ = format!("class {class}, round {round}, arm {}", row.arm.name());

    // 0. The reported peak RSS is the *union* of the two observations, and a poll that did sample
    //    the child observed something. There is deliberately no ordering invariant between them:
    //    the child reads its own `VmHWM` before it prints and exits while the parent keeps
    //    sampling until the child is gone, so either figure may be the larger (module docs).
    if run.hwm_samples > 0 {
        assert!(
            run.peak_kib > 0,
            "{where_}: the parent took {} VmHWM samples of the child and every one read 0 kB — the \
             poll is broken",
            run.hwm_samples
        );
    }
    assert_eq!(
        run.peak_rss_kib,
        row.self_kb.max(run.peak_kib),
        "{where_}: the reported peak RSS must be the union of the child's own VmHWM and the \
         parent's polled peak, not one arm's figure"
    );
    // 1. The arm was real, and its pin was recorded.
    assert_eq!(
        row.kind,
        row.arm.expected_kind(),
        "{where_}: the measured collector must really run the arm's strategy"
    );
    assert_eq!(
        row.config_source,
        MatureSource::Explicit,
        "{where_}: the configuration must record an explicit pin"
    );
    assert_eq!(
        row.config_kind,
        row.arm.expected_kind(),
        "{where_}: the recorded pin must be the arm's own strategy"
    );

    // 2. The driver drove exactly N collections, and every one was accounted for.
    assert_eq!(row.n, row.rounds, "{where_}: the child drove N rounds");
    assert_eq!(
        row.collections, row.rounds,
        "{where_}: one collection per round"
    );
    assert_eq!(
        row.minors + row.majors,
        row.collections as u64,
        "{where_}: every collection is either a minor or a fullsweep"
    );
    assert_eq!(
        row.reported_collections, row.collections as u64,
        "{where_}: the collector's own cumulative GcStats must report exactly the collections the \
         driver drove — no collection appears from nowhere"
    );
    assert_eq!(
        row.minors + row.majors,
        row.reported_collections,
        "{where_}: the driver's tally and the collector's cumulative counters must agree"
    );

    // 3. There really was GC, in the mature generation the arm exists to exercise.
    assert!(
        row.words_copied > 0 && row.objects_copied > 0,
        "{where_}: a measured arm must have copied something (words {}, objects {})",
        row.words_copied,
        row.objects_copied
    );
    assert!(
        row.mature_peak_words > 0,
        "{where_}: the mature generation must have been occupied at some point"
    );

    // 4. The footprint is an exact reserved-capacity account over an empty off-heap roster.
    assert_eq!(
        row.footprint_entries, 0,
        "{where_}: the sweep installs an empty off-heap roster"
    );
    assert_eq!(
        row.footprint_capacity_bytes,
        (row.young_capacity + row.mature_capacity) * WORD_BYTES,
        "{where_}: the footprint's capacity must be exactly the two generations' words in bytes"
    );
    assert!(
        row.footprint_capacity_bytes >= row.footprint_used_bytes,
        "{where_}: reserved capacity can never be below the occupancy it holds ({} < {})",
        row.footprint_capacity_bytes,
        row.footprint_used_bytes
    );

    // 5. The mature generation really holds the live set.
    assert!(
        row.mature_capacity >= row.live_words,
        "{where_}: the mature generation must be able to hold the whole live set \
         (capacity {} < live {} words)",
        row.mature_capacity,
        row.live_words
    );
    assert!(
        row.footprint_used_bytes >= row.live_words * WORD_BYTES,
        "{where_}: the post-collection occupancy must contain the live set ({} < {} bytes)",
        row.footprint_used_bytes,
        row.live_words * WORD_BYTES
    );

    // 6. The live set is intact and has the geometry the class promises.
    assert!(
        row.intact,
        "{where_}: the live set must survive every collection"
    );
    let (expected_roots, expected_words) = expected_live_set(class, row.n);
    assert_eq!(
        row.live_roots, expected_roots,
        "{where_}: the class's live-set cardinality"
    );
    assert_eq!(
        row.live_words, expected_words,
        "{where_}: the live set's frozen-layout footprint"
    );

    // 7. The cadence is accounted for.
    //
    // The due-fullsweep rule (`fullsweep_after` minors, then one fullsweep, then the minor counter
    // restarts) fires a fullsweep at collections `cycle, 2·cycle, 3·cycle, …`, so **any** run of
    // `collections` collections has at least `collections ÷ cycle` fullsweeps — an escalation can
    // only *add* one, never remove one. That floor therefore holds for both arms and every class.
    let cycle = FULLSWEEP_AFTER as u64 + 1;
    let floor = row.collections as u64 / cycle;
    assert!(
        row.majors >= floor,
        "{where_}: {} fullsweeps are below the due rule's floor of {floor} for {} collections at a \
         {cycle}-collection cadence",
        row.majors,
        row.collections
    );
    if row.arm == Arm::Copying && cadence_is_exact(class) {
        // Nothing but the due rule can have produced a fullsweep here, so the count is exact to
        // within one fullsweep cycle: `majors × cycle` never overshoots the collection count, and
        // the gap to it is shorter than a whole cycle.
        let product = row.majors * cycle;
        assert!(
            product <= row.collections as u64,
            "{where_}: the copying cadence is exact, so {} fullsweeps × {cycle} must not exceed \
             the {} collections",
            row.majors,
            row.collections
        );
        let slack = row.collections as u64 - product;
        assert!(
            slack < cycle,
            "{where_}: the copying cadence is exact to within one fullsweep cycle, but {} \
             fullsweeps × {cycle} leaves {slack} of {} collections uncovered",
            row.majors,
            row.collections
        );
    }
}

/// The live set `class` ends a run of `n` rounds with: `(roots, words)`.
///
/// Derived from the class geometry, never from a recorded total: `churn` keeps its persistent set,
/// `copy_out` its table, and `growth` keeps every batch it ever allocated. `Tally::live_words` is
/// this module's own `Σ size_object` measurement, so the equality is a real cross-check of the
/// frozen layout against the class's arithmetic rather than a restatement of it.
fn expected_live_set(class: &str, n: usize) -> (usize, usize) {
    match class {
        fixtures::CHURN => (fixtures::CHURN_PERSISTENT, fixtures::CHURN_PERSISTENT_WORDS),
        fixtures::COPY_OUT => (fixtures::TABLE_ROWS, fixtures::TABLE_WORDS),
        GROWTH => (n * GROWTH_KEEP, n * GROWTH_KEEP_WORDS),
        other => panic!("no live-set formula for class `{other}`"),
    }
}

/// Asserts that the two arms of one round measured the same class under the same geometry, so
/// their difference is the strategy and nothing else.
fn assert_pair(class: &str, round: usize, copying: &Row, immix: &Row) {
    let where_ = format!("class {class}, round {round}");
    assert_eq!(
        copying.n, immix.n,
        "{where_}: both arms must run the same N"
    );
    assert_eq!(
        copying.rounds, immix.rounds,
        "{where_}: both arms must drive the same number of rounds"
    );
    assert_eq!(
        copying.collections, immix.collections,
        "{where_}: both arms must drive the same number of collections"
    );
    assert_eq!(
        (copying.live_roots, copying.live_words),
        (immix.live_roots, immix.live_words),
        "{where_}: the two arms must end with the same live set"
    );
    assert!(
        copying.intact && immix.intact,
        "{where_}: both arms' live sets must be intact"
    );
}

// ---------------------------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------------------------

/// Prints one arm's row: the machine-readable line first (unindented, so it is greppable and
/// re-parseable), then a human line carrying the reported peak RSS — the union of the child's own
/// `VmHWM` and the parent's polled samples — beside the parent's polled figure alone.
fn print_arm_row(run: &ChildRun) {
    println!("{}", run.row.to_line());
    let row = &run.row;
    println!(
        "  round {:<2} {:<8} kind={:<7} pin={}/{:<7} collections={:<4} minors={:<4} majors={:<4} \
         words_copied={:<8} objects_copied={:<6} words_reclaimed={:<8} \
         mature_peak={:<6} {:<10} mature_capacity={:<6} young_capacity={:<6} \
         young_current={:<6} young_peak_used={:<6} footprint={}B/{}B \
         live={}roots/{}words intact={} peak_rss_kib={} polled_kib={} wall_ms={:>8.2}",
        row.round,
        row.arm.name(),
        kind_name(row.kind),
        source_name(row.config_source),
        kind_name(row.config_kind),
        row.collections,
        row.minors,
        row.majors,
        row.words_copied,
        row.objects_copied,
        row.words_reclaimed,
        row.mature_peak_words,
        row.mature_unit,
        row.mature_capacity,
        row.young_capacity,
        row.young_current_capacity,
        row.young_peak_used,
        row.footprint_capacity_bytes,
        row.footprint_used_bytes,
        row.live_roots,
        row.live_words,
        row.intact,
        run.peak_rss_kib,
        run.peak_kib,
        row.wall_ms,
    );
}

/// Every metric the estimator pairs across the arms.
const PAIRED_METRICS: &[&str] = &[
    "wall_ms",
    "peak_rss_kib",
    "polled_kib",
    "collections",
    "minors",
    "majors",
    "words_copied",
    "objects_copied",
    "words_reclaimed",
    "mature_peak_words",
    "mature_peak_lines",
    "mature_capacity",
    "young_capacity",
    "young_current_capacity",
    "young_peak_used",
    "footprint_capacity_bytes",
    "footprint_used_bytes",
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
            "peak_rss_kib" => run.peak_rss_kib as f64,
            "polled_kib" => run.peak_kib as f64,
            "collections" => row.collections as f64,
            "minors" => row.minors as f64,
            "majors" => row.majors as f64,
            "words_copied" => row.words_copied as f64,
            "objects_copied" => row.objects_copied as f64,
            "words_reclaimed" => row.words_reclaimed as f64,
            "mature_peak_words" => row.mature_peak_words as f64,
            "mature_peak_lines" => row.mature_peak_lines as f64,
            "mature_capacity" => row.mature_capacity as f64,
            "young_capacity" => row.young_capacity as f64,
            "young_current_capacity" => row.young_current_capacity as f64,
            "young_peak_used" => row.young_peak_used as f64,
            "footprint_capacity_bytes" => row.footprint_capacity_bytes as f64,
            "footprint_used_bytes" => row.footprint_used_bytes as f64,
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

    /// The per-round differences, `Immix − Copying`.
    fn diffs(&self) -> Vec<f64> {
        self.pairs.iter().map(|(c, i)| i - c).collect()
    }

    /// The median of the per-round differences (`Immix − Copying`).
    fn median_diff(&self) -> Option<f64> {
        Paired::median(&mut self.diffs())
    }

    /// The median of the per-round ratios (`Immix ÷ Copying`), skipping a zero denominator.
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
        let mut diffs = self.diffs();
        Paired::median(&mut diffs)?;
        Some((
            diffs.first().copied().unwrap_or(0.0),
            diffs.last().copied().unwrap_or(0.0),
        ))
    }

    /// The median of one arm's own samples (`0` = copying, `1` = immix).
    fn median_of(&self, arm_index: usize) -> Option<f64> {
        let mut values: Vec<f64> = self
            .pairs
            .iter()
            .map(|(copying, immix)| if arm_index == 0 { *copying } else { *immix })
            .collect();
        Paired::median(&mut values)
    }
}

/// Prints the estimator: the two arms' medians, the median paired difference, the median paired
/// ratio, and the observed range of the per-round differences.
fn print_estimator(values: &[Paired]) {
    println!(
        "    {:<24} {:>14} {:>14} {:>14} {:>12} {}",
        "metric", "copying(med)", "immix(med)", "median_diff", "med_ratio", "diff[min..max]"
    );
    for paired in values {
        let fmt = |value: Option<f64>| match value {
            Some(value) => format!("{value:.3}"),
            None => "n/a".to_string(),
        };
        let range = match paired.diff_range() {
            Some((min, max)) => format!("[{min:.3} .. {max:.3}]"),
            None => "n/a".to_string(),
        };
        println!(
            "    {:<24} {:>14} {:>14} {:>14} {:>12} {}",
            paired.name,
            fmt(paired.median_of(0)),
            fmt(paired.median_of(1)),
            fmt(paired.median_diff()),
            fmt(paired.median_ratio()),
            range,
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The parent's one driver
// ---------------------------------------------------------------------------------------------

/// **The sweep: every class's two arms, strictly interleaved, one fresh child per arm per round,
/// then the noise floor.**
///
/// This is the binary's only non-ignored test, so the classes cannot race each other for the host's
/// cores or for `/proc/loadavg`, and each class's rounds are run in one uninterrupted sequence.
/// What it asserts and what it merely prints is the module doc's "What is asserted" list; the
/// measurement itself happens in the children.
#[test]
fn the_mature_strategy_sweep_over_the_workload_classes() {
    if !has_procfs() {
        println!(
            "mature_strategy_sweep: WARNING — no procfs at /proc, so peak RSS is reported as 0 \
             (n/a) and the parent cannot sample a child's VmHWM. The deterministic counters every \
             assertion reads are unaffected; the peak-RSS column is simply absent from this run."
        );
    }
    println!(
        "mature_strategy_sweep: arms=copying/immix (explicit GcConfig pins, identical HeapPolicy \
         per class) fullsweep_after={FULLSWEEP_AFTER} rounds={} noise_repeats={NOISE_REPEATS} \
         profile={} loadavg={}",
        rounds(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        loadavg(),
    );
    for class in CLASSES {
        measure_class(class);
    }
    print_noise_floor(fixtures::CHURN);
    println!();
    println!(
        "mature_strategy_sweep: done (classes={CLASSES:?}) loadavg={}",
        loadavg()
    );
}

/// **Measures one class: the arms strictly interleaved, one child per arm per round**, asserts
/// every deterministic invariant of each arm and of each pair, and prints the per-round rows and
/// the class's estimator table.
fn measure_class(class: &'static str) {
    let n = n_for(class);
    let rounds = rounds();
    let policy = fixtures::policy(class);
    println!();
    println!(
        "mature_strategy_sweep: class={class} n={n} rounds={rounds} \
         young_words={} mature_words={} min_heap_words={} max_heap_words={} \
         fullsweep_after={} batch_words={} loadavg={}",
        policy.initial_young_words,
        policy.initial_mature_words,
        policy.min_heap_words,
        policy.max_heap_words,
        policy.fullsweep_after,
        fixtures::batch_words(class),
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
            });
            print_arm_row(&run);
            arms[index] = Some(run);
        }
        let [Some(copying), Some(immix)] = arms else {
            panic!("class {class}, round {round}: both arms must have been measured");
        };
        println!(
            "  round {round} loadavg: before={} after={} (hwm_samples {} / {})",
            copying.load_before, immix.load_after, copying.hwm_samples, immix.hwm_samples
        );
        assert_arm(class, round, &copying);
        assert_arm(class, round, &immix);
        assert_pair(class, round, &copying.row, &immix.row);
        measured.push((copying, immix));
    }
    println!(
        "  estimator (class={class}, n={n}, rounds={rounds}; paired per round, \
         Immix − Copying / Immix ÷ Copying):"
    );
    print_estimator(&Paired::all(&measured));
    println!(
        "mature_strategy_sweep: class={class} done loadavg={}",
        loadavg()
    );
}

/// **The noise floor: one arm at one fixed `N`, repeated [`NOISE_REPEATS`] times.**
///
/// The spread of the repeats' peak RSS and wall time is the band any claimed effect must beat; it
/// is **printed**, never asserted (a shared host's noise is measured, not legislated).
///
/// The deterministic side *is* asserted, and exactly: every counter of [`Row::deterministic_fields`]
/// must be bit-identical across the repeats, because a fresh collector over an identical allocation
/// sequence is a deterministic machine. A drift there is a real finding — a counter that depends on
/// something outside the geometry — and it fails the sweep rather than being averaged away.
fn print_noise_floor(class: &'static str) {
    let n = n_for(class);
    println!();
    println!(
        "mature_strategy_sweep: noise floor class={class} arm={} n={n} repeats={NOISE_REPEATS} \
         loadavg={}",
        Arm::Copying.name(),
        loadavg(),
    );
    let mut runs: Vec<ChildRun> = Vec::with_capacity(NOISE_REPEATS);
    for repeat in 1..=NOISE_REPEATS {
        let run = run_child(&ChildJob {
            class,
            arm: Arm::Copying,
            n,
            round: repeat,
        });
        print_arm_row(&run);
        assert_arm(class, repeat, &run);
        // Bit-identical deterministic counters across the repeats.
        if let Some(first) = runs.first() {
            let reference = first.row.deterministic_fields();
            let candidate = run.row.deterministic_fields();
            assert_eq!(
                reference, candidate,
                "noise floor, repeat {repeat}: the deterministic counters must be bit-identical \
                 to repeat 1's"
            );
        }
        runs.push(run);
    }
    for metric in [
        "wall_ms",
        "peak_rss_kib",
        "polled_kib",
        "words_copied",
        "objects_copied",
        "footprint_used_bytes",
    ] {
        let mut values: Vec<f64> = runs.iter().map(|run| Paired::value(metric, run)).collect();
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let median = Paired::median(&mut values).unwrap_or(f64::NAN);
        let spread = if min > 0.0 { max / min } else { f64::NAN };
        println!(
            "  noise floor: {metric:<22} min={min:>12.3} median={median:>12.3} max={max:>12.3} \
             spread={spread:.3}x"
        );
    }
    println!(
        "mature_strategy_sweep: noise floor done loadavg={}",
        loadavg()
    );
}
