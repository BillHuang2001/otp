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

//! The engine's interpreter-vs-baseline tier measurement harness, over **every**
//! measured workload.
//!
//! This module is the single home for the JIT benchmark: the tier seam, the timing
//! loop, the reporting (here), the workload programs ([`workloads`], [`inlined`]), the
//! per-workload result checks ([`checks`]) and the shared scaffolding ([`support`],
//! which the sibling Cranelift ABBA harness also consumes). The `#[test]` entry points
//! live in the integration test `tests/jit_bench.rs`, which calls
//! [`assert_tier_agreement`] (fast, in the ordinary suite) and
//! [`run_measurement_table`] (the `#[ignore]`d table). Keeping the harness here is what
//! stops the unit-test and integration-test halves from growing a copy each.
//!
//! Twelve workloads, split by lowering story:
//!
//! * The shapes the baseline tier **deopts** (every instruction is a `jit_step_one`
//!   helper round-trip on a native tier) — **`build/0`** (`put_tuple2`/`put_list`),
//!   **`map/2`** (`has_map_fields`/`get_map_elements`/`put_map_assoc`), **`select/1`**
//!   (a body dominated by `select_val`, deliberately mixing the inlined immediate shape
//!   with the deopted register shape), **`walk/2`** (a hot tail-recursive cons-list
//!   walk: an `is_nonempty_list` guard + `get_list` + a small-int `'+'/2` + `call_only`)
//!   and **`sum/1`** (the **non**-tail-recursive cons-list sum: the caller's frame — and
//!   therefore a Y slot holding the head — stays live across the recursive call, so the
//!   `gc_bif '+'/2` reads a Y operand and the tier's inlined small-int fast path refuses
//!   it and deopts).
//! * The shapes the tier **inlines** ([`inlined`]) — **`slice:fib/1`** (a real `.beam`
//!   fixture), **`kernel/2`**, **`wide/2`**, **`aggr/2`** and **`cmp/2`**.
//! * **`cross/2`** ([`workloads::workloads`]) — a two-module `ping/2` ⇄ `pong/2`
//!   ping-pong whose `call_ext_last` deopts to one interpreter step, which the
//!   **driver** turns into a module-context switch (`Step::Enter`). It is the one
//!   workload that measures a cross-module *transfer*: on the baseline tier the callee
//!   is compiled into the per-thread code cache on first sight, so it runs natively
//!   too and the figure isolates the switch.
//! * **`call_ext/0`** ([`workloads::workloads`]) — the `call_ext*` **decline** path: a
//!   counted loop of `call_ext 2 erlang:'+'/2` calls the host answers "run it
//!   synchronously" (`Ok(None)`). On the baseline tier the native `jit_ext_call` helper
//!   consults the host's cross-module seam **once** and hands the instruction to
//!   `jit_step_ext` with that already-computed answer, so `call_ext_dispatch` never
//!   re-asks the seam (it did, before that helper existed). Its per-instruction pin is
//!   `src/jit/baseline/tests.rs::the_baseline_tier_agrees_with_the_interpreter_on_the_declined_call_ext_seam_count`.
//!
//! # Method
//!
//! Every workload is a hand-built synthetic [`Module`] assembled by the loader's own
//! block decomposition ([`beam_loader::lower::build_blocks`]), run on the shared
//! [`MinimalContext`] + a [`JitBackend`]. `cross/2` additionally installs a
//! cross-module-call resolver
//! ([`Workload::install_cross`](workloads::Workload::install_cross)) and drives a
//! second module through it. A native tier **compiles once** outside the
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
//!   and would be flaky. Every workload asserts only that the tiers **agree** on
//!   the result, and that the result is the value the workload is supposed to compute
//!   (compared **structurally**, since the two machines have separate heaps).
//! * Both tiers allocate in the context's arena, which is never freed
//!   (`MinimalContext` has no collector), so a workload that builds terms per run grows
//!   RSS with the iteration count. The counts are chosen to keep that bounded.
//! * **`beam_rt` still hardcodes `Tier::Interpreter`**, so these are engine-mechanism
//!   figures, not end-to-end runtime figures.
//!
//! # Layout
//!
//! [`support`] owns the scaffolding the harnesses share (the synthetic [`Module`]
//! builder, the arithmetic BIF hook and the ns/op report primitives); [`workloads`]
//! owns the workload type, the deopted programs and the cross-module ping-pong,
//! [`inlined`] the tier-inlined ones; [`checks`] owns the per-workload result checks;
//! this file owns the tier seam, the timing loop and the reporting.
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
mod inlined;
mod workloads;

use support::{ns_per_op, ops_per_sec};
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
    /// directly: keeping the owner alive *is* the point (the terms live in the
    /// arena's reserved heap regions, so moving this struct does not move them).
    _context: MinimalContext,
}

impl Measurement {
    /// A measurement of `runs` timed runs of `workload` on `tier`.
    fn run(tier: Tier, workload: &Workload, runs: u32) -> Measurement {
        let mut machine = Machine::new();
        let mut context = MinimalContext::with_bif_hook(workload.bif_hook());
        workload.install_cross(&mut context);
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
// Entry points (driven by the integration test `tests/jit_bench.rs`)
// ---------------------------------------------------------------------------

/// Every workload runs **identically** on the interpreter and on every native tier,
/// each producing the value it is *supposed* to compute.
///
/// Deliberately not timing-sensitive: this is the correctness half, so the `#[test]`
/// that calls it (`tests/jit_bench.rs`) runs in the ordinary `cargo test` suite.
/// Panics with a named message on the first disagreement.
pub fn assert_tier_agreement() {
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
/// The `#[test]` that calls it (`tests/jit_bench.rs`) is `#[ignore]`d because this is a
/// benchmark, not a unit test: run one test (not several) so the workload timings never
/// run concurrently and contaminate each other —
/// `... -p beam_engine --test jit_bench -- --ignored --nocapture`.
pub fn run_measurement_table() {
    for workload in workloads() {
        bench_workload(&workload);
    }
}
