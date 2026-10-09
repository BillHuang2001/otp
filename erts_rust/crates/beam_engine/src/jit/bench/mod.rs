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

//! Owner-side interpreter-vs-baseline measurement harness for the opcode shapes the
//! integration-level `tests/jit_bench.rs` does **not** cover.
//!
//! `tests/jit_bench.rs` measures the arithmetic/register/comparison/aggregate-read
//! inlines (`fib`/`kernel`/`wide`/`aggr`/`cmp`). This harness adds the four shapes
//! whose lowering story is the *opposite* one — they are **not** in the baseline
//! tier's inline set, so every one of them is a `jit_step_one` helper round-trip on a
//! native tier:
//!
//! * **`build/0`** — a straight-line body of `put_tuple2` / `put_list` constructors.
//!   Neither constructor is inlined, so each one is one interpreter step on the
//!   baseline tier (plus one heap allocation in the context's arena).
//! * **`map/2`** — a body of the map instructions (`has_map_fields`,
//!   `get_map_elements`, `put_map_assoc`) over a flat map built once by the harness.
//!   All three are family instructions, i.e. deopts.
//! * **`select/1`** — a body dominated by `select_val` dispatch, deliberately mixing
//!   the **inlined** shape (immediate alternatives + a resolved fail label, lowered by
//!   `src/jit/baseline/emit/select.rs`) with the **deopted** shape (a register
//!   alternative, which `constant_word` refuses), so the measurement sees both.
//! * **`walk/2`** — a hot tail-recursive loop walking a cons list (an
//!   `is_nonempty_list` guard + `get_list` + a small-int `'+'/2` + `call_only`), i.e.
//!   control flow the straight-line `aggr/2` workload never exercises. The list is
//!   built by the harness (in the run's own context) so the timed loop is the walk only.
//!
//! # Method (copied from `tests/jit_bench.rs`, deliberately)
//!
//! Every workload is a hand-built synthetic [`Module`] assembled by the loader's own
//! block decomposition ([`beam_loader::lower::build_blocks`]), run on the shared
//! [`MinimalContext`] + a [`JitBackend`]. A native tier **compiles once** outside the
//! timed loop ([`JitBackend::compile`]) and the loop only ever calls
//! [`JitBackend::run_compiled`]; one warm-up run precedes the timed runs, and the same
//! [`Machine`] is reused across them (`run_*` resets it). `ns/op` is the **mean** over
//! the timed runs, and includes the per-run setup (`Machine::reset` + the label-map
//! build), so a workload whose body is shorter than that setup floors at the setup cost.
//!
//! # Caveats (read before quoting a number)
//!
//! * The sandbox is **shared and high-load**: these are wall-clock numbers with no CPU
//!   pinning, so treat them as order-of-magnitude evidence, not precise figures.
//! * These tiers are **correctness-first mechanisms, not optimisers** (Part II §F).
//!   Nothing here asserts that a native tier is faster — that is environment dependent
//!   and would be flaky. Every workload asserts only that the two tiers **agree** on
//!   the result, and that the result is the value the workload is supposed to compute
//!   (compared **structurally**, since the two machines have separate heaps).
//! * Both tiers allocate in the context's arena, which is never freed
//!   (`MinimalContext` has no collector), so a workload that builds terms per run grows
//!   RSS with the iteration count. The counts are chosen to keep that bounded.
//!
//! # Layout
//!
//! [`support`] owns the scaffolding both measurement harnesses share (the synthetic
//! [`Module`] builder, the arithmetic BIF hook and the ns/op report primitives);
//! [`workloads`] owns the workload definitions (the synthetic modules, their argument
//! preparation and their result-shape checks); this file owns the measurement harness
//! (the tier seam, the timing loop and the reporting).
//!
//! The timing test is `#[ignore]`d (run it with `--ignored --nocapture`); the agreement
//! test is not, so a plain `cargo test -p beam_engine` still checks that every workload
//! behaves identically on both tiers.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use beam_common::Result;
use beam_loader::Module;
use beam_term::{cmp, make_small, Atom, Eterm};

use crate::context::MinimalContext;
use crate::jit::baseline::{BaselineJit, CompiledModule};
#[cfg(feature = "jit-cranelift")]
use crate::jit::cranelift::{CompiledFunctions, CraneliftJit};
use crate::jit::{available_tiers, JitBackend, Tier};
use crate::machine::Machine;

mod checks;
/// The scaffolding shared with the sibling Cranelift bench (`crate::jit::cranelift::bench`).
pub(crate) mod support;
mod workloads;

use support::{arith_hook, ns_per_op, ops_per_sec};
use workloads::{workloads, Workload};

