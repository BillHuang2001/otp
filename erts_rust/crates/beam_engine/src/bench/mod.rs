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

//! A small, in-process **decomposition harness** for real foreign workloads.
//!
//! [`foreign_bstr_to_lower_decomposition`] compiles `cowboy_bstr:to_lower/1` with the system
//! `erlc`, loads the artifact through [`beam_loader`], and runs it on **both** execution tiers
//! — the interpreter and the native baseline tier — reporting wall-clock cost *and* the
//! deterministic host-side counters of [`counting::CountingContext`] (instructions, the two
//! heap-admission doors, BIF dispatches, off-heap payloads) plus, for the baseline tier, the
//! existing per-thread census (`crate::jit::baseline::counters`) that splits the per-variant
//! deopt count from the emitted-instruction census.
//!
//! The point is **decomposition**, not headline throughput: one foreign Erlang function whose
//! per-call cost is attributed to instruction count, allocation-door traffic and (on the native
//! tier) the instructions that still deopt to the interpreter. Only the correct-result
//! invariants and tier agreement are asserted — every timing figure is evidence, not a gate.
//!
//! Measurement discipline (inherited from `TEST_NOTES.md`): a **fresh** [`Machine`] and a
//! **fresh** inner host per batch of `BENCH_BATCH` calls, an untimed warmup phase, and a
//! measured phase split into five sub-runs of which the **minimum** and **median** are
//! reported. `Machine::refc_binaries` and the inner host's region heap grow monotonically within
//! a batch and this harness never prunes them (a host may, with `retain_refc_binaries`), so one
//! 20 000-call machine would leak roughly 600k payloads instead of measuring anything.
//!
//! ## The workload, verbatim
//!
//! [`SOURCE`] carries `to_lower/1` and `char_to_lower/1` copied **verbatim** (the comment
//! included) from `cowboy_bstr.erl` in the Cowboy project
//! (<https://github.com/ninenines/cowboy>), which is distributed under the ISC licence
//! (© Loïc Hoguin). Only those two functions plus the module/export declarations are kept, so
//! the compiled shape is exactly the upstream one:
//!
//! * `to_lower/1` — `bs_init_writable` of a 256-bit (32-byte) writable binary, then a
//!   `call_last 2` into the binary-comprehension body;
//! * that body — `bs_start_match3` + `bs_match` (`ensure_at_least 8`, `integer 8`), a call to
//!   `char_to_lower/1`, and a `bs_create_bin` with `[private_append, …]` per byte, tail-called;
//! * `char_to_lower/1` — a 27-clause pure-Erlang `select_val` (no BIF).
//!
//! That is why the harness reports zero BIF dispatches: the whole computation is engine-side.
//!
//! [`long_append::foreign_long_append_decomposition`] is the second workload: a fixture whose
//! binary comprehension genuinely **outgrows** its `bs_init_writable` hint (16 KiB in, 64 KiB
//! out, against a 32-byte hint), so it measures the writable accumulator's payload **growth**
//! rather than a build that fits the hint. Both workloads share this module's batch loop
//! ([`run_calls`]), [`measure`] and [`Measurement`]; the fixture-specific parts live in the
//! sibling file.
//!
//! Two more workloads attribute the engine's per-call cost at the **same `N`** from inside the
//! crate, so the runtime's own per-workload figures can be decomposed without its CLI boot:
//!
//! * [`list_walk::list_walk_decomposition`] — the BIF-free per-element dispatch floor
//!   (`erts_rust/tools/bench`'s `list_walk` program, verbatim), reported **per element** with a
//!   paired, build-cancelled marginal (`walk(K)` minus `walk(0)`).
//! * [`foreign::foreign_cowboy_bstr_decomposition`] and
//!   [`foreign::foreign_emqx_base62_decomposition`] — real functions compiled from the
//!   **read-only foreign checkouts** (`cowboy_bstr:to_lower/1`, `cowboy_bstr:capitalize_token/1`,
//!   `emqx_base62:encode/1`, `emqx_base62:decode/1`), each with one fixed input and one verified
//!   expected result.
//!
//! Every workload supplies its own correctness predicate and its own per-unit label through
//! [`ReportSpec`]; [`report_generic`] is the single report formatter, so the key set is identical
//! across workloads and the per-case `workload = …` line names what was measured.

mod counting;
mod foreign;
mod list_walk;
mod long_append;
mod send_split;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_loader::{load_file, Module};
use beam_term::{is_small, signed_value, Atom, Eterm, NIL};

use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::{binary_bytes, make_heap_binary};

use counting::{Counters, CountingContext};

/// The module name the fixture compiles to.
const MODULE: &str = "cowboy_bstr";

/// The exported function under measurement.
const ENTRY: &str = "to_lower";

/// The measured argument: 30 bytes in.
const INPUT: &[u8] = b"The Quick Brown Fox 0123456789";

