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

//! A Cranelift-specific, **order-balanced** measurement harness for the optional hot
//! tier (`#[ignore]`d; see the test at the bottom for how to run it).
//!
//! # Why this exists next to `tests/jit_bench.rs`
//!
//! `tests/jit_bench.rs` is the project-level tier table: it measures every tier in a
//! fixed order (interpreter, baseline, Cranelift) once per workload and asserts only
//! *correctness*. That is the right shape for a smoke benchmark, but on a shared,
//! loaded machine a single fixed-order sample cannot separate a tier's true cost from
//! the drift of the box, and a fixed order cannot separate it from *position* effects
//! (the baseline always runs before Cranelift, so any warm-up/thermal/frequency drift
//! is charged to Cranelift's account).
//!
//! This harness fixes both: it measures the two **native** tiers — the ones the
//! decision is between; the interpreter's numbers are in `tests/jit_bench.rs` and are
//! an order of magnitude away, so they add nothing here — in an **ABBA** order
//! (`baseline, cranelift, cranelift, baseline`) repeated many times, and it reports the
//! **minimum** (the robust statistic under additive background load: noise can only
//! make a sample slower) alongside the median and the per-round paired ratio.
//!
//! # The workloads
//!
//! Three straight-line synthetic kernels, each the shape of one inline family the
//! Cranelift tier claims — the shapes where it could plausibly beat the baseline
//! (whose emitter is a hand-written x86-64 backend with an explicit per-block register
//! cache and native local-tail-call linking):
//!
//! * `aggr` — 128 steps of `get_list`/`get_hd`/`get_tl`, `get_tuple_element`,
//!   `select_tuple_arity` and small-int `erlang:'*'/2`. The aggregate reads and the
//!   multiply fast path are exactly the aggregate/inline set.
//! * `arith` — 256 chained small-int `erlang:'+'/2` `gc_bif`s over two x-registers,
//!   i.e. the pure inlined-arithmetic chain.
//! * `cmp` — 64 rounds of the eight `erlang` comparison operators over three small-int
//!   x-registers, i.e. the inlined-comparison family.
//!
//! Every invariant that keeps the timed loop on an **inlined** path (so the numbers
//! measure lowering, not the deopt helper) is by construction, exactly as in
//! `tests/jit_bench.rs`: the kernels stay inside the small-integer range, every
//! operator's operands stay small integers, and comparison results are never read back
//! as operands.
//!
//! The module builders are still deliberately **not** shared with `tests/jit_bench.rs`:
//! an in-crate `#[cfg(test)]` module cannot import from an integration test, and the two
//! answer different questions (a fixed-order tier table vs a balanced two-tier
//! comparison). The two in-crate harnesses *do* share their scaffolding —
//! `crate::jit::bench::support` owns the one synthetic-`Module` builder, the one
//! small-int BIF hook and the ns/op report primitive — while the **measurement
//! strategy** below stays this harness's own.

use std::time::{Duration, Instant};