// ---------------------------------------------------------------------------
// The tiers
// ---------------------------------------------------------------------------

/// A tier bound to an artifact compiled **once** (except the interpreter, which has
/// nothing to compile).
enum Runner {
    /// The direct-threaded interpreter: `Machine::run_export`.
    Interpreter,
    /// The native baseline tier over its compiled blocks.
    Baseline {
        /// The (zero-sized) backend that owns the compilation.
        backend: BaselineJit,
        /// Its already-compiled artifact.
        compiled: CompiledModule,
    },
    /// The optional Cranelift hot tier (feature-gated).
    #[cfg(feature = "jit-cranelift")]
    Cranelift {
        /// The Cranelift backend that owns the compilation.
        backend: CraneliftJit,
        /// Its already-compiled artifact.
        compiled: CompiledFunctions,
    },
}

impl Runner {
    /// Prepare `tier` for `module`, compiling once and reporting that one-time cost.
    ///
    /// Only called for a tier [`Tier::is_available`] reports.
    fn prepare(tier: Tier, module: &Module) -> Result<(Runner, Option<Duration>)> {
        match tier {
            Tier::Interpreter => Ok((Runner::Interpreter, None)),
            Tier::BaselineJit => {
                let mut backend = BaselineJit::new();
                let start = Instant::now();
                let compiled = backend.compile(module)?;
                let compile = start.elapsed();
                Ok((Runner::Baseline { backend, compiled }, Some(compile)))
            }
            #[cfg(feature = "jit-cranelift")]
            Tier::Cranelift => {
                let mut backend = CraneliftJit::new();
                let start = Instant::now();
                let compiled = backend.compile(module)?;
                let compile = start.elapsed();
                Ok((Runner::Cranelift { backend, compiled }, Some(compile)))
            }
            #[cfg(not(feature = "jit-cranelift"))]
            Tier::Cranelift => Err(crate::error::unsupported("jit-cranelift tier not built")),
        }
    }

    /// Run exported `function/arity` once, against the prepared artifact.
    fn run(
        &self,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut MinimalContext,
    ) -> Result<Eterm> {
        match self {
            Runner::Interpreter => machine.run_export(module, function, arity, args, ctx),
            Runner::Baseline { backend, compiled } => {
                backend.run_compiled(compiled, machine, module, function, arity, args, ctx)
            }
            #[cfg(feature = "jit-cranelift")]
            Runner::Cranelift { backend, compiled } => {
                backend.run_compiled(compiled, machine, module, function, arity, args, ctx)
            }
        }
    }
}

/// The tiers to measure: every tier this build reports as available, interpreter first.
fn benchmarked_tiers() -> Vec<Tier> {
    Tier::ALL
        .iter()
        .copied()
        .filter(|tier| tier.is_available())
        .collect()
}

// ---------------------------------------------------------------------------
// The timing harness
// ---------------------------------------------------------------------------

/// One `(workload, tier)` measurement.
struct Measurement {
    /// The timed tier.
    tier: Tier,
    /// The number of timed runs.
    runs: u32,
    /// The total wall-clock time of the timed runs (warm-up excluded).
    elapsed: Duration,
    /// The one-time [`JitBackend::compile`] latency behind the artifact the timed loop
    /// ran against; `None` for the interpreter. Excluded from `elapsed`.
    compile: Option<Duration>,
    /// The last run's result.
    result: Eterm,
    /// The context the run used — it **owns** the arena `result` points into, so it
    /// must outlive every read of `result` (the tiers have separate heaps). Never read
    /// directly: keeping the owner alive *is* the point (the arena's chunks are boxed,
    /// so moving this struct does not move them).
    _context: MinimalContext,
}

impl Measurement {
    /// A measurement of `runs` timed runs of `workload` on `tier`.
    fn run(tier: Tier, workload: &Workload, runs: u32) -> Measurement {
        let mut machine = Machine::new();
        let mut context = MinimalContext::with_bif_hook(arith_hook(None, None));
        let args = workload
            .arguments(&mut context)
            .expect("the workload's arguments must build");
        let (runner, compile) =
            Runner::prepare(tier, &workload.module).expect("the workload must prepare on the tier");

        // The warm-up run: not timed, and the first execution through the artifact.
        runner
            .run(
                &mut machine,
                &workload.module,
                workload.function,
                workload.arity,
                &args,
                &mut context,
            )
            .expect("the warm-up run must succeed");

        let start = Instant::now();
        let mut result = make_small(0);
        for _ in 0..runs {
            result = runner
                .run(
                    &mut machine,
                    &workload.module,
                    workload.function,
                    workload.arity,
                    &args,
                    &mut context,
                )
                .expect("every timed run must succeed");
        }
        let elapsed = start.elapsed();

        Measurement {
            tier,
            runs,
            elapsed,
            compile,
            result,
            _context: context,
        }
    }
}