/// What `cowboy_bstr:to_lower/1` must return for [`INPUT`] (30 bytes out).
const EXPECTED: &[u8] = b"the quick brown fox 0123456789";

/// The fixture source: upstream Cowboy's `to_lower/1` + `char_to_lower/1`, verbatim.
const SOURCE: &str = r#"-module(cowboy_bstr).
-export([to_lower/1]).

%% The first letter and all letters after a dash are capitalized.
-spec to_lower(B) -> B when B::binary().
to_lower(B) ->
	<< << (char_to_lower(C)) >> || << C >> <= B >>.

-spec char_to_lower(char()) -> char().
char_to_lower($A) -> $a;
char_to_lower($B) -> $b;
char_to_lower($C) -> $c;
char_to_lower($D) -> $d;
char_to_lower($E) -> $e;
char_to_lower($F) -> $f;
char_to_lower($G) -> $g;
char_to_lower($H) -> $h;
char_to_lower($I) -> $i;
char_to_lower($J) -> $j;
char_to_lower($K) -> $k;
char_to_lower($L) -> $l;
char_to_lower($M) -> $m;
char_to_lower($N) -> $n;
char_to_lower($O) -> $o;
char_to_lower($P) -> $p;
char_to_lower($Q) -> $q;
char_to_lower($R) -> $r;
char_to_lower($S) -> $s;
char_to_lower($T) -> $t;
char_to_lower($U) -> $u;
char_to_lower($V) -> $v;
char_to_lower($W) -> $w;
char_to_lower($X) -> $x;
char_to_lower($Y) -> $y;
char_to_lower($Z) -> $z;
char_to_lower(Ch) -> Ch.
"#;

/// Default measured calls.
const DEFAULT_ITERS: usize = 20_000;
/// Default calls per fresh machine/host batch.
const DEFAULT_BATCH: usize = 500;
/// Default untimed warmup calls.
const DEFAULT_WARMUP: usize = 2_000;
/// The measured phase is at least this many timed sub-runs.
const SUB_RUNS: usize = 5;
/// How many rows of a per-variant ranking are printed.
const TOP_ROWS: usize = 12;
/// How many rows of the BIF histogram are printed.
const TOP_BIFS: usize = 10;

/// Decompose `cowboy_bstr:to_lower/1` on every selected tier.
///
/// `#[ignore]`d: it is a measurement, not a gate. Run it with
/// `-- --ignored --nocapture foreign_bstr_to_lower_decomposition`.
#[test]
#[ignore = "measurement harness"]
fn foreign_bstr_to_lower_decomposition() {
    let beam = match compile_fixture(MODULE, SOURCE, "bstr") {
        Ok(beam) => beam,
        Err(why) => {
            println!("SKIP: {why} -- the cowboy_bstr:to_lower/1 decomposition did not run");
            return;
        }
    };
    let module = Arc::new(
        load_file(&beam).unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display())),
    );
    let iters = env_usize("BENCH_ITERS", DEFAULT_ITERS);
    let batch = env_usize("BENCH_BATCH", DEFAULT_BATCH);
    let warmup = env_usize("BENCH_WARMUP", DEFAULT_WARMUP);
    let tiers = selected_tiers();

    println!(
        "workload = {MODULE}:{ENTRY}/1 ({} bytes in, {} bytes out)",
        INPUT.len(),
        EXPECTED.len()
    );
    println!("beam = {}", beam.display());
    println!("note = instr_per_call is the INTERPRETER's reduce-hook count (one charge per");
    println!("note = interpreted instruction); the native tier offers the same hook once per");
    println!("note = driver-loop iteration (a block entry), so the instruction figure -- a");
    println!("note = property of the code -- is reported for both tiers.");
    println!("note = every *_per_call count is deterministic (a host-side tally); min/median");
    println!("note = ns_per_call are wall clock on a shared host -- compare ratios, not absolutes.");

    let mut measurements = Vec::new();
    for tier in &tiers {
        measurements.push((
            *tier,
            measure(
                &module,
                atom_named(&module, ENTRY),
                *tier,
                iters,
                batch,
                warmup,
                &bstr_input,
            ),
        ));
    }
    // The instruction count is a property of the code, not of the tier (see the header note).
    let entry = atom_named(&module, ENTRY);
    let interpreter = measurements
        .iter()
        .find(|(tier, _)| *tier == Tier::Interpreter)
        .map(|(_, measurement)| measurement);
    let per_call_instr = instr_per_call(interpreter, &module, entry, batch, &bstr_input);

    for (tier, measurement) in &measurements {
        report(measurement, *tier, per_call_instr);
    }
    if let [(_, first), (_, second)] = measurements.as_slice() {
        assert_tiers_agree(first, second, &format!("{MODULE}:{ENTRY}/1"));
    }
    if tiers.contains(&Tier::BaselineJit) {
        baseline_census(&beam, ENTRY, iters, batch, &bstr_input, None);
    }
}

