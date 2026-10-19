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

//! The **`list_walk`** workload: the BIF-free per-element dispatch floor.
//!
//! `erts_rust/tools/bench`'s `list_walk` workload is the runtime's interpreter-throughput
//! metric — a body-recursive `sum/1` over a 1000-element list, walked `N` times. Its per-element
//! figure is measured across two processes (the Rust runtime and the system oracle), so it
//! cannot say *where* the per-element cost goes. This workload runs the same program
//! **in process**, at the same `N`, through [`CountingContext`], so the per-element cost is
//! attributed to an instruction count, a rooted-allocation count and a BIF count directly.
//!
//! The program ([`source`]) is the sibling workload's, verbatim in shape (module
//! `bench_list_walk`, export `walk/1`, [`LEN`] = 1000 elements):
//!
//! ```erlang
//! walk(N) -> walk(N, build(1000, []), 0).
//! walk(0, _L, Acc) -> Acc;
//! walk(N, L, Acc) -> walk(N - 1, L, Acc + sum(L)).
//! build(0, Acc) -> Acc;
//! build(N, Acc) -> build(N - 1, [N | Acc]).
//! sum([]) -> 0;
//! sum([H | T]) -> H + sum(T).
//! ```
//!
//! `walk(N)` returns `N * 500500` (the sum of `1..1000`, [`PER_SUM`]), so the result is an
//! independently checkable constant, not a self-reported timing.
//!
//! # Why `walk(K)` *and* `walk(0)`, and what the paired marginal is
//!
//! Every call pays for building the 1000-element list (`build/2`) **plus** `K` walks of it.
//! Dividing the whole call by `K * 1000` would charge that one-off build to the elements, which
//! is exactly the confound the runtime's `list_walk` figure cancels by subtracting its own
//! startup pass. This workload does the same thing in process and calls it the **paired
//! marginal**:
//!
//! * `t_K` = `walk(K)`'s ns per call (K walks, [`DEFAULT_K`] = 20 → 20 000 walked elements),
//! * `t_0` = `walk(0)`'s ns per call (the same build, zero walks),
//! * `ns_per_element = (t_K − t_0) / (K * LEN)` — the build-cancelled, per-element figure.
//!
//! `walk(0)` is the *same* export called with a small-integer `0`, so no second fixture function
//! and no second compiled artifact is involved. The two passes are not interleaved round by
//! round (the batch loop of [`super::run_calls`] runs one workload at a time), so on a shared
//! host read the **ratio** of the marginal to the per-instruction figure, not its absolute
//! value — the notes the run prints say so.
//!
//! The whole-call normalisation is still reported (through
//! [`super::ReportSpec`]'s [`super::Unit`]), because it is the number that composes with the
//! other workloads; the marginal block below it is the one that lines up with the runtime's
//! `list_walk` per-element figure.
//!
//! # BIF traffic
//!
//! The program is arithmetic-only, but a small-int `'+'/2` **is** a BIF dispatch on this
//! engine (`gc_bif` → [`ExecutionContext::call_bif`](crate::context::ExecutionContext::call_bif)),
//! so `bif_calls` is exactly one per element (going back out) plus the loop counters'
//! `'-'/2`. That is why the workload is *not* declared `bif_free`: the histogram is the
//! evidence that the arithmetic `gc_bif` is the only BIF traffic, and it is served by the
//! harness' own floor ([`super::counting`]'s module docs).

use std::sync::Arc;

use beam_loader::Module;
use beam_term::{make_small, Eterm};

use crate::jit::Tier;
use crate::machine::Machine;

use super::counting::CountingContext;
use super::{
    atom_named, compile_fixture, env_usize, instr_per_call, measure_result, report_generic,
    selected_tiers, Measurement, ReportSpec, ResultSnapshot, Unit,
};

/// The module name the fixture compiles to.
const MODULE: &str = "bench_list_walk";

/// The exported function under measurement.
const ENTRY: &str = "walk";

/// The number of list elements one walk traverses.
const LEN: i64 = 1000;

/// What one full walk returns: the sum of `1..=LEN`.
const PER_SUM: i64 = LEN * (LEN + 1) / 2;

/// The default walks per call ([`DEFAULT_K`] × [`LEN`] = 20 000 walked elements per call).
const DEFAULT_K: usize = 20;

/// Default measured calls.
const DEFAULT_ITERS: usize = 100;
/// Default calls per fresh machine/host batch (a call is ~20 000 walked elements).
const DEFAULT_BATCH: usize = 10;
/// Default untimed warmup calls.
const DEFAULT_WARMUP: usize = 10;