/// Print one measurement line and return its `ns/op`.
fn report(measurement: &Measurement) -> f64 {
    let per_op_ns = ns_per_op(measurement.elapsed, measurement.runs);
    println!(
        "jit_bench | tier={:<12} runs={:<6} total={:>9.3} ms | {:>14.1} ns/op | {:>14.1} ops/s",
        measurement.tier.name(),
        measurement.runs,
        measurement.elapsed.as_secs_f64() * 1e3,
        per_op_ns,
        ops_per_sec(per_op_ns),
    );
    if let Some(compile) = measurement.compile {
        println!(
            "jit_bench |   {}: one-time compile = {:.3} us (excluded from the timed loop; \
             the loop is {} run_compiled calls against that artifact)",
            measurement.tier.name(),
            compile.as_secs_f64() * 1e6,
            measurement.runs,
        );
    }
    per_op_ns
}

/// Measure `workload` on every available tier, **assert tier agreement**, print the
/// table. Never asserts that a native tier is faster (see the module docs).
fn bench_workload(workload: &Workload) {
    let tiers = benchmarked_tiers();
    println!(
        "jit_bench | workload = {} | tiers = {:?}",
        workload.label,
        tiers.iter().map(Tier::name).collect::<Vec<_>>()
    );

    // The measurements are kept alive so their contexts (and therefore their heap
    // terms) outlive the cross-tier comparison below.
    let mut kept: Vec<Measurement> = Vec::new();
    let mut interpreter_ns = 0.0f64;
    for tier in tiers {
        assert!(
            available_tiers().contains(&tier.name()),
            "benchmarked tier {} must be in the build roster",
            tier.name()
        );
        let measurement = Measurement::run(tier, workload, workloads::TIMED_RUNS);
        let ns = report(&measurement);

        match kept.first() {
            None => {
                interpreter_ns = ns;
                workload
                    .verify(measurement.result)
                    .unwrap_or_else(|msg| panic!("{}: {msg}", workload.label));
            }
            Some(first) => {
                assert!(
                    cmp(measurement.result, first.result, &workload.module.atom_table)
                        == Ordering::Equal,
                    "tier {} disagrees with {} on {}",
                    tier.name(),
                    first.tier.name(),
                    workload.label,
                );
                let speedup = if ns > 0.0 {
                    interpreter_ns / ns
                } else {
                    f64::INFINITY
                };
                println!(
                    "jit_bench |   speed-up {} over interpreter: {:.2}x",
                    tier.name(),
                    speedup,
                );
            }
        }
        kept.push(measurement);
    }
    println!();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Every workload runs **identically** on the interpreter and on every native tier.
///
/// Deliberately not timing-sensitive: this is the correctness half, kept in the
/// ordinary test run so the default `cargo test -p beam_engine` covers it.
#[test]
fn the_interpreter_and_the_baseline_tier_agree_on_every_workload() {
    for workload in workloads() {
        for tier in benchmarked_tiers() {
            let measurement = Measurement::run(tier, &workload, workloads::AGREEMENT_RUNS);
            workload
                .verify(measurement.result)
                .unwrap_or_else(|msg| panic!("{} on {}: {msg}", workload.label, tier.name()));
        }

        // Cross-tier agreement, structurally (the tiers have separate heaps).
        let mut kept: Vec<Measurement> = Vec::new();
        for tier in benchmarked_tiers() {
            let measurement = Measurement::run(tier, &workload, workloads::AGREEMENT_RUNS);
            if let Some(first) = kept.first() {
                assert!(
                    cmp(measurement.result, first.result, &workload.module.atom_table)
                        == Ordering::Equal,
                    "tier {} disagrees with {} on {}",
                    tier.name(),
                    first.tier.name(),
                    workload.label,
                );
            }
            kept.push(measurement);
        }
        println!("jit_bench | {} agrees on every tier", workload.label);
    }
}

/// The measurement table: `ns/op` + `ops/s` per `(workload, tier)`, plus the speed-up
/// over the interpreter.
///
/// `#[ignore]`d because it is a benchmark, not a unit test:
/// `... -p beam_engine --lib -- --ignored --nocapture`. One test (not several) so the
/// workload timings never run concurrently and contaminate each other.
#[test]
#[ignore = "measurement table: run with --ignored --nocapture"]
fn interpreter_versus_native_tiers_measurement_table() {
    for workload in workloads() {
        bench_workload(&workload);
    }
}