/// One tier's measured phase.
struct Measurement {
    /// The calls the phase measured: `SUB_RUNS` whole sub-runs of [`measured_calls`].
    iters: usize,
    /// ns per call, one sample per timed sub-run.
    samples: Vec<f64>,
    /// The host counters accumulated over the whole phase.
    counters: Counters,
    /// Off-heap payloads the phase's machines were holding at their last call.
    refc_payloads: u64,
    /// The last measured call's raw result.
    ///
    /// Kept for the workloads that still key their own report off it
    /// ([`long_append`](long_append)); the generic report uses [`Measurement::snapshot`],
    /// because a term is only guaranteed readable while the host that built it is alive.
    result: Eterm,
    /// The last measured call's result, **copied out while the host was still alive**.
    ///
    /// `measure` captures this at the end of the phase, before the batch's [`CountingContext`]
    /// (and with it the region heap every term of that batch lives on) is dropped, so the
    /// generic report's correctness predicate never reads a retired heap.
    snapshot: ResultSnapshot,
    /// `(decided_without_cmp, decided_by_cmp)`: how the phase's `select_val` pairs were
    /// answered, from `crate::interpreter`'s test-only pair tally (see the `select` node).
    ///
    /// Deterministic, like the [`Counters`] — a structural figure, not a timing one. On the
    /// native tier a `select_val` the emitter inlines never reaches the interpreter's ladder
    /// at all, so the two tiers legitimately report different values here.
    select_answers: (usize, usize),
}

/// What a measured call returned, copied out while its host was alive.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ResultSnapshot {
    /// The result was a binary: its bytes.
    Binary(Vec<u8>),
    /// The result was a small integer: its value.
    Small(i64),
    /// The result is neither of the above (an atom, a tuple, a refc binary, …): its bytes, if it
    /// even has any, are not part of this harness' contract.
    Opaque,
}

impl ResultSnapshot {
    /// Snapshot `term`; call this while the host that built it is still alive.
    fn of(term: Eterm) -> ResultSnapshot {
        if is_small(term) {
            return ResultSnapshot::Small(signed_value(term));
        }
        match binary_bytes(term) {
            Ok(bytes) => ResultSnapshot::Binary(bytes.to_vec()),
            Err(_) => ResultSnapshot::Opaque,
        }
    }
}

impl Measurement {    /// The fastest sub-run (the robust statistic under additive background load).
    fn min(&self) -> f64 {
        self.samples.iter().copied().fold(f64::INFINITY, f64::min)
    }

    /// The median sub-run.
    fn median(&self) -> f64 {
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite ns/call"));
        let middle = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[middle - 1] + sorted[middle]) / 2.0
        } else {
            sorted[middle]
        }
    }
}

/// The calls one timed sub-run makes: `iters` rounded up to `SUB_RUNS` whole sub-runs, each at
/// least one batch long.
fn measured_calls(iters: usize) -> usize {
    iters.div_ceil(SUB_RUNS).max(1)
}

/// Run the untimed warmup and the measured phase of one tier.
///
/// Panics on a workload error: the fixtures in this harness are expected to run. A workload
/// whose *capability* is host-dependent (the foreign cases, whose BIF traffic a minimal host
/// may not be able to serve) uses [`measure_result`] and reports a loud skip instead.
fn measure(
    module: &Arc<Module>,
    entry: Atom,
    tier: Tier,
    iters: usize,
    batch: usize,
    warmup: usize,
    prepare: Prepare<'_>,
) -> Measurement {
    measure_result(module, entry, tier, iters, batch, warmup, prepare)
        .unwrap_or_else(|error| panic!("measure on {}: {error:?}", tier.name()))
}

/// [`measure`], but a workload error (rather than a panic) is reported to the caller.
///
/// The error surfaces from the **warmup** pass first, which is the point: a fixture the host
/// cannot serve never reaches the timed phase, so no figure is ever reported for it.
fn measure_result(
    module: &Arc<Module>,
    entry: Atom,
    tier: Tier,
    iters: usize,
    batch: usize,
    warmup: usize,
    prepare: Prepare<'_>,
) -> beam_common::Result<Measurement> {
    // Warmup runs on its own context, so none of its traffic lands in the phase's counters.
    let mut warm = CountingContext::new(Arc::clone(module));
    run_calls(module, entry, tier, warmup, batch, &mut warm, prepare)?;
    drop(warm);

    let per_run = measured_calls(iters);
    let mut ctx = CountingContext::new(Arc::clone(module));
    let mut samples = Vec::with_capacity(SUB_RUNS);
    let mut refc_payloads = 0u64;
    let mut result = NIL;
    crate::interpreter::reset_select_answers();
    for _ in 0..SUB_RUNS {
        let phase = run_calls(module, entry, tier, per_run, batch, &mut ctx, prepare)?;
        samples.push(ns_per_call(phase.elapsed, per_run));
        refc_payloads += phase.refc_payloads;
        result = phase.result;
    }
    let select_answers = crate::interpreter::select_answers();
    // The result term lives on the last batch's host heap, which is dropped when `ctx` is; take
    // the copy now (see `Measurement::snapshot`).
    let snapshot = ResultSnapshot::of(result);
    Ok(Measurement {
        iters: per_run * SUB_RUNS,
        samples,
        counters: ctx.counters,
        refc_payloads,
        result,
        snapshot,
        select_answers,
    })
}