/// Decompose the `list_walk` per-element dispatch floor on every selected tier.
///
/// `#[ignore]`d: it is a measurement, not a gate. Run it with
/// `-- --ignored --nocapture list_walk_decomposition`; `BENCH_K` selects the walks per call.
#[test]
#[ignore = "measurement harness"]
fn list_walk_decomposition() {
    let source = source();
    let beam = match compile_fixture(MODULE, &source, "list_walk") {
        Ok(beam) => beam,
        Err(why) => {
            println!("SKIP: {why} -- the list_walk decomposition did not run");
            return;
        }
    };
    let module = match load_file_or_skip(&beam) {
        Some(module) => module,
        None => return,
    };
    let entry = atom_named(&module, ENTRY);
    let walks = env_usize("BENCH_K", DEFAULT_K) as i64;
    let iters = env_usize("BENCH_ITERS", DEFAULT_ITERS);
    let batch = env_usize("BENCH_BATCH", DEFAULT_BATCH);
    let warmup = env_usize("BENCH_WARMUP", DEFAULT_WARMUP);
    let tiers = selected_tiers();

    let elements = (walks * LEN) as f64;
    let expected_walks = walks * PER_SUM;
    let label = format!("{MODULE}:{ENTRY}/1 ({walks} walks of {LEN} elements per call)");

    println!("workload = {label}");
    println!("beam = {}", beam.display());
    println!("elements_per_call = {elements:.0} (walk(K) = {walks} walks of {LEN} elements)");
    println!("expected_result = {expected_walks} (walk({walks}) = {walks} * {PER_SUM})");
    println!("expected_result_walk_0 = 0 (the same export, zero walks: the build only)");
    println!("note = the program is BIF-free Erlang, but a small-int '+'/'-' IS a BIF dispatch on");
    println!("note = this engine, so bif_calls_per_element is the arithmetic gc_bif traffic -- the");
    println!("note = histogram at the end of each block is the evidence that it is the ONLY one.");
    println!("note = instr_per_element divides the WHOLE call (build included) by {elements:.0};");
    println!("note = the marginal block subtracts the walk(0) pass and is the figure that lines up");
    println!("note = with the runtime's list_walk per-element number.");

    let prepare_walks = move |_ctx: &mut CountingContext,
                              _machine: &mut Machine|
          -> beam_common::Result<Eterm> { Ok(make_small(walks)) };
    let prepare_zero =
        |_ctx: &mut CountingContext, _machine: &mut Machine| -> beam_common::Result<Eterm> {
            Ok(make_small(0))
        };

    // (tier, walk(K) phase, walk(0) phase)
    let mut measured: Vec<(Tier, Measurement, Measurement)> = Vec::new();
    for tier in &tiers {
        let walks_phase = match measure_result(
            &module,
            entry,
            *tier,
            iters,
            batch,
            warmup,
            &prepare_walks,
        ) {
            Ok(phase) => phase,
            Err(error) => {
                println!(
                    "SKIP: walk({walks}) on {}: {error:?} -- the list_walk decomposition did not run",
                    tier.name()
                );
                return;
            }
        };
        let zero_phase =
            match measure_result(&module, entry, *tier, iters, batch, warmup, &prepare_zero) {
                Ok(phase) => phase,
                Err(error) => {
                    println!(
                        "SKIP: walk(0) on {}: {error:?} -- the list_walk decomposition did not run",
                        tier.name()
                    );
                    return;
                }
            };
        measured.push((*tier, walks_phase, zero_phase));
    }

    // The instruction count is a property of the code, not of the tier: the native tier offers
    // the same reduction hook once per driver-loop iteration (not once per instruction).
    let interpreter_walks = measured
        .iter()
        .find(|(tier, _, _)| *tier == Tier::Interpreter)
        .map(|(_, walks_phase, _)| walks_phase);
    let interpreter_zero = measured
        .iter()
        .find(|(tier, _, _)| *tier == Tier::Interpreter)
        .map(|(_, _, zero_phase)| zero_phase);
    let instr_walks = instr_per_call(interpreter_walks, &module, entry, batch, &prepare_walks);
    let instr_zero = instr_per_call(interpreter_zero, &module, entry, batch, &prepare_zero);

    let expect_walks =
        |measurement: &Measurement| measurement.snapshot == ResultSnapshot::Small(expected_walks);
    let spec = ReportSpec {
        workload: &label,
        unit: Some(Unit {
            label: "element",
            per_call: elements,
        }),
        expect: &expect_walks,
        expected: expected_walks.to_string(),
        bif_free: false,
    };

    for (tier, walks_phase, zero_phase) in &measured {
        assert_eq!(
            zero_phase.snapshot,
            ResultSnapshot::Small(0),
            "walk(0) on {} must build the list and return 0",
            tier.name()
        );
        report_generic(walks_phase, *tier, instr_walks, &spec);
        report_marginal(
            *tier,
            &label,
            walks,
            walks_phase,
            zero_phase,
            elements,
            instr_walks,
            instr_zero,
        );
    }
    if let [(_, first_walks, first_zero), (_, second_walks, second_zero)] = measured.as_slice() {
        super::assert_tiers_agree(first_walks, second_walks, &label);
        super::assert_tiers_agree(first_zero, second_zero, &format!("{label} walk(0)"));
    }
}