use beam_loader::{Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{make_small, Atom, Eterm};

use crate::context::BifHook;
use crate::jit::baseline::{BaselineJit, CompiledModule};
use crate::jit::bench::support::{arith_hook, ns_per_op, Fixture, Samples};
use crate::jit::JitBackend;
use crate::{Machine, MinimalContext};

use super::{CompiledFunctions, CraneliftJit};

/// The number of ABBA rounds per workload: each round yields **two** samples per tier.
const ROUNDS: usize = 15;

/// The number of `get_list`/`get_hd`/`get_tl` + `get_tuple_element` + two-multiply steps
/// in the aggregate kernel (the same count `tests/jit_bench.rs` uses).
const AGGR_STEPS: usize = 128;

/// The iterations per timed loop for the aggregate kernel.
const AGGR_ITERATIONS: u32 = 2_500;

/// The number of chained small-int `erlang:'+'/2` `gc_bif`s in the arithmetic kernel.
const ARITH_OPS: usize = 256;

/// The iterations per timed loop for the arithmetic kernel.
const ARITH_ITERATIONS: u32 = 5_000;

/// The number of rounds of the eight comparison operators in the comparison kernel.
const CMP_ROUNDS: usize = 64;

/// The iterations per timed loop for the comparison kernel.
const CMP_ITERATIONS: u32 = 2_500;

/// The eight `erlang` comparison operators the comparison kernel chains, one per round.
const CMP_OPERATORS: [&str; 8] = ["=:=", "=/=", "==", "/=", "<", "=<", ">", ">="];

/// The x-registers the comparison kernel compares, holding `10`, `20` and `3`.
const CMP_REGISTERS: [u32; 3] = [0, 1, 2];

/// The three x-registers the comparison results are written to (never read back).
const CMP_DESTINATIONS: [u32; 3] = [3, 4, 5];

/// One measurement kernel: a module, an export, its arguments and its iteration count.
struct Kernel {
    /// The label printed in the table.
    label: &'static str,
    /// The module holding the export.
    module: Module,
    /// The exported function's name atom.
    function: Atom,
    /// The exported arity.
    arity: u32,
    /// The call arguments.
    args: Vec<Eterm>,
    /// The iterations per timed loop.
    iterations: u32,
    /// The result every tier must produce (correctness gate for the harness).
    expected: Eterm,
}

/// The BIF hook every kernel needs: small-int `+`/`-`/`*` and the eight comparisons.
///
/// It is reached only on a **deopt** path (a native tier's inlined fast path computes
/// the same word itself), so this is the reference implementation the inlines must
/// agree with, not the code under measurement.
fn hook_for(kernel: &Kernel) -> BifHook {
    let truthy = kernel.module.atom_table.lookup("true").map(beam_term::make_atom);
    let falsy = kernel.module.atom_table.lookup("false").map(beam_term::make_atom);
    arith_hook(truthy, falsy)
}

/// The aggregate kernel: `aggr/2`, [`AGGR_STEPS`] steps of the inlined aggregate reads
/// plus two small-int multiplies each, over aggregates built once at the top.
///
/// `x(0)` arrives as `1` and both multipliers are `1`, so the product stays `1` (the
/// inlined `'*'` range check never rejects) and the export's result is exactly `1`.
fn aggregate_kernel() -> Kernel {
    let mut fixture = Fixture::new("cl_bench_aggregate");
    // The reads' deopt path builds `{badmatch, Source}`; the kernel never takes it,
    // but the atom must exist so a typed reason can never differ from the oracle's.
    fixture.intern("badmatch");

    let star = fixture.import("erlang", "*", 2);

    let entry = Label(1);
    let body = Label(2);
    let fail = Label(9);

    let mul = |arg: u32| Instr::GcBif {
        arity: 2,
        bif: star,
        fail: NO_FAIL,
        live: 7,
        args: vec![Operand::X(0), Operand::X(arg)],
        dst: Operand::X(0),
    };

    let mut code = vec![
        Instr::Label(entry),
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Integer(2),
            dst: Operand::X(2),
        },
        Instr::PutTuple2 {
            dst: Operand::X(3),
            elements: vec![
                Operand::Integer(1),
                Operand::Integer(1),
                Operand::Integer(1),
            ],
        },
        Instr::SelectTupleArity {
            tuple: Operand::X(3),
            fail,
            destinations: vec![(Operand::Integer(3), body)],
        },
        Instr::Label(body),
    ];
    for _ in 0..AGGR_STEPS {
        code.push(Instr::GetList {
            source: Operand::X(2),
            head: Operand::X(4),
            tail: Operand::X(5),
        });
        code.push(Instr::GetHd {
            source: Operand::X(2),
            head: Operand::X(4),
        });
        code.push(Instr::GetTl {
            source: Operand::X(2),
            tail: Operand::X(5),
        });
        code.push(Instr::GetTupleElement {
            source: Operand::X(3),
            element: 1,
            dst: Operand::X(6),
        });
        code.push(mul(4));
        code.push(mul(6));
    }
    code.push(Instr::Return);
    // The `select_tuple_arity` fail arm: unreachable (the tuple is arity 3), present so
    // the switch is inlined rather than deopted.
    code.push(Instr::Label(fail));
    code.push(Instr::Move {
        src: Operand::Integer(-1),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let (module, aggr) = fixture.finish("aggr", 2, code);

    Kernel {
        label: "aggr/2 (aggregate reads + '*')",
        module,
        function: aggr,
        arity: 2,
        args: vec![make_small(1), make_small(1)],
        iterations: AGGR_ITERATIONS,
        expected: make_small(1),
    }
}

/// The arithmetic kernel: `arith/2`, [`ARITH_OPS`] chained small-int `erlang:'+'/2`
/// `gc_bif`s alternating between `x(0)` and `x(1)`, then `return`.
///
/// The chain accumulates on the `(0, 0)` fixed point, so every intermediate stays a
/// small integer (a `(1, 1)` start would leave the small range after ~90 ops) and the
/// inlined fast path lowers every instruction.
fn arith_kernel() -> Kernel {
    let mut fixture = Fixture::new("cl_bench_arith");
    let plus = fixture.import("erlang", "+", 2);

    let entry = Label(1);
    let mut code = vec![Instr::Label(entry)];
    for step in 0..ARITH_OPS {
        let (args, dst) = if step % 2 == 0 {
            (vec![Operand::X(0), Operand::X(1)], Operand::X(0))
        } else {
            (vec![Operand::X(1), Operand::X(0)], Operand::X(1))
        };
        code.push(Instr::GcBif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            live: 2,
            args,
            dst,
        });
    }
    code.push(Instr::Return);

    let (module, arith) = fixture.finish("arith", 2, code);

    Kernel {
        label: "arith/2 (256 chained '+')",
        module,
        function: arith,
        arity: 2,
        args: vec![make_small(0), make_small(0)],
        iterations: ARITH_ITERATIONS,
        expected: make_small(0),
    }
}

/// The comparison kernel: `cmp/2`, [`CMP_ROUNDS`] rounds of the eight `erlang`
/// comparison operators over `x(0)`=10, `x(1)`=20 and `x(2)`=3 (seeded by the body),
/// with every result written to one of [`CMP_DESTINATIONS`] and never read back.
///
/// The export's result is the trailing `x(2) < x(1)` — the module's own `true` atom.
fn compare_kernel() -> Kernel {
    let mut fixture = Fixture::new("cl_bench_compare");
    let truthy = fixture.intern("true");
    // The comparison fast paths materialize the module's **own** `true`/`false` words
    // and refuse to inline when either is absent, so both must be interned.
    fixture.intern("false");

    let mut operators = Vec::with_capacity(CMP_OPERATORS.len());
    for name in CMP_OPERATORS {
        operators.push(fixture.import("erlang", name, 2));
    }

    let entry = Label(1);
    let mut code = vec![Instr::Label(entry)];
    code.push(Instr::Move {
        src: Operand::Integer(3),
        dst: Operand::X(CMP_REGISTERS[2]),
    });
    for _ in 0..CMP_ROUNDS {
        for (index, operator) in operators.iter().enumerate() {
            let lhs = CMP_REGISTERS[index % CMP_REGISTERS.len()];
            let rhs = CMP_REGISTERS[(index + 1) % CMP_REGISTERS.len()];
            let dst = CMP_DESTINATIONS[index % CMP_DESTINATIONS.len()];
            code.push(Instr::Bif {
                arity: 2,
                bif: *operator,
                fail: NO_FAIL,
                args: vec![Operand::X(lhs), Operand::X(rhs)],
                dst: Operand::X(dst),
            });
        }
    }
    // `x(2)` (3) `<` `x(1)` (20) — the export's result, written to `x(0)`.
    code.push(Instr::Bif {
        arity: 2,
        bif: operators[CMP_OPERATORS
            .iter()
            .position(|name| *name == "<")
            .expect("`<` is in CMP_OPERATORS")],
        fail: NO_FAIL,
        args: vec![Operand::X(CMP_REGISTERS[2]), Operand::X(CMP_REGISTERS[1])],
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let (module, cmp) = fixture.finish("cmp", 2, code);

    Kernel {
        label: "cmp/2 (512 comparison BIFs)",
        module,
        function: cmp,
        arity: 2,
        args: vec![make_small(10), make_small(20)],
        iterations: CMP_ITERATIONS,
        expected: beam_term::make_atom(truthy),
    }
}

/// Time `iterations` runs of a tier on `kernel`, returning the total duration and the
/// last result.
fn timed_tier(
    native: &Native,
    kernel: &Kernel,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
) -> (Duration, Eterm) {
    let start = Instant::now();
    let mut result = make_small(0);
    for _ in 0..kernel.iterations {
        result = native.run(machine, kernel, ctx);
    }
    (start.elapsed(), result)
}

/// One compiled native tier, held so a kernel can be run on it any number of times.
enum Native {
    /// The native baseline tier.
    Baseline {
        /// The backend that owns the compilation.
        backend: BaselineJit,
        /// Its compiled artifact.
        compiled: CompiledModule,
    },
    /// The optional Cranelift hot tier.
    Cranelift {
        /// The backend that owns the compilation.
        backend: CraneliftJit,
        /// Its compiled artifact.
        compiled: CompiledFunctions,
    },
}

impl Native {
    /// The tier's name (the table's `tier` column).
    fn name(&self) -> &'static str {
        match self {
            Native::Baseline { .. } => "baseline-jit",
            Native::Cranelift { .. } => "jit-cranelift",
        }
    }

    /// Run the kernel's export on the already-compiled artifact.
    fn run(&self, machine: &mut Machine, kernel: &Kernel, ctx: &mut MinimalContext) -> Eterm {
        match self {
            Native::Baseline { backend, compiled } => JitBackend::run_compiled(
                backend,
                compiled,
                machine,
                &kernel.module,
                kernel.function,
                kernel.arity,
                &kernel.args,
                ctx,
            ),
            Native::Cranelift { backend, compiled } => JitBackend::run_compiled(
                backend,
                compiled,
                machine,
                &kernel.module,
                kernel.function,
                kernel.arity,
                &kernel.args,
                ctx,
            ),
        }
        .expect("the kernel runs on the native tier")
    }
}

/// Measure one kernel on the baseline and Cranelift tiers with an ABBA order.
///
/// Returns the two tiers' samples plus the per-round paired ratio
/// (`cranelift / baseline` over the two samples that share a round).
fn measure_kernel(kernel: &Kernel) -> (Samples, Samples, Vec<f64>) {
    let mut baseline_backend = BaselineJit::new();
    let start = Instant::now();
    let baseline_compiled =
        JitBackend::compile(&mut baseline_backend, &kernel.module).expect("baseline compile");
    let baseline_compile = start.elapsed();

    let mut cranelift_backend = CraneliftJit::new();
    let start = Instant::now();
    let cranelift_compiled =
        JitBackend::compile(&mut cranelift_backend, &kernel.module).expect("cranelift compile");
    let cranelift_compile = start.elapsed();

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(hook_for(kernel));

    // One warm-up run per tier (each run resets the machine at entry, so one machine
    // serves the whole measurement).
    let baseline = Native::Baseline {
        backend: baseline_backend,
        compiled: baseline_compiled,
    };
    let cranelift = Native::Cranelift {
        backend: cranelift_backend,
        compiled: cranelift_compiled,
    };

    assert_eq!(
        baseline.run(&mut machine, kernel, &mut ctx),
        kernel.expected,
        "the baseline tier must produce the kernel's pinned result"
    );
    assert_eq!(
        cranelift.run(&mut machine, kernel, &mut ctx),
        kernel.expected,
        "the Cranelift tier must produce the kernel's pinned result"
    );

    let mut baseline_samples = Vec::with_capacity(ROUNDS * 2);
    let mut cranelift_samples = Vec::with_capacity(ROUNDS * 2);
    let mut ratios = Vec::with_capacity(ROUNDS);

    for _ in 0..ROUNDS {
        // ABBA: baseline, cranelift, cranelift, baseline — position-balanced, so a
        // monotone drift of the box contributes equally to both tiers.
        let (baseline_a, _) = timed_tier(&baseline, kernel, &mut machine, &mut ctx);
        let (cranelift_a, _) = timed_tier(&cranelift, kernel, &mut machine, &mut ctx);
        let (cranelift_b, _) = timed_tier(&cranelift, kernel, &mut machine, &mut ctx);
        let (baseline_b, _) = timed_tier(&baseline, kernel, &mut machine, &mut ctx);

        let (ba, ca, cb, bb) = (
            ns_per_op(baseline_a, kernel.iterations),
            ns_per_op(cranelift_a, kernel.iterations),
            ns_per_op(cranelift_b, kernel.iterations),
            ns_per_op(baseline_b, kernel.iterations),
        );
        ratios.push((ca + cb) / (ba + bb));
        baseline_samples.push(ba);
        baseline_samples.push(bb);
        cranelift_samples.push(ca);
        cranelift_samples.push(cb);
    }

    (
        Samples {
            tier: baseline.name(),
            compile: baseline_compile,
            ns_per_op: baseline_samples,
        },
        Samples {
            tier: cranelift.name(),
            compile: cranelift_compile,
            ns_per_op: cranelift_samples,
        },
        ratios,
    )
}

/// Print one kernel's markdown table rows plus the per-round paired ratios.
fn report(kernel: &Kernel, baseline: &Samples, cranelift: &Samples, ratios: &[f64]) {
    let b_min = baseline.min();
    let c_min = cranelift.min();
    println!(
        "| {} | {} | {:.1} | {:.1} | {:.1} | — |",
        kernel.label,
        baseline.tier,
        b_min,
        baseline.median(),
        baseline.compile.as_secs_f64() * 1e6,
    );
    println!(
        "| {} | {} | {:.1} | {:.1} | {:.1} | {:.3}× min, {:.3}× median |",
        kernel.label,
        cranelift.tier,
        c_min,
        cranelift.median(),
        cranelift.compile.as_secs_f64() * 1e6,
        c_min / b_min,
        cranelift.median() / baseline.median(),
    );
    let spread = ratios
        .iter()
        .copied()
        .fold((f64::INFINITY, 0.0f64), |(lo, hi), r| {
            (lo.min(r), hi.max(r))
        });
    let mean: f64 = ratios.iter().sum::<f64>() / ratios.len() as f64;
    println!(
        "| {} | paired cranelift/baseline per round | mean {:.3} | range {:.3}–{:.3} | \
         {} rounds | — |",
        kernel.label,
        mean,
        spread.0,
        spread.1,
        ratios.len(),
    );
}

/// The order-balanced Cranelift-vs-baseline measurement.
///
/// Run it with:
/// `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine \
///  --features jit-cranelift --lib -- --ignored --nocapture cl_bench`
#[test]
#[ignore = "measurement harness — run explicitly with `-- --ignored --nocapture`"]
fn cl_bench_cranelift_vs_baseline_interleaved() {
    let kernels = [aggregate_kernel(), arith_kernel(), compare_kernel()];

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    println!(
        "\n=== beam_engine `jit-cranelift` vs `baseline-jit` — ABBA-interleaved, \
         {ROUNDS} rounds × 2 samples/tier ==="
    );
    println!(
        "(machine: {cores} cores, shared sandbox — min is the robust statistic; a \
         ratio < 1.0 means Cranelift is FASTER)\n"
    );
    println!("| workload | tier | min ns/op | median ns/op | compile us | cranelift/baseline |");
    println!("|---|---|---|---|---|---|");

    for kernel in &kernels {
        let (baseline, cranelift, ratios) = measure_kernel(kernel);
        report(kernel, &baseline, &cranelift, &ratios);
    }
    println!();
}