/// What one [`run_calls`] phase produced.
struct Phase {
    /// The call loops' accumulated wall time (**setup excluded**).
    elapsed: Duration,
    /// Off-heap payloads the dropped machines held at their last measured call.
    refc_payloads: u64,
    /// The last call's raw result term.
    result: Eterm,
}

/// Builds the argument term a batch's timed calls reuse.
///
/// It runs **once per batch, before the timed loop**, so fixture setup is never charged to
/// the engine; the [`Counters`] are parked across it (see [`run_calls`]), so a fixture that
/// builds its own argument *through the engine* contributes nothing to the tally either.
type Prepare<'a> = &'a dyn Fn(&mut CountingContext, &mut Machine) -> beam_common::Result<Eterm>;

/// The `cowboy_bstr:to_lower/1` fixture's argument: one 30-byte heap binary from [`INPUT`],
/// built directly through the inner host (no engine run, so no counter traffic at all).
fn bstr_input(ctx: &mut CountingContext, _machine: &mut Machine) -> beam_common::Result<Eterm> {
    make_heap_binary(ctx.inner_mut(), INPUT)
}

/// Run `calls` calls in batches of `batch`, each batch with a fresh [`Machine`] and a fresh
/// inner host, and return the time the **call loops** took.
///
/// The reused input term is built once per batch by `prepare`, so fixture setup is
/// not charged to the engine and not included in the timing, and the batch's [`Machine`] is
/// dropped before the next one starts — `Machine::refc_binaries` and the host's region heap
/// never give memory back, so a single long-lived pair would measure the harness' own growth.
fn run_calls(
    module: &Arc<Module>,
    entry: Atom,
    tier: Tier,
    calls: usize,
    batch: usize,
    ctx: &mut CountingContext,
    prepare: Prepare<'_>,
) -> beam_common::Result<Phase> {
    let batch = batch.max(1);
    let mut elapsed = Duration::ZERO;
    let mut refc_payloads = 0u64;
    let mut result = NIL;
    let mut remaining = calls;
    while remaining > 0 {
        let n = remaining.min(batch);
        ctx.reset_inner();
        let mut machine = Machine::new();
        // The argument term is fixture setup. `prepare` may enter the engine (a fixture that
        // builds its own input), so the whole counter set is parked across it: the tally
        // below is the timed calls only.
        let parked = std::mem::take(&mut ctx.counters);
        let built = prepare(ctx, &mut machine);
        ctx.counters = parked;
        let input = built?;
        // Payloads the `prepare` itself created (a fixture-built input) are not the timed
        // calls' cost, so the batch's payload count starts here.
        let payloads_before = machine.refc_binaries().len();
        let start = Instant::now();
        for _ in 0..n {
            match machine.run_export_outcome_with_tier(module, entry, 1, &[input], ctx, tier)? {
                RunOutcome::Returned(value) => result = value,
                RunOutcome::Wait { .. } => {
                    panic!("{} blocked in a receive: this fixture has none", tier.name())
                }
            }
        }
        elapsed += start.elapsed();
        // Every batch starts from a fresh machine, so the growth of its payload roster is
        // exactly the number of off-heap payloads the batch's timed calls created.
        refc_payloads += (machine.refc_binaries().len() - payloads_before) as u64;
        remaining -= n;
    }
    Ok(Phase {
        elapsed,
        refc_payloads,
        result,
    })
}

/// The interpreter's instruction count over `calls` calls (for `BENCH_TIER=baseline`).
fn interpreter_instruction_count(
    module: &Arc<Module>,
    entry: Atom,
    calls: usize,
    prepare: Prepare<'_>,
) -> (u64, usize) {
    let mut ctx = CountingContext::new(Arc::clone(module));
    run_calls(module, entry, Tier::Interpreter, calls, calls, &mut ctx, prepare)
        .expect("the interpreter counting pass");
    (ctx.counters.instructions, calls.max(1))
}

/// The per-call **interpreter** instruction count for one prepared argument.
///
/// The count is a property of the code, not of the tier (the native tier offers the same
/// reduction hook once per driver-loop iteration, not once per instruction), so a run that
/// selected only the baseline tier takes a short interpreter-only counting pass instead.
fn instr_per_call(
    measurement: Option<&Measurement>,
    module: &Arc<Module>,
    entry: Atom,
    batch: usize,
    prepare: Prepare<'_>,
) -> f64 {
    match measurement {
        Some(measurement) => {
            measurement.counters.instructions as f64 / measurement.iters.max(1) as f64
        }
        None => {
            println!(
                "note = the instruction count came from a short interpreter counting pass \
                 (BENCH_TIER did not select the interpreter)"
            );
            let (instructions, calls) = interpreter_instruction_count(module, entry, batch, prepare);
            instructions as f64 / calls as f64
        }
    }
}

