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

//! The **plateau pin** for the unscanned-root valve's childless-foreign exemption
//! ([`super::root_is_unscanned`]).
//!
//! # What this pins
//!
//! A process that mints a reference per iteration — `{make_ref(), self(), 17}` handed to a
//! worker and echoed back, so the reference rides the process's own reachable graph — must keep
//! the **same** heap profile as the identical loop that mints no reference at all.
//!
//! Before the exemption they differed by three orders of magnitude. `make_ref/0`'s
//! runtime-native result is a childless frozen M1 block on the runtime's pooled BIF heap — a
//! region the process's collector does not scan — so it opened the sticky per-pid descent latch
//! on the first iteration, every later collection of that process was declined, its
//! `collection_debt` was never settled, and the allocation doors' no-relocation
//! [`Process::grow_nursery`](beam_process::Process) retry **retired** a nursery space per growth
//! (`Nursery::collapse` runs only on a collection). Measured on this fixture's tree: a peak
//! `total_heap_size` of 833 108 words with references versus 466 without.
//!
//! # Why the measurement is self-calibrating
//!
//! The measurement is the process's own collector ([`beam_gc::HeapInfo`] over
//! [`beam_gc::HeapObservable`]), sampled at every slice boundary of the run — the only window a
//! live process's collector is readable from outside (`Runtime::spawn_process` removes a
//! terminated process from the table). It is a **structure**, never a clock, never an oracle
//! figure, and it hard-codes no heap-policy number: the two loops are the **same compiled
//! module**, differ only in whether the payload tuple carries a reference, and must report the
//! same peak. Two more assertions bound the same figure along the iteration axis, so a policy
//! change that moved the plateau would be caught as an N-dependent growth.
//!
//! The fixture is compiled with the system `erlc` (the crate's
//! [`crate::context::test_support`] convention); a missing toolchain is a **loud skip**, never a
//! silent pass.

use std::path::Path;
use std::time::{Duration, Instant};

use beam_term::prelude::make_small;
use beam_term::Eterm;

use crate::context::test_support::{compile_fixture, resolve_erlc};
use crate::runtime::Runtime;

/// The **reference-loop** fixture, and its reference-free control.
///
/// Both loops are the measured regression's shape (`srstats`) with the timer arming and the
/// statistics call removed: per iteration one heap tuple, one cross-process send and the matching
/// receive, so the send copies the tuple into the worker's heap and the reply copies it back.
/// `run/1` mints a reference into that tuple; `run_without_refs/1` is the same loop with the
/// reference left out — the control that says minting one is not a heap event.
const REF_LOOP_SOURCE: &str = r#"
-module(gc_ref_loop).
-export([run/1, run_without_refs/1]).
run(N) when is_integer(N), N >= 0 ->
    loop(N, spawn(fun worker/0), 0).
run_without_refs(N) when is_integer(N), N >= 0 ->
    loop_without_refs(N, spawn(fun worker/0), 0).
loop(0, _W, Acc) ->
    Acc;
loop(N, W, Acc) ->
    Tag = {make_ref(), self(), 17},
    W ! {self(), Tag},
    receive
        {W, T2} when T2 =:= Tag -> loop(N - 1, W, Acc + element(3, T2))
    end.
loop_without_refs(0, _W, Acc) ->
    Acc;
loop_without_refs(N, W, Acc) ->
    Tag = {self(), 17},
    W ! {self(), Tag},
    receive
        {W, T2} when T2 =:= Tag -> loop_without_refs(N - 1, W, Acc + element(2, T2))
    end.
worker() ->
    receive
        {From, T} -> From ! {self(), T}, worker()
    end.
"#;

/// The `{module, function}` of the two loops in [`REF_LOOP_SOURCE`].
const WITH_REFS: (&str, &str) = ("gc_ref_loop", "run");
/// The reference-free control (see [`WITH_REFS`]).
const WITHOUT_REFS: (&str, &str) = ("gc_ref_loop", "run_without_refs");

/// What one iteration accumulates into a loop's return value — `element(3, Tag)` of the
/// reference-bearing payload (`{make_ref(), self(), 17}`) and `element(2, Tag)` of the control's
/// (`{self(), 17}`), so the finished value proves every iteration really ran.
const ACCUMULATED_PER_ITERATION: i64 = 17;