/// Load `beam`, or print the skip reason and leave the entry point; `None` means "skip".
fn load_file_or_skip(beam: &std::path::Path) -> Option<Arc<Module>> {
    match beam_loader::load_file(beam) {
        Ok(module) => Some(Arc::new(module)),
        Err(error) => {
            println!(
                "SKIP: load {}: {error:?} -- the list_walk decomposition did not run",
                beam.display()
            );
            None
        }
    }
}

/// The paired, build-cancelled marginal: one tier's `walk(K)` phase minus its `walk(0)` phase.
///
/// `elements` is the number of elements the `K` walks traverse, so the ns figure is the
/// per-element dispatch floor with the list build cancelled out. The structural counters are
/// differenced the same way, so `marginal_instr_per_element` is the ~7 instructions one element
/// costs the interpreter rather than the whole call averaged over the elements.
#[allow(clippy::too_many_arguments)]
fn report_marginal(
    tier: Tier,
    label: &str,
    walks: i64,
    walks_phase: &Measurement,
    zero_phase: &Measurement,
    elements: f64,
    instr_walks: f64,
    instr_zero: f64,
) {
    let per_call = |phase: &Measurement, count: u64| count as f64 / phase.iters.max(1) as f64;
    let marginal = |walks_count: u64, zero_count: u64| {
        (per_call(walks_phase, walks_count) - per_call(zero_phase, zero_count)) / elements
    };
    let min_walks = walks_phase.min();
    let min_zero = zero_phase.min();
    let median_walks = walks_phase.median();
    let median_zero = zero_phase.median();
    println!("---");
    println!("workload = {label} marginal (paired walk(K) - walk(0))");
    println!("tier = {}", tier.name());
    println!("walks_per_call = {walks}");
    println!("elements_per_call = {elements:.0}");
    println!("min_ns_per_call_walk_K = {min_walks:.1}");
    println!("median_ns_per_call_walk_K = {median_walks:.1}");
    println!("min_ns_per_call_walk_0 = {min_zero:.1}");
    println!("median_ns_per_call_walk_0 = {median_zero:.1}");
    println!("ns_per_element = {:.2}", (min_walks - min_zero) / elements);
    println!(
        "median_ns_per_element = {:.2}",
        (median_walks - median_zero) / elements
    );
    println!(
        "marginal_instr_per_element = {:.3}",
        (instr_walks - instr_zero) / elements
    );
    println!(
        "marginal_instr_per_call_build = {instr_zero:.2} (walk(0): the 1000-element build)"
    );
    println!(
        "marginal_rooted_calls_per_element = {:.4}",
        marginal(
            walks_phase.counters.rooted_calls,
            zero_phase.counters.rooted_calls
        )
    );
    println!(
        "marginal_rooted_words_per_element = {:.4}",
        marginal(
            walks_phase.counters.rooted_words,
            zero_phase.counters.rooted_words
        )
    );
    println!(
        "marginal_heap_door_calls_per_element = {:.4}",
        marginal(walks_phase.counters.heap_calls, zero_phase.counters.heap_calls)
    );
    println!(
        "marginal_bif_calls_per_element = {:.4}",
        marginal(walks_phase.counters.bif_calls, zero_phase.counters.bif_calls)
    );
    println!(
        "marginal_local_call_requests_per_element = {:.4}",
        marginal(
            walks_phase.counters.local_call_requests,
            zero_phase.counters.local_call_requests
        )
    );
    println!(
        "note = this marginal is the difference of two separately measured phases, not of two"
    );
    println!(
        "note = interleaved rounds, so on a shared host read it as a ratio against"
    );
    println!("note = marginal_instr_per_element, never as an absolute.");
}

/// The fixture source: the `tools/bench` `list_walk` program, generated from the constants
/// above so the Rust and Erlang sides cannot drift.
fn source() -> String {
    format!(
        "\
-module({MODULE}).
-export([{ENTRY}/1]).

%% A BIF-free, body-recursive walk of a {LEN}-element list: nothing but the interpreter.
%% {ENTRY}(N) -> N * {PER_SUM} (the sum of 1..{LEN}), so the result is a deterministic
%% constant that both tiers must return.
{ENTRY}(N) -> {ENTRY}(N, build({LEN}, []), 0).

{ENTRY}(0, _L, Acc) -> Acc;
{ENTRY}(N, L, Acc) -> {ENTRY}(N - 1, L, Acc + sum(L)).

build(0, Acc) -> Acc;
build(N, Acc) -> build(N - 1, [N | Acc]).

sum([]) -> 0;
sum([H | T]) -> H + sum(T).
"
    )
}