/// One workload's report labelling: what was measured, at which granularity, and what the
/// result must be.
///
/// Every reported block keeps the same key set; a workload only chooses its `workload = …`
/// line, whether it also reports **per unit** (per element, say) and how its result is checked.
struct ReportSpec<'a> {
    /// The `workload = …` line: what this block measured.
    workload: &'a str,
    /// The per-unit normalisation, when the workload has a natural unit smaller than a call.
    unit: Option<Unit>,
    /// The correctness predicate over the measurement's captured result.
    expect: &'a dyn Fn(&Measurement) -> bool,
    /// What the predicate expects, for the assertion message.
    expected: String,
    /// `true` when the workload must dispatch **no** BIF at all (a BIF-free program).
    bif_free: bool,
}

/// A workload's per-unit normalisation: `per_call` units are performed by one call.
#[derive(Clone, Copy)]
struct Unit {
    /// The unit's name, used in the `*_per_<label>` keys (`element`, …).
    label: &'static str,
    /// How many units one call performs.
    per_call: f64,
}

/// Print one tier's `key = value` table (the `cowboy_bstr:to_lower/1` workload).
fn report(measurement: &Measurement, tier: Tier, instr_per_call: f64) {
    let expect = |measurement: &Measurement| {
        measurement.snapshot == ResultSnapshot::Binary(EXPECTED.to_vec())
    };
    let spec = ReportSpec {
        workload: "cowboy_bstr:to_lower/1",
        unit: None,
        expect: &expect,
        expected: format!("{:?}", String::from_utf8_lossy(EXPECTED)),
        bif_free: true,
    };
    report_generic(measurement, tier, instr_per_call, &spec);
}

/// Both tiers must produce the same result for the same input — the engine's tier-equivalence
/// contract, here on a foreign module rather than a committed fixture.
fn assert_tiers_agree(first: &Measurement, second: &Measurement, workload: &str) {
    assert_eq!(
        first.snapshot, second.snapshot,
        "the two tiers disagree on {workload}"
    );
}