/// How far a loop's peak heap may exceed another's and still count as the same profile, in
/// words. The two loops' live sets differ by the 4-word reference block, which lives on the
/// pooled BIF heap — **not** the process heap this samples — so the post-exemption peaks are
/// equal and this only absorbs a slab/policy rounding step, not retention.
const PLATEAU_SLACK_WORDS: usize = 1_024;

/// The smaller iteration count; every loop is measured at this count and at `2 *` it.
const PLATEAU_ITERATIONS: i64 = 20_000;

/// A generous wall-clock safety net for one measured run: the loops are pure interpreter work
/// under the debug runtime and finish well inside it. It only bounds a pathological hang and is
/// never measured.
const RUN_DEADLINE: Duration = Duration::from_secs(600);

/// The peak heap of one scheduled run of a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PeakHeap {
    /// The loop's returned value (`ACCUMULATED_PER_ITERATION * N` when it completed).
    value: i64,
    /// The largest `HeapInfo::heap_size` — the **summed** capacity of every nursery space, the
    /// figure a declined collection inflates.
    peak_heap_words: usize,
    /// The largest `HeapInfo::total_heap_size` (nursery plus mature generation, in words).
    peak_total_words: usize,
    /// The largest retired-nursery chain length observed at a slice boundary.
    peak_young_spaces: usize,
}

/// Runs `module:function/1` with `n` as a **scheduled process**, sampling that process's own
/// collector at every slice boundary.
fn run_counting_peak(rt: &mut Runtime, (module, function): (&str, &str), n: i64) -> PeakHeap {
    let pid = rt
        .spawn_process(module, function, vec![make_small(n)])
        .unwrap_or_else(|err| panic!("{module}:{function}/1 must spawn: {err}"));
    let mut peak = PeakHeap {
        value: 0,
        peak_heap_words: 0,
        peak_total_words: 0,
        peak_young_spaces: 0,
    };
    let deadline = Instant::now() + RUN_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "{module}:{function}/1 with N={n} did not terminate within {}s",
            RUN_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        if let Some(process) = rt.processes().get(pid) {
            let info = beam_gc::HeapInfo::of(process.collector());
            peak.peak_heap_words = peak.peak_heap_words.max(info.heap_size);
            peak.peak_total_words = peak.peak_total_words.max(info.total_heap_size);
            peak.peak_young_spaces = peak
                .peak_young_spaces
                .max(process.collector().young_space_count());
        }
    }
    let result: Eterm = rt.process_result(pid).expect("the loop returned a value");
    assert!(
        beam_term::prelude::is_small(result),
        "{module}:{function}({n}) must return a small integer"
    );
    peak.value = beam_term::signed_value(result);
    peak
}

/// A booted runtime with `beam` loaded and its run queue drained, so the measured loop does not
/// contend with the boot tree for slices (the runner tests' own rig).
fn quiescent_runtime(beam: &Path) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    let mut guard = 0u32;
    while rt.run_one_slice().expect("a boot-drain slice runs") > 0 {
        guard += 1;
        assert!(guard < 100_000, "the boot process tree never quiesced");
    }
    rt.load_module(beam).expect("the reference-loop fixture loads");
    rt
}

/// Measures one loop at `PLATEAU_ITERATIONS` and at twice that, on a fresh runtime each time
/// (the peak over a run is a per-run figure), asserting both runs ran to completion.
fn measure_at_both_counts(beam: &Path, target: (&str, &str)) -> (PeakHeap, PeakHeap) {
    let narrow = run_counting_peak(&mut quiescent_runtime(beam), target, PLATEAU_ITERATIONS);
    let wide = run_counting_peak(
        &mut quiescent_runtime(beam),
        target,
        2 * PLATEAU_ITERATIONS,
    );
    assert_eq!(
        narrow.value,
        ACCUMULATED_PER_ITERATION * PLATEAU_ITERATIONS,
        "{target:?} must complete every iteration: {narrow:?}"
    );
    assert_eq!(
        wide.value,
        ACCUMULATED_PER_ITERATION * 2 * PLATEAU_ITERATIONS,
        "{target:?} must complete every iteration: {wide:?}"
    );
    (narrow, wide)
}

/// The loud skip of the crate's toolchain convention (`crate::context::test_support`): a missing
/// `erlc` can never turn this proof into a silent pass.
fn skip_without_erlc(why: &str) {
    eprintln!();
    eprintln!("=============== beam_rt reference-loop plateau pin SKIPPED ===============");
    eprintln!("{why}: the scheduled-process plateau pin cannot run.");
    eprintln!("Set $ERTS_ORACLE (or install Erlang/OTP) and re-run. NOT a pass.");
    eprintln!("==========================================================================");
    eprintln!();
}

