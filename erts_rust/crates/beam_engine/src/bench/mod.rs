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
//! reported. `Machine::refc_binaries` and the inner host's region heap grow monotonically and
//! are never given back, so one 20 000-call machine would leak roughly 600k payloads instead of
//! measuring anything.
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

mod counting;
mod long_append;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_loader::{load_file, Module};
use beam_term::{Atom, Eterm, NIL};

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
    println!("note = instr_per_call is the INTERPRETER's reduce-hook count; the native baseline");
    println!("note = tier makes no reduce call at all, so the same figure is reported for both");
    println!("note = tiers (it is a property of the code, not of the tier).");
    println!("note = every *_per_call count is deterministic (a host-side tally); min/median");
    println!("note = ns_per_call are wall clock on a shared host -- compare ratios, not absolutes.");

    let mut measurements = Vec::new();
    for tier in &tiers {
        measurements.push((
            *tier,
            measure(
                &module,
                entry_atom(&module),
                *tier,
                iters,
                batch,
                warmup,
                &bstr_input,
            ),
        ));
    }
    // The instruction count is a property of the code, not of the tier (see the header note).
    let instr_per_call = match measurements
        .iter()
        .find(|(tier, _)| *tier == Tier::Interpreter)
    {
        Some((_, measurement)) => {
            measurement.counters.instructions as f64 / measurement.iters.max(1) as f64
        }
        None => {
            if tiers.contains(&Tier::BaselineJit) {
                println!(
                    "note = the instruction count came from a short interpreter counting pass \
                     (BENCH_TIER did not select the interpreter)"
                );
            }
            let (instructions, calls) =
                interpreter_instruction_count(&module, batch, &bstr_input);
            instructions as f64 / calls as f64
        }
    };

    for (tier, measurement) in &measurements {
        report(measurement, *tier, instr_per_call);
    }
    if let [(_, first), (_, second)] = measurements.as_slice() {
        assert_tiers_agree(first, second);
    }
    if tiers.contains(&Tier::BaselineJit) {
        baseline_census(&beam, iters, batch);
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
    result: Eterm,
}