/// Print one tier's `key = value` table for `spec`'s workload.
fn report_generic(
    measurement: &Measurement,
    tier: Tier,
    instr_per_call: f64,
    spec: &ReportSpec<'_>,
) {
    let calls = measurement.iters.max(1) as f64;
    let per_call = |count: u64| count as f64 / calls;
    let min = measurement.min();
    let median = measurement.median();
    let counters = &measurement.counters;
    let correct = (spec.expect)(measurement);
    println!("---");
    println!("workload = {}", spec.workload);
    println!("tier = {}", tier.name());
    println!("iters = {}", measurement.iters);
    println!("min_ns_per_call = {min:.1}");
    println!("median_ns_per_call = {median:.1}");
    println!("instr_per_call = {instr_per_call:.2}");
    println!(
        "ns_per_instruction = {:.2}",
        if instr_per_call > 0.0 {
            min / instr_per_call
        } else {
            0.0
        }
    );
    println!("rooted_calls_per_call = {:.2}", per_call(counters.rooted_calls));
    println!("rooted_words_per_call = {:.2}", per_call(counters.rooted_words));
    println!("heap_door_calls_per_call = {:.2}", per_call(counters.heap_calls));
    println!("heap_door_words_per_call = {:.2}", per_call(counters.heap_words));
    println!("bif_calls_per_call = {:.2}", per_call(counters.bif_calls));
    println!("apply_calls_per_call = {:.2}", per_call(counters.apply_calls));
    println!("apply_fun_calls_per_call = {:.2}", per_call(counters.apply_fun_calls));
    println!("external_call_requests_per_call = {:.2}", per_call(counters.external_call_requests));
    println!("local_fun_requests_per_call = {:.2}", per_call(counters.local_fun_requests));
    println!("local_call_requests_per_call = {:.2}", per_call(counters.local_call_requests));
    println!("bif_floor_declines_per_call = {:.2}", per_call(counters.floor_declines));
    let (word_pairs, cmp_pairs) = measurement.select_answers;
    println!(
        "select_pairs_by_word_per_call = {:.2}",
        per_call(word_pairs as u64)
    );
    println!(
        "select_pairs_by_cmp_per_call = {:.2}",
        per_call(cmp_pairs as u64)
    );
    println!("refc_payloads_per_call = {:.2}", per_call(measurement.refc_payloads));
    println!("result_is_correct = {correct}");
    if let Some(unit) = &spec.unit {
        let units = unit.per_call.max(1.0);
        let label = unit.label;
        println!("{label}s_per_call = {units:.0}");
        println!("ns_per_{label} = {:.2}", min / units);
        println!("median_ns_per_{label} = {:.2}", median / units);
        println!("instr_per_{label} = {:.3}", instr_per_call / units);
        println!("rooted_calls_per_{label} = {:.4}", per_call(counters.rooted_calls) / units);
        println!("rooted_words_per_{label} = {:.4}", per_call(counters.rooted_words) / units);
        println!(
            "heap_door_calls_per_{label} = {:.4}",
            per_call(counters.heap_calls) / units
        );
        println!("bif_calls_per_{label} = {:.4}", per_call(counters.bif_calls) / units);
        println!(
            "local_call_requests_per_{label} = {:.4}",
            per_call(counters.local_call_requests) / units
        );
        println!(
            "note = those per-{label} figures divide the WHOLE call by {units:.0} {label}s, so \
             a per-call setup cost is"
        );
        println!(
            "note = included; the marginal block (a paired K-call pass minus a 0-call pass) is \
             the build-cancelled figure."
        );
    }
    // The dispatch path keys the histogram allocation-free (`counting::BifHistogram`); the
    // `module:function/arity` display strings are built here, once, at report time.
    let histogram = counters.bif_histogram.to_map();
    let histogram = ranked(&histogram, TOP_BIFS);
    if counters.bif_histogram.is_empty() {
        println!("note = bif_histogram is empty: the workload dispatched no BIF at all");
    }
    for (name, count) in histogram {
        println!("bif_histogram[{name}] = {count}");
    }

    assert!(
        correct,
        "{} on {} returned {:?}, not {}",
        spec.workload,
        tier.name(),
        measurement.snapshot,
        spec.expected
    );
    if spec.bif_free {
        assert_eq!(
            counters.bif_calls, 0,
            "{} dispatched BIF call(s) on {} -- this workload is BIF-free by construction (see \
             the histogram above)",
            counters.bif_calls,
            tier.name()
        );
    }
    // A decline means the harness' own BIF floor recognised a call and refused its operands, so
    // the figure above was produced with a member of that set unavailable: the correctness
    // predicate may not have noticed (a guard failure can change a branch without changing the
    // shape of the answer), so it is asserted here too.
    assert_eq!(
        counters.floor_declines, 0,
        "the harness' BIF floor declined {} call(s) on {} -- the figures above are not reportable \
         (see bif_histogram and the module docs of `counting`)",
        counters.floor_declines,
        tier.name()
    );
}

/// The counting-only baseline pass: the existing per-thread census over a **freshly loaded**
/// module.
///
/// Two thread-local resources shape this pass. The per-thread block cache would answer a
/// second measurement of the *same* `Module` from its already-compiled artifact — so the
/// static `instr_instances`/`instr_deopts` census would be empty, because the counters were
/// off when it compiled. A second [`load_file`] mints a fresh atom-table identity, which is
/// half the cache key, so this pass compiles the module again with recording on and the
/// census is complete; and because the counters are reset immediately before it, every figure
/// below belongs to this pass alone.
///
/// `unit` adds a per-unit normalisation (the `element` rows of
/// [`list_walk::list_walk_decomposition`](list_walk)) beside the per-call ones; the foreign
/// cases pass `None` because one call is their only natural unit.
fn baseline_census(
    beam: &Path,
    entry_name: &str,
    iters: usize,
    batch: usize,
    prepare: Prepare<'_>,
    unit: Option<&Unit>,
) {
    use crate::jit::baseline::counters;

    let module = Arc::new(
        load_file(beam).unwrap_or_else(|error| panic!("reload {}: {error:?}", beam.display())),
    );
    let entry = atom_named(&module, entry_name);
    let mut ctx = CountingContext::new(Arc::clone(&module));
    counters::set_enabled(true);
    counters::reset();
    let phase = run_calls(
        &module,
        entry,
        Tier::BaselineJit,
        iters,
        batch,
        &mut ctx,
        prepare,
    )
    .expect("the baseline census pass");
    counters::set_enabled(false);
    print_census(counters::snapshot(), iters.max(1), &phase, unit);
}