/// **A process that mints a reference per iteration keeps a bounded, reference-free heap.**
///
/// The reference-bearing loop and its reference-free control are each driven at `N` and `2N`
/// iterations on a fresh runtime, sampling the running process's peak
/// `total_heap_size`/`heap_size`/nursery-chain length. Three structural assertions, none of them a
/// wall clock and none of them an oracle figure:
///
/// 1. **the exemption**: at `N` the reference-bearing loop's peak must not exceed the control's
///    by more than [`PLATEAU_SLACK_WORDS`] — a childless foreign leaf costs the heap nothing.
///    This is the assertion the pre-exemption valve misses by three orders of magnitude (its
///    retention is the accumulated retired-nursery chain: 833 108 words against the control's
///    466 on this fixture);
/// 2. **the plateau**: each loop's `2N` peak must not exceed its own `N` peak by more than the
///    same slack — retention that grows with the iteration count is exactly what a declined
///    collection produces;
/// 3. the retired-nursery chain obeys the same two bounds, so a chain that only *grows* (without
///    inflating `total_heap_size` yet) is caught too.
#[test]
fn a_process_that_mints_a_reference_per_iteration_keeps_a_bounded_heap() {
    let Some(erlc) = resolve_erlc() else {
        skip_without_erlc("no erlc on $PATH");
        return;
    };
    let dir = compile_fixture(&erlc, "gc_ref_loop_plateau", "gc_ref_loop", REF_LOOP_SOURCE);
    let beam = dir.join("gc_ref_loop.beam");

    let (refs_narrow, refs_wide) = measure_at_both_counts(&beam, WITH_REFS);
    let (control_narrow, control_wide) = measure_at_both_counts(&beam, WITHOUT_REFS);
    eprintln!("reference-loop plateau pin (N : value peak_heap_words peak_total_words spaces)");
    eprintln!("  with refs   N={PLATEAU_ITERATIONS} : {refs_narrow:?}");
    eprintln!("  with refs   N={} : {refs_wide:?}", 2 * PLATEAU_ITERATIONS);
    eprintln!("  without refs N={PLATEAU_ITERATIONS} : {control_narrow:?}");
    eprintln!(
        "  without refs N={} : {control_wide:?}",
        2 * PLATEAU_ITERATIONS
    );

    assert!(
        refs_narrow.peak_total_words <= control_narrow.peak_total_words + PLATEAU_SLACK_WORDS,
        "minting a reference per iteration must cost the process heap nothing: peak \
         total_heap_size is {} words with references but {} words without (slack \
         {PLATEAU_SLACK_WORDS}) — a grown nursery chain means collections were declined",
        refs_narrow.peak_total_words,
        control_narrow.peak_total_words,
    );
    assert!(
        refs_wide.peak_total_words <= control_wide.peak_total_words + PLATEAU_SLACK_WORDS,
        "the same must hold at N={}: {} words with references vs {} without",
        2 * PLATEAU_ITERATIONS,
        refs_wide.peak_total_words,
        control_wide.peak_total_words,
    );
    for (label, narrow, wide) in [
        ("with references", refs_narrow, refs_wide),
        ("without references", control_narrow, control_wide),
    ] {
        assert!(
            wide.peak_total_words <= narrow.peak_total_words + PLATEAU_SLACK_WORDS,
            "{label}: peak total_heap_size must plateau in N, but is {} words at N={} and {} at \
             N={} (retention grows with the iteration count)",
            narrow.peak_total_words,
            PLATEAU_ITERATIONS,
            wide.peak_total_words,
            2 * PLATEAU_ITERATIONS,
        );
        assert!(
            wide.peak_young_spaces <= narrow.peak_young_spaces.max(1),
            "{label}: the retired-nursery chain must not grow with N ({} spaces at N={}, {} at \
             N={})",
            narrow.peak_young_spaces,
            PLATEAU_ITERATIONS,
            wide.peak_young_spaces,
            2 * PLATEAU_ITERATIONS,
        );
    }
    assert!(
        refs_narrow.peak_young_spaces <= control_narrow.peak_young_spaces.max(1),
        "the reference-bearing loop must not retire more nursery spaces than the control: {} vs \
         {}",
        refs_narrow.peak_young_spaces,
        control_narrow.peak_young_spaces,
    );
}