impl Measurement {
    /// The fastest sub-run (the robust statistic under additive background load).
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
fn measure(
    module: &Arc<Module>,
    entry: Atom,
    tier: Tier,
    iters: usize,
    batch: usize,
    warmup: usize,
    prepare: Prepare<'_>,
) -> Measurement {
    // Warmup runs on its own context, so none of its traffic lands in the phase's counters.
    let mut warm = CountingContext::new(Arc::clone(module));
    run_calls(module, entry, tier, warmup, batch, &mut warm, prepare)
        .unwrap_or_else(|error| panic!("warmup on {}: {error:?}", tier.name()));
    drop(warm);

    let per_run = measured_calls(iters);
    let mut ctx = CountingContext::new(Arc::clone(module));
    let mut samples = Vec::with_capacity(SUB_RUNS);
    let mut refc_payloads = 0u64;
    let mut result = NIL;
    for _ in 0..SUB_RUNS {
        let phase = run_calls(module, entry, tier, per_run, batch, &mut ctx, prepare)
            .unwrap_or_else(|error| panic!("measure on {}: {error:?}", tier.name()));
        samples.push(ns_per_call(phase.elapsed, per_run));
        refc_payloads += phase.refc_payloads;
        result = phase.result;
    }
    Measurement {
        iters: per_run * SUB_RUNS,
        samples,
        counters: ctx.counters,
        refc_payloads,
        result,
    }
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
    calls: usize,
    prepare: Prepare<'_>,
) -> (u64, usize) {
    let entry = entry_atom(module);
    let mut ctx = CountingContext::new(Arc::clone(module));
    run_calls(module, entry, Tier::Interpreter, calls, calls, &mut ctx, prepare)
        .expect("the interpreter counting pass");
    (ctx.counters.instructions, calls.max(1))
}

/// Print one tier's `key = value` table.
fn report(measurement: &Measurement, tier: Tier, instr_per_call: f64) {
    let calls = measurement.iters.max(1) as f64;
    let per_call = |count: u64| count as f64 / calls;
    let min = measurement.min();
    let counters = &measurement.counters;
    println!("---");
    println!("tier = {}", tier.name());
    println!("iters = {}", measurement.iters);
    println!("min_ns_per_call = {min:.1}");
    println!("median_ns_per_call = {:.1}", measurement.median());
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
    println!("refc_payloads_per_call = {:.2}", per_call(measurement.refc_payloads));
    println!("result_is_correct = {}", is_expected(measurement.result));
    let histogram = ranked(&counters.bif_histogram, TOP_BIFS);
    let empty = histogram.is_empty();
    for (name, count) in histogram {
        println!("bif_histogram[{name}] = {count}");
    }
    if empty {
        println!("note = bif_histogram is empty: the workload dispatched no BIF at all");
    }

    assert!(
        is_expected(measurement.result),
        "{} returned {:?}, not {:?}",
        tier.name(),
        measurement.result,
        String::from_utf8_lossy(EXPECTED)
    );
    if counters.bif_calls > 0 {
        println!(
            "surprise: {} dispatched {} BIF call(s) -- this workload is expected to be \
             engine-only (see the histogram above)",
            tier.name(),
            counters.bif_calls
        );
    }
}

/// Whether `result` is the byte-identical [`EXPECTED`] binary.
fn is_expected(result: Eterm) -> bool {
    binary_bytes(result)
        .map(|bytes| bytes == EXPECTED)
        .unwrap_or(false)
}

/// Both tiers must produce the same term for the same input — the engine's tier-equivalence
/// contract, here on a foreign module rather than a committed fixture.
fn assert_tiers_agree(first: &Measurement, second: &Measurement) {
    assert_eq!(
        binary_bytes(first.result).ok(),
        binary_bytes(second.result).ok(),
        "the two tiers disagree on {MODULE}:{ENTRY}/1"
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
fn baseline_census(beam: &Path, iters: usize, batch: usize) {
    use crate::jit::baseline::counters;

    let module = Arc::new(
        load_file(beam).unwrap_or_else(|error| panic!("reload {}: {error:?}", beam.display())),
    );
    let entry = entry_atom(&module);
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
        &bstr_input,
    )
    .expect("the baseline census pass");
    counters::set_enabled(false);
    print_census(counters::snapshot(), iters.max(1), &phase);
}

/// Print the baseline census snapshot as one `key = value` block.
fn print_census(snapshot: crate::jit::baseline::counters::Counters, iters: usize, phase: &Phase) {
    let calls = iters as f64;
    let per_call = |count: u64| count as f64 / calls;
    println!("---");
    println!("tier = {}-census", Tier::BaselineJit.name());
    println!("iters = {iters}");
    println!("census_min_ns_per_call = {:.1}", ns_per_call(phase.elapsed, iters));
    println!("step_one_per_call = {:.2}", per_call(snapshot.step_one));
    println!("step_ext_per_call = {:.2}", per_call(snapshot.step_ext));
    println!("driver_round_trips_per_call = {:.2}", per_call(snapshot.driver_round_trips));
    println!("driver_interp_steps_per_call = {:.2}", per_call(snapshot.driver_interp_steps));
    println!("block_entry_lookups = {}", snapshot.block_entry_lookups);
    println!("block_entry_hits = {}", snapshot.block_entry_hits);
    println!(
        "block_entry_misses = {}",
        snapshot
            .block_entry_lookups
            .saturating_sub(snapshot.block_entry_hits)
    );
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

/// The exported function's atom.
fn entry_atom(module: &Module) -> Atom {
    module
        .atom_table
        .lookup(ENTRY)
        .unwrap_or_else(|| panic!("the fixture exports {ENTRY:?}"))
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
fn compile_fixture(module: &str, source: &str, tag: &str) -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("beam_engine_bench_{tag}_{}", std::process::id()));
    let path = dir.join(format!("{module}.erl"));
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    std::fs::write(&path, source)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(&path)
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
    Ok(dir.join(format!("{module}.beam")))
}