/// Print the baseline census snapshot as one `key = value` block.
fn print_census(
    snapshot: crate::jit::baseline::counters::Counters,
    iters: usize,
    phase: &Phase,
    unit: Option<&Unit>,
) {
    let calls = iters as f64;
    let per_call = |count: u64| count as f64 / calls;
    let min_ns = ns_per_call(phase.elapsed, iters);
    let block_entry_misses = snapshot
        .block_entry_lookups
        .saturating_sub(snapshot.block_entry_hits);
    println!("---");
    println!("tier = {}-census", Tier::BaselineJit.name());
    println!("iters = {iters}");
    println!("census_min_ns_per_call = {min_ns:.1}");
    println!("step_one_per_call = {:.2}", per_call(snapshot.step_one));
    println!("step_ext_per_call = {:.2}", per_call(snapshot.step_ext));
    println!("driver_round_trips_per_call = {:.2}", per_call(snapshot.driver_round_trips));
    println!("driver_interp_steps_per_call = {:.2}", per_call(snapshot.driver_interp_steps));
    // The in-block call/return link path: a frame-preserving local `call` the block tail-jumped
    // into a natively compiled callee (a driver round-trip the tier did **not** pay), and the
    // declines that paid the pre-existing round-trip anyway. They explain the
    // `driver_round_trips_per_call` figure beside them.
    println!("in_block_calls_per_call = {:.2}", per_call(snapshot.in_block_calls));
    println!(
        "in_block_call_declines_per_call = {:.2}",
        per_call(snapshot.in_block_call_declines)
    );
    println!("block_entry_lookups = {}", snapshot.block_entry_lookups);
    println!("block_entry_hits = {}", snapshot.block_entry_hits);
    println!("block_entry_misses = {block_entry_misses}");
    if let Some(unit) = unit {
        // The same driver/block figures per unit of work, so a workload that is not
        // call-shaped (a K-walk, 1000-element traversal) can be compared with the other
        // workloads' per-element rows without re-deriving the division by hand.
        let units = unit.per_call.max(1.0);
        let label = unit.label;
        println!("{label}s_per_call = {units:.0}");
        println!("census_ns_per_{label} = {:.3}", min_ns / units);
        println!("step_one_per_{label} = {:.4}", per_call(snapshot.step_one) / units);
        println!("step_ext_per_{label} = {:.4}", per_call(snapshot.step_ext) / units);
        println!(
            "driver_round_trips_per_{label} = {:.4}",
            per_call(snapshot.driver_round_trips) / units
        );
        println!(
            "driver_interp_steps_per_{label} = {:.4}",
            per_call(snapshot.driver_interp_steps) / units
        );
        println!(
            "in_block_calls_per_{label} = {:.4}",
            per_call(snapshot.in_block_calls) / units
        );
        println!(
            "in_block_call_declines_per_{label} = {:.4}",
            per_call(snapshot.in_block_call_declines) / units
        );
        println!(
            "block_entry_lookups_per_{label} = {:.4}",
            per_call(snapshot.block_entry_lookups) / units
        );
        println!(
            "block_entry_hits_per_{label} = {:.4}",
            per_call(snapshot.block_entry_hits) / units
        );
        println!(
            "block_entry_misses_per_{label} = {:.6}",
            per_call(block_entry_misses) / units
        );
        println!(
            "note = those divide the WHOLE census pass by {units:.0} {label}s, so a per-call \
             setup cost is included."
        );
    }
    for (variant, count) in ranked(&snapshot.deopts_by_variant, TOP_ROWS) {
        println!("deopt_count[{variant}] = {count}");
    }
    if snapshot.deopts_by_variant.is_empty() {
        println!("note = deopt_count is empty: the native tier lowered every executed instance");
    }
    println!(
        "note = emitted_instance_count[...] is Counters::instr_instances, the **static** \
         per-variant census"
    );
    println!(
        "note = (instances the emitter visited during this pass' compile), not a dynamic \
         execution count."
    );
    for (variant, count) in ranked(&snapshot.instr_instances, TOP_ROWS) {
        println!("emitted_instance_count[{variant}] = {count}");
    }
}

/// The `rows` highest-count entries of `map`, ties broken by name so the output is stable.
fn ranked(map: &BTreeMap<String, u64>, rows: usize) -> Vec<(String, u64)> {
    let mut ranked: Vec<(String, u64)> = map
        .iter()
        .map(|(name, count)| (name.clone(), *count))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(rows);
    ranked
}

/// Nanoseconds per call.
fn ns_per_call(elapsed: Duration, calls: usize) -> f64 {
    elapsed.as_secs_f64() * 1e9 / calls.max(1) as f64
}

/// The atom of the export `name` in `module`; a module that does not export it is a panic,
/// because every workload names an export its own fixture (or its staged foreign source) has.
fn atom_named(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("the loaded module exports {name:?}"))
}

/// `usize` env knob; an unset, unparsable or zero value falls back to `default`.
fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// The tiers `BENCH_TIER` selects, in canonical order (interpreter first).
fn selected_tiers() -> Vec<Tier> {
    match std::env::var("BENCH_TIER").unwrap_or_default().trim() {
        "interpreter" => vec![Tier::Interpreter],
        "baseline" => vec![Tier::BaselineJit],
        "both" | "" => vec![Tier::Interpreter, Tier::BaselineJit],
        other => {
            println!("note = BENCH_TIER={other:?} is not interpreter|baseline|both -- using both");
            vec![Tier::Interpreter, Tier::BaselineJit]
        }
    }
}

/// Write `source` as `<module>.erl` into a unique temp directory and compile it with the
/// system `erlc`.
///
/// `tag` only distinguishes one workload's temp directory from another's.
/// `Err` means "skip": no spawnable `erlc`, or an `erlc` that refused the (valid) source — the
/// caller prints the reason and returns, which is this crate's skip-loudly convention.
fn compile_fixture(module: &str, source: &str, tag: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("beam_engine_bench_{tag}_{}", std::process::id()));
    let path = dir.join(format!("{module}.erl"));
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    std::fs::write(&path, source)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    run_erlc(&dir, &path)?;
    Ok(dir.join(format!("{module}.beam")))
}

/// Compile a module **staged from an existing source file** (the read-only foreign checkouts)
/// into a unique temp directory with the system `erlc`.
///
/// The [`compile_fixture`] sibling: the source is *copied*, never rewritten or embedded, so the
/// workload measures exactly the foreign project's own code. `Err` means "skip", with the same
/// two causes (no spawnable `erlc`, or an `erlc` that refused the source).
fn compile_staged(module: &str, source: &Path, tag: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("beam_engine_bench_{tag}_{}", std::process::id()));
    let path = dir.join(format!("{module}.erl"));
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    std::fs::copy(source, &path).map_err(|error| {
        format!(
            "copy {} -> {}: {error}",
            source.display(),
            path.display()
        )
    })?;
    run_erlc(&dir, &path)?;
    Ok(dir.join(format!("{module}.beam")))
}

/// Run `erlc +deterministic -o dir path`; the shared compile step of [`compile_fixture`] and
/// [`compile_staged`].
fn run_erlc(dir: &Path, path: &Path) -> Result<(), String> {
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("-o")
        .arg(dir)
        .arg(path)
        .output()
        .map_err(|error| format!("no usable `erlc` on PATH ({error})"))?;
    if !output.status.success() {
        return Err(format!(
            "`erlc` exited {} on {}: {}",
            output.status,
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The `select_val` ladder's structural gate
// ---------------------------------------------------------------------------

/// **Every** `select_val` pair on `cowboy_bstr:to_lower/1` is decided without a general
/// term comparison — the structural counter the ladder in `src/interpreter/select.rs`
/// is gated on.
///
/// `char_to_lower/1` is a pure-Erlang 27-clause function whose body is one `select_val`
/// over 26 small-int alternatives (`$A`/`$B`/…/`$Z`), so a call scans them by word
/// identity alone: the candidates are `Operand::Integer` constants, i.e. **unboxed**
/// alternates (no term is built, no `cmp` is entered). Before the ladder existed the
/// same call built one `Eterm` per candidate and ran the full `beam_term::compare`
/// ordering between every pair; this asserts that no pair takes that path any more.
///
/// The tally is deterministic, so this is a **gate**, not a measurement; it skips loudly
/// when the system `erlc` (which compiles the fixture) is unavailable.
#[test]
fn every_bstr_select_val_pair_is_decided_without_a_general_compare() {
    let beam = match compile_fixture(MODULE, SOURCE, "bstr_ladder") {
        Ok(beam) => beam,
        Err(why) => {
            println!("SKIP: {why} -- the select_val ladder gate did not run");
            return;
        }
    };
    let module = Arc::new(
        load_file(&beam).unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display())),
    );
    let entry = atom_named(&module, ENTRY);

    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let mut ctx = CountingContext::new(Arc::clone(&module));
        let mut machine = Machine::new();
        let input = bstr_input(&mut ctx, &mut machine).expect("the 30-byte argument");
        crate::interpreter::reset_select_answers();
        let outcome = machine
            .run_export_outcome_with_tier(&module, entry, 1, &[input], &mut ctx, tier)
            .unwrap_or_else(|error| panic!("{ENTRY}/1 on {}: {error:?}", tier.name()));
        let (word_pairs, cmp_pairs) = crate::interpreter::select_answers();
        let RunOutcome::Returned(value) = outcome else {
            panic!("{} blocked in a receive: this fixture has none", tier.name())
        };
        assert_eq!(
            ResultSnapshot::of(value),
            ResultSnapshot::Binary(EXPECTED.to_vec()),
            "{} produced the wrong bytes",
            tier.name()
        );
        println!(
            "select_val_ladder[{}] = word {word_pairs}, cmp {cmp_pairs} (one to_lower/1 call)",
            tier.name()
        );
        assert_eq!(
            cmp_pairs, 0,
            "{} paid {cmp_pairs} general term comparison(s) on a small-int switch",
            tier.name()
        );
        if tier == Tier::Interpreter {
            // 30 bytes × a 26-alternative scan that runs to the catch-all clause for the
            // lowercase bytes, so the count is large and, above all, non-zero: the pairs are
            // answered, just not by `cmp`.
            assert!(
                word_pairs > 0,
                "the interpreter answered no pair by word identity: the ladder did not engage"
            );
        }
    }
}