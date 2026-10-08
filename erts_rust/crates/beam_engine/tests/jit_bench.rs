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

//! Micro-benchmark: the interpreter against the native JIT tiers.
//!
//! The Milestone-1 JIT tiers are **mechanisms**, not optimisers (Part II §F):
//! correctness first, performance work follows. This file is the honest
//! measurement of where they stand today — it prints `ns/op` (and `ops/s`) per
//! workload and tier, and asserts **only correctness** (every tier's result equals
//! the interpreter's). It never asserts that a JIT is faster: that is
//! environment-dependent and would be flaky; the printed numbers are the evidence.
//!
//! Five workloads: the first three hit the three shapes the baseline tier's
//! arithmetic/register story treats differently, the fourth the **aggregate reads**
//! it inlines in the same round, and the fifth the **comparison BIFs** the same round
//! also lowered.
//!
//! * **`fib(20)`** — the real `.beam` fixture's `fib/1`
//!   (`beam_loader/tests/fixtures/slice.beam`, reused by relative path so there is
//!   a single source of truth for the image), which is `fib(N) -> fib(N, 0, 1)`:
//!   a *short* tail-recursive chain of `call`/`gc_bif`/`return` (about 21 calls),
//!   so the driver loop's interpreter-step path and the per-run setup both count.
//! * **`kernel/2`** — a hand-built synthetic module (the loader is not involved;
//!   see `tests/opcodes.rs` for the same scaffolding pattern) whose body is a long
//!   straight-line chain of `KERNEL_OPS` (256) chained `erlang:'+'/2` `gc_bif`s
//!   accumulating on `x(0)`/`x(1)`, ending in `return`. This is exactly the shape
//!   the baseline tier's inlined guarded small-int fast path (`try_inline_gc_bif`)
//!   lowers to native machine code with no interpreter round-trip, and it is sized
//!   so that execution — not the per-run setup — dominates the measurement. Every
//!   register it touches is one of the four the block caches.
//! * **`wide/2`** — the same synthetic style, but straight-line over
//!   [`WIDE_REGISTERS`] (8) x-registers ([`WIDE_OPS`] moves plus adds), i.e. **more
//!   than the four the emitter caches**. That is the shape the *direct `machine.x`
//!   addressing* of the `move`/`swap`/fast-path lowering is for: the same body used
//!   to deopt at the first instruction touching `x(4)` and run the rest one
//!   interpreter step at a time.
//! * **`aggr/2`** — the same synthetic style, but a straight-line body of
//!   [`AGGR_STEPS`] steps whose every instruction is either one of the guarded
//!   aggregate reads (`get_list`/`get_hd`/`get_tl`, `get_tuple_element`,
//!   `select_tuple_arity`) or a small-int `erlang:'*'/2` `gc_bif` — the opcodes
//!   inlined in the same round as the direct-addressing work. The cons cell and the
//!   tuple are built **once** at the top of the body (the `put_list`/`put_tuple2`
//!   builders stay on the interpreter step) and every read result is fed into the
//!   multiply chain, so the timed loop is dominated by the new inlines rather than by
//!   the aggregate construction, the BIF machinery or the per-run setup.
//! * **`cmp/2`** — the same synthetic style, but a straight-line body of
//!   [`CMP_OPS`] rounds of the eight `erlang` comparison operators
//!   (`=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`), every one of them a **non-collecting**
//!   `bif` over two small-int x-registers with an x-register destination — the shape
//!   the comparison fast path lowers natively (see `src/jit/baseline/emit/compare.rs`
//!   and `src/jit/cranelift/compare.rs`). Both operands are `x`-register *runtime*
//!   values, so the inlined run-time small-int guard is exercised too. Left on the
//!   helper each comparison costs a full `jit_step_one` round-trip, which is exactly
//!   what this workload is here to measure.
//!
//! # Method — execution, not compilation
//!
//! For each `(workload, tier)`:
//!
//! 1. a native tier **compiles the module exactly once**, *outside* the timed
//!    loop, using [`JitBackend::compile`] and keeping the resulting artifact;
//! 2. one warm-up run (not timed);
//! 3. `iterations` timed *executions* on a reused [`Machine`]:
//!    [`JitBackend::run_compiled`] for a native tier (the compile-once / run-many
//!    entry, so **no compilation is inside the timed loop**), and
//!    [`Machine::run_export`] for the interpreter;
//! 4. the elapsed wall-clock time from [`Instant`], divided by `iterations`.
//!
//! Every `run_*` entry `reset()`s the machine at the start of a run, so one machine
//! can be reused across the whole loop (the timed cost is the call, not per-iteration
//! `Machine` construction noise).
//!
//! **Honest caveat, printed with the numbers.** Neither native backend has a
//! *cross-run code cache* yet (see `beam_engine/CONTEXT.md`, Known Issues): the cache
//! is the caller's job, which is why this benchmark holds the artifact across the
//! loop via [`JitBackend::run_compiled`]. The one-shot [`JitBackend::run_export`]
//! — what `Machine::run_with_tier` uses — still recompiles on every call; its cost
//! is *not* what the numbers below measure. Each native tier's one-time `compile`
//! latency is measured and printed separately, and the timed loop **excludes**
//! compilation. What remains in `ns/op` is per-run setup (`Machine::reset` of the
//! register/float-register files plus the module-wide label-map build) plus the
//! actual execution; for `fib(20)` that is thousands of instructions, for the
//! kernel it is [`KERNEL_OPS`] inlined adds, for the aggregate kernel it is
//! [`AGGR_STEPS`] steps of the inlined aggregate reads plus `'*'`, and for the
//! comparison kernel it is [`CMP_OPS`] rounds of inlined comparison BIFs.
//!
//! Run with `--nocapture` to see the table.
//!
//! [`run_export`]: JitBackend::run_export
//! [`run_compiled`]: JitBackend::run_compiled

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_engine::context::BifHook;
use beam_engine::jit::baseline::{BaselineJit, CompiledModule};
#[cfg(feature = "jit-cranelift")]
use beam_engine::jit::cranelift::{CompiledFunctions, CraneliftJit};
use beam_engine::{available_tiers, JitBackend, Machine, MinimalContext, Tier};
use beam_loader::{
    load_file, Block, Export, Function, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{is_small, make_atom, make_small, signed_value, Atom, Eterm};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// The number of chained `erlang:'+'/2` `gc_bif`s in the synthetic kernel.
///
/// Large enough that the *execution* of the block (the inlined native arithmetic)
/// dominates the per-run setup (`Machine::reset` + the label-map build), which is
/// the point of the kernel workload: it measures compute, not call overhead.
const KERNEL_OPS: usize = 256;

/// The number of x-registers the wide kernel walks — twice the number of registers
/// a block caches in machine registers, so half of them are addressed directly in
/// `machine.x`.
const WIDE_REGISTERS: u32 = 8;

/// The number of straight-line steps (one `move` plus one `gc_bif '+'/2`) in the
/// wide kernel. Every value stays a small integer (the accumulator is bounded by
/// this count), so the inlined small-int fast path lowers every single step.
const WIDE_OPS: usize = 256;

/// The number of straight-line steps in the aggregate kernel — one
/// `get_list`/`get_hd`/`get_tl` trio, one `get_tuple_element` and two
/// `gc_bif '*'/2` fast-path multiplies each.
const AGGR_STEPS: usize = 128;

/// The number of rounds the comparison kernel walks its eight operators — eight
/// comparison BIFs per round, so this many times eight comparisons per run.
///
/// Each round is eight `erlang` comparison BIFs over the same three small-int
/// x-registers — the exact shape the baseline and Cranelift tiers now lower natively
/// (`src/jit/baseline/emit/compare.rs`, `src/jit/cranelift/compare.rs`). Every one of
/// them would otherwise cost a full `jit_step_one` helper round-trip (see
/// [`compare_workload`]), so a body long enough for execution to dominate the per-run
/// setup is what makes this workload measure the inlines rather than the driver.
const CMP_OPS: usize = 64;

/// The eight `erlang` comparison operators the comparison kernel chains, in the order
/// it walks them — one per round (the operator is looked up in each module's import
/// table by name, so the order here only decides the round's shape).
const CMP_OPERATORS: [&str; 8] = ["=:=", "=/=", "==", "/=", "<", "=<", ">", ">="];

/// The index of `"<"` in [`CMP_OPERATORS`] — the operator of the trailing comparison
/// that fixes the comparison kernel's exported result (see [`compare_workload`]).
const CMP_LT: usize = 4;

/// The x-registers the comparison kernel compares, and the values they hold:
/// `10`, `20` and `3`.
///
/// All three are **small integers for the whole run** (nothing writes them again), so
/// every comparison's inlined run-time guard passes and the timed loop stays on the
/// native fast path instead of deopting to the helper. Register pairs rotate over
/// these three (see [`compare_workload`]) so the rounds produce a mix of `true` and
/// `false` results rather than a constant.
const CMP_REGISTERS: [u32; 3] = [0, 1, 2];

/// The values [`CMP_REGISTERS`] hold: `x(0)`/`x(1)` arrive as the call arguments,
/// `x(2)` is seeded by the body itself.
const CMP_ARGS: [i64; 2] = [10, 20];

/// The value the body seeds `x(2)` with.
const CMP_SEED: i64 = 3;

/// The three x-registers the comparison results are written to, cycled over the
/// rounds. None of them is ever read back as an operand (a comparison result is an
/// atom, and the kernel's operands must stay small integers), so the chain never
/// feeds a boolean forward.
const CMP_DESTINATIONS: [u32; 3] = [3, 4, 5];

/// The slice BIF hook: `erlang:'+'/2`, `erlang:'-'/2` and `erlang:'*'/2` over
/// small integers.
///
/// The same implementation serves every workload (the synthetic kernels import
/// `erlang:'+'/2`, the aggregate kernel imports `erlang:'*'/2`). It is reached only
/// on a **deopt** path: a native tier's inlined fast path computes the same small-int
/// results itself, so the hook is what the *interpreter* tier (and any rejected
/// operand) goes through.
fn arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match function {
        "+" => Ok(make_small(a + b)),
        "-" => Ok(make_small(a - b)),
        "*" => Ok(make_small(a * b)),
        _ => Err(BeamError::BadArg),
    }
}

/// The eight `erlang` comparison operators, answered for two small-int operands.
///
/// The comparison kernel's every operand is a small integer (it never feeds a
/// boolean result forward), so the exact (`=:=`/`=/=`) and arithmetic
/// (`==`/`/=`/`<`/`=<`/`>`/`>=`) orders coincide with the plain integer comparison
/// used here — the same value the tiers' inlined single `cmp`/`jcc` produces. `None`
/// means "not one of the eight, or not the small-int shape this kernel runs" and lets
/// the hook fall through to [`arith_hook`], whose error is loud rather than a
/// fabricated boolean.
fn compare_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
    truthy: Eterm,
    falsy: Eterm,
) -> Option<Result<Eterm, BeamError>> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return None;
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return None;
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    let result = match function {
        "=:=" | "==" => a == b,
        "=/=" | "/=" => a != b,
        "<" => a < b,
        "=<" => a <= b,
        ">" => a > b,
        ">=" => a >= b,
        _ => return None,
    };
    Some(Ok(if result { truthy } else { falsy }))
}

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

/// Which workload is being timed (selects the iteration count).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `slice:fib/1` from the committed oracle fixture.
    Fib,
    /// The hand-built straight-line arithmetic kernel.
    Kernel,
    /// The hand-built straight-line kernel wide enough to exceed the register cache.
    Wide,
    /// The hand-built straight-line kernel of the inlined aggregate reads.
    Aggr,
    /// The hand-built straight-line kernel of the inlined comparison BIFs.
    Cmp,
}

impl Kind {
    /// The iteration count for this workload.
    ///
    /// Compilation is **outside** the timed loop now (each native tier compiles once,
    /// up front, and only ever calls [`JitBackend::run_compiled`] afterwards), so
    /// every tier gets the *same* count — there is no per-call compile for a tier to
    /// amortise, and the counts only have to keep the whole file inside its ~1 s
    /// budget while leaving each tier's timed loop long enough to be stable.
    fn iterations(self) -> u32 {
        match self {
            // `fib/1` in the fixture is `fib(N) -> fib(N, 0, 1)`, i.e. a ~20-call
            // tail-recursive chain — about 5 us per run, so per-run setup dominates.
            Kind::Fib => 5_000,
            // [`KERNEL_OPS`] chained inlined adds per run: a few microseconds on the
            // interpreter, well under a microsecond on the baseline tier.
            Kind::Kernel => 5_000,
            // [`WIDE_OPS`] `move`+`add` steps per run: the interpreter pays a full
            // step for each of them.
            Kind::Wide => 5_000,
            // [`AGGR_STEPS`] steps of six inlined instructions each (the aggregate
            // reads plus two `'*'` multiplies) — roughly three times the wide body per
            // run, so a smaller count keeps the whole file inside its ~1 s budget.
            Kind::Aggr => 2_500,
            // [`CMP_OPS`] rounds of eight comparison BIFs each: 512 comparisons per
            // run, which the interpreter pays a helper round-trip (and a `Box`ed hook
            // call) for and a native tier lowers to a compare and a branch each.
            Kind::Cmp => 2_500,
        }
    }
}

/// A benchmark workload: a module, an exported `function/arity` and its arguments.
struct Workload {
    /// The human-readable label printed with the numbers.
    label: String,
    /// The workload shape (selects the iteration count).
    kind: Kind,
    /// The module holding the export.
    module: Module,
    /// The exported function's name atom (in `module`'s atom table).
    function: Atom,
    /// The exported arity.
    arity: u32,
    /// The call arguments.
    args: Vec<Eterm>,
}

impl Workload {
    /// The BIF hook this workload's runs need — the pluggable body of the
    /// interpreter's `ctx.call_bif`.
    ///
    /// Every arithmetic workload uses [`arith_hook`] verbatim (the native tiers'
    /// inlined fast paths never consult the hook, so it is the interpreter tier and
    /// every rejected operand that goes through it). The comparison kernel needs one
    /// thing more: the result of a comparison is the module's **own** `true`/`false`
    /// atom word, and an atom index only means something inside the [`AtomTable`] it
    /// was interned in (each workload here owns a private table), so the boolean can
    /// only be built from *this* module's table. The closure captures the two words
    /// the kernel's module defines and answers the eight operators with them, falling
    /// through to [`arith_hook`] for every other BIF.
    ///
    /// [`AtomTable`]: beam_term::AtomTable
    fn bif_hook(&self) -> BifHook {
        if self.kind != Kind::Cmp {
            return Box::new(arith_hook);
        }
        let truthy = self.module.atom_table.lookup("true").map(make_atom);
        let falsy = self.module.atom_table.lookup("false").map(make_atom);
        Box::new(move |module, function, arity, args| {
            if let (Some(truthy), Some(falsy)) = (truthy, falsy) {
                if let Some(result) = compare_hook(module, function, arity, args, truthy, falsy) {
                    return result;
                }
            }
            arith_hook(module, function, arity, args)
        })
    }
}

/// `slice:fib/1` over the committed oracle fixture.
fn fib_workload() -> Workload {
    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    let function = atom(&module, "fib");
    Workload {
        label: "slice:fib/1 (20)".to_string(),
        kind: Kind::Fib,
        module,
        function,
        arity: 1,
        args: vec![make_small(20)],
    }
}

/// The synthetic straight-line kernel: `kernel/2` is [`KERNEL_OPS`] chained
/// `erlang:'+'/2` `gc_bif`s accumulating on `x(0)`/`x(1)`, then `return`.
///
/// The body deliberately stays inside the baseline tier's inlined fast path: the
/// only BIF is small-int `'+'`, both operands and the destination are (cacheable)
/// x-registers, so every instruction of the block is lowered to machine code and
/// the driver loop never takes an interpreter step — except for the single trailing
/// `return`, which is one helper call per run.
///
/// `Function::blocks` **must** be populated for that to happen: a native tier
/// compiles blocks, so a function with an empty `blocks` list has no native entry
/// point at all and the driver would take an interpreter step per instruction
/// (measuring the deopt path, not the inlined arithmetic). The hand-built block is
/// the faithful decomposition of this straight-line body: one block starting at the
/// entry label and covering the whole function.
///
/// The chain accumulates on `x(0)`/`x(1)` from the `(0, 0)` fixed point. That is
/// forced by the shape: an `erlang:'+'/2`-only dependency chain is monotone, so the
/// Fibonacci-style growth a `(1, 1)` start produces leaves the 59-bit small-integer
/// range after ~90 of the [`KERNEL_OPS`] ops (`make_small` would reject the result).
/// Starting at zero keeps every intermediate value a small integer — which is
/// exactly what the baseline tier's inlined fast path requires — while executing the
/// same 256 chained BIFs over the same registers, each of which reads the previous
/// one's result (a genuine serial dependency chain).
fn kernel_workload() -> Workload {
    let mut module = Module::default();
    module.name = module.atom_table.intern("jit_bench_kernel");

    let plus_module = module.atom_table.intern("erlang");
    let plus_function = module.atom_table.intern("+");
    module.imports.push(Import {
        module: plus_module,
        function: plus_function,
        arity: 2,
    });
    let plus = ImportRef(0);

    let kernel = module.atom_table.intern("kernel");
    let entry = Label(1);
    module.exports.push(Export {
        function: kernel,
        arity: 2,
        label: entry,
    });

    let mut code = vec![Instr::Label(entry)];
    for step in 0..KERNEL_OPS {
        // `x0 = x0 + x1` then `x1 = x1 + x0`, alternating: a dependency chain that
        // keeps both registers live across every operation.
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

    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();

    // One block: the whole straight-line body, from the entry label to the end.
    let blocks = vec![Block {
        label: entry,
        start: 0,
        end: code.len(),
    }];

    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name: kernel,
        arity: 2,
        code,
        blocks,
        labels,
    });

    Workload {
        label: format!("kernel/2 ({KERNEL_OPS} chained gc_bif '+'/2)"),
        kind: Kind::Kernel,
        module,
        function: kernel,
        arity: 2,
        args: vec![make_small(0), make_small(0)],
    }
}

/// The synthetic **wide** kernel: `wide/2` is a straight-line body of [`WIDE_OPS`]
/// `move`+`gc_bif '+'/2` steps over [`WIDE_REGISTERS`] x-registers.
///
/// Each step writes `1` into `x(r)` and accumulates it into `x(r + 1)`, walking all
/// eight registers. The block caches only four of them, so `x(4)`..`x(7)` — reached
/// from the fourth step on — are addressed **directly in `machine.x`** by the
/// inlined `move` and by the fast path's operand loads / result store. Before that
/// path existed the block deopted at the first instruction touching `x(4)` and the
/// whole remaining body ran one interpreter step at a time, which is exactly the
/// difference this workload is here to measure.
///
/// Every value stays a small integer (the accumulator is bounded by [`WIDE_OPS`]
/// writes of `1`), so the fast path lowers every step; the leading block of `move`s
/// seeds all eight registers with `0` so that not even the first guard sees a
/// non-small value (a fresh `Machine` starts every register at `NIL`).
///
/// The body is one basic block, exactly like [`kernel_workload`] — a function with an
/// empty `blocks` list has no native entry point at all.
fn wide_workload() -> Workload {
    let mut module = Module::default();
    module.name = module.atom_table.intern("jit_bench_wide");

    let plus_module = module.atom_table.intern("erlang");
    let plus_function = module.atom_table.intern("+");
    module.imports.push(Import {
        module: plus_module,
        function: plus_function,
        arity: 2,
    });
    let plus = ImportRef(0);

    let wide = module.atom_table.intern("wide");
    let entry = Label(1);
    module.exports.push(Export {
        function: wide,
        arity: 2,
        label: entry,
    });

    let mut code = vec![Instr::Label(entry)];
    for register in 0..WIDE_REGISTERS {
        code.push(Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(register),
        });
    }
    for step in 0..WIDE_OPS {
        let register = (step as u32) % WIDE_REGISTERS;
        let next = (register + 1) % WIDE_REGISTERS;
        code.push(Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(register),
        });
        code.push(Instr::GcBif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            live: WIDE_REGISTERS,
            args: vec![Operand::X(register), Operand::X(next)],
            dst: Operand::X(next),
        });
    }
    code.push(Instr::Move {
        src: Operand::X(WIDE_REGISTERS - 1),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();

    // One block: the whole straight-line body, from the entry label to the end.
    let blocks = vec![Block {
        label: entry,
        start: 0,
        end: code.len(),
    }];

    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name: wide,
        arity: 2,
        code,
        blocks,
        labels,
    });

    Workload {
        label: format!("wide/2 ({WIDE_OPS} steps over {WIDE_REGISTERS} x-registers)"),
        kind: Kind::Wide,
        module,
        function: wide,
        arity: 2,
        args: vec![make_small(0), make_small(0)],
    }
}

/// The synthetic **aggregate** kernel: `aggr/2` is a straight-line body of
/// [`AGGR_STEPS`] steps, each reading the same cons cell and tuple through every
/// aggregate read the baseline tier inlines and feeding each result into a small-int
/// `erlang:'*'/2` `gc_bif` chain:
///
/// ```text
///   x2 = [1 | 2]                     ; put_list        (interpreter step)
///   x3 = {1, 1, 1}                   ; put_tuple2      (interpreter step)
///   select_tuple_arity x3 Fail {3 -> body}   ; inlined switch
/// body:
///   get_list x2 x4 x5                ; x4 = CAR (1), x5 = CDR (2)
///   get_hd   x2 x4                   ; x4 = CAR
///   get_tl   x2 x5                   ; x5 = CDR
///   get_tuple_element x3 1 x6        ; x6 = element 1 (1)
///   x0 = x0 * x4                     ; '*'/2 fast path
///   x0 = x0 * x6                     ; '*'/2 fast path
///   ... repeated [`AGGR_STEPS`] times, then `return`
/// ```
///
/// The aggregates are built **once per run** at the top of the body and the two
/// multipliers are deliberately `1`: `x(0)` arrives as `1` (the call argument), so
/// the product stays `1` and can never leave the small-integer range. That is what
/// keeps the measurement on the **inlined** fast path — the inlined `'*'` range check
/// would take its deopt branch (and this workload would then time the interpreter
/// fallback) as soon as the accumulator left the small range, which a growing product
/// does after ~59 steps.
///
/// Registers `x(0)`, `x(2)`..`x(6)` are touched; the block caches four of them and
/// addresses the rest directly in `machine.x`, so both lowering paths are exercised.
/// The body is three blocks (`build_blocks`): the entry block that builds the
/// aggregates, the body block, and the (unreachable) `select_tuple_arity` fail arm.
fn aggregate_workload() -> Workload {
    let mut module = Module::default();
    module.name = module.atom_table.intern("jit_bench_aggregate");
    // The reads' deopt path builds `{badmatch, Source}`; the workload never takes it,
    // but the atom is interned so the typed reason cannot differ from the oracle's.
    module.atom_table.intern("badmatch");

    let star_module = module.atom_table.intern("erlang");
    let star_function = module.atom_table.intern("*");
    module.imports.push(Import {
        module: star_module,
        function: star_function,
        arity: 2,
    });
    let star = ImportRef(0);

    let aggr = module.atom_table.intern("aggr");
    let entry = Label(1);
    let body = Label(2);
    let fail = Label(9);
    module.exports.push(Export {
        function: aggr,
        arity: 2,
        label: entry,
    });

    // `x0 = x0 * x(b)` — the small-int `'*'/2` fast path the native tiers inline.
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
        // The cons cell `[1 | 2]` (both halves small integers, so every read result
        // is a legal multiplier) and the 3-tuple `{1, 1, 1}` — built once per run.
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
    // The `select_tuple_arity` fail arm: reachable only if the tuple were not arity
    // 3, i.e. never — it exists so the switch is inlined (a `NO_FAIL` switch deopts).
    code.push(Instr::Label(fail));
    code.push(Instr::Move {
        src: Operand::Integer(-1),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    // The faithful decomposition of that body: the loader's own label-delimited
    // grouping (a native tier compiles blocks, so a wrong grouping would leave the
    // body block without a native entry point).
    let (blocks, labels) = beam_loader::lower::build_blocks(&code);

    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name: aggr,
        arity: 2,
        code,
        blocks,
        labels,
    });

    Workload {
        label: format!(
            "aggr/2 ({AGGR_STEPS} steps of get_list/get_hd/get_tl + get_tuple_element + \
             gc_bif '*'/2)"
        ),
        kind: Kind::Aggr,
        module,
        function: aggr,
        arity: 2,
        args: vec![make_small(1), make_small(1)],
    }
}

/// The synthetic **comparison** kernel: `cmp/2` is a straight-line body of
/// [`CMP_OPS`] rounds of the eight `erlang` comparison operators, i.e. inside each
/// round one comparison per operator, over the three small-int x-registers seeded
/// with [`CMP_ARGS`] / [`CMP_SEED`]:
///
/// ```text
///   x2 = 3                           ; move   (inlined; x0 = 10, x1 = 20 arrive as args)
///   x3 = x0 =:= x1                   ; bif    (inlined comparison fast path)
///   x4 = x1 =/= x2                   ; bif    (inlined)
///   x5 = x2 ==  x0                   ; bif    (inlined)
///   x3 = x0 /=  x1                   ; bif    (inlined)
///   x4 = x1 <   x2                   ; bif    (inlined)
///   x5 = x2 =<  x0                   ; bif    (inlined)
///   x3 = x0 >   x1                   ; bif    (inlined)
///   x4 = x1 >=  x2                   ; bif    (inlined)
///   ... repeated [`CMP_OPS`] times,
///   x0 = x2 < x1                     ; the export's result — 3 < 20, i.e. `true`
///   return
/// ```
///
/// This is the shape the baseline tier's and Cranelift tier's comparison fast paths
/// claim (`src/jit/baseline/emit/compare.rs`, `src/jit/cranelift/compare.rs`): a
/// **non-collecting** `bif` (`Instr::Bif`, which is how the compiler emits these
/// operators) whose two operands are x-registers and whose destination is an
/// x-register. Both operands are *runtime* values, so the fast path's inlined small-int
/// guard runs on every one of them; the interpreter, by contrast, pays a full dispatch
/// step (an argument `Vec` plus a `Box`ed hook call) for each of them — and a native
/// tier that failed to inline one would pay a `jit_step_one` round-trip — which is the
/// difference this workload is here to measure.
///
/// Two invariants keep the measurement on the inlined path, and both are by
/// construction rather than by hope:
///
/// * every **operand** register (`x(0)`..`x(2)`) holds a small integer for the whole
///   run (nothing ever writes them again), so no guard ever rejects; and
/// * every **result** goes to one of [`CMP_DESTINATIONS`] (`x(3)`..`x(5)`), which are
///   never read as operands — a comparison result is an atom, and feeding one forward
///   would leave the small-int shape the fast path requires.
///
/// The operator/operand pairing (`x(k)` against `x(k+1)`, cyclically) is chosen so the
/// rounds produce a mix of `true` and `false` results rather than a constant, while the
/// trailing comparison's `true` is what the export returns — so the benchmark's
/// correctness assertion sees a real comparison result (the module's own `true` atom),
/// not a trivially-agreeing accumulator.
///
/// The body is one basic block, exactly like [`kernel_workload`]: a function with an
/// empty `blocks` list has no native entry point at all.
fn compare_workload() -> Workload {
    assert_eq!(
        CMP_OPERATORS[CMP_LT], "<",
        "CMP_LT must index the `<` operator the trailing comparison uses"
    );

    let mut module = Module::default();
    module.name = module.atom_table.intern("jit_bench_compare");
    // The comparison fast paths materialize the **module's own** `true`/`false` atom
    // words and refuse to inline a comparison when either is absent (they never
    // fabricate an atom), so both must be interned — and the interpreter's hook needs
    // them for the same reason (see `Workload::bif_hook`).
    module.atom_table.intern("true");
    module.atom_table.intern("false");

    let erlang = module.atom_table.intern("erlang");
    let mut operators = Vec::with_capacity(CMP_OPERATORS.len());
    for name in CMP_OPERATORS {
        let function = module.atom_table.intern(name);
        operators.push(ImportRef(module.imports.len() as u32));
        module.imports.push(Import {
            module: erlang,
            function,
            arity: 2,
        });
    }

    let cmp = module.atom_table.intern("cmp");
    let entry = Label(1);
    module.exports.push(Export {
        function: cmp,
        arity: 2,
        label: entry,
    });

    let mut code = vec![Instr::Label(entry)];
    // `x(0)`/`x(1)` arrive as the call arguments; `x(2)` is seeded here.
    code.push(Instr::Move {
        src: Operand::Integer(CMP_SEED),
        dst: Operand::X(CMP_REGISTERS[2]),
    });
    for _ in 0..CMP_OPS {
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
    // The export's result: `x(2)` (3) `<` `x(1)` (20) is `true`. Writing `x(0)` is
    // safe here and only here — it is an operand register, but this is the last
    // instruction before the `return`.
    code.push(Instr::Bif {
        arity: 2,
        bif: operators[CMP_LT],
        fail: NO_FAIL,
        args: vec![Operand::X(CMP_REGISTERS[2]), Operand::X(CMP_REGISTERS[1])],
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();

    // One block: the whole straight-line body, from the entry label to the end.
    let blocks = vec![Block {
        label: entry,
        start: 0,
        end: code.len(),
    }];

    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name: cmp,
        arity: 2,
        code,
        blocks,
        labels,
    });

    Workload {
        label: format!(
            "cmp/2 ({CMP_OPS} rounds of the eight comparison BIFs over \
             {} small-int x-registers)",
            CMP_REGISTERS.len()
        ),
        kind: Kind::Cmp,
        module,
        function: cmp,
        arity: 2,
        args: vec![make_small(CMP_ARGS[0]), make_small(CMP_ARGS[1])],
    }
}

// ---------------------------------------------------------------------------
// The native backends
// ---------------------------------------------------------------------------
/// A native [`JitBackend`] **with the workload already compiled into it**.
///
/// The enum exists to erase the two backends' distinct
/// [`Compiled`](JitBackend::Compiled) associated types behind one holder, so the
/// benchmark can compile once up front and only call
/// [`JitBackend::run_compiled`] inside its timed loop — there is no `compile` call
/// (and therefore no compilation) in the timed code path.
enum NativeBackend {
    /// The native baseline tier (always in the roster).
    Baseline {
        /// The (zero-sized) backend that owns the compilation.
        backend: BaselineJit,
        /// Its already-compiled artifact (executable code + block entries).
        compiled: CompiledModule,
    },
    /// The optional Cranelift hot tier (feature-gated).
    #[cfg(feature = "jit-cranelift")]
    Cranelift {
        /// The Cranelift backend that owns the compilation.
        backend: CraneliftJit,
        /// Its already-compiled artifact (the JIT module + native entries).
        compiled: CompiledFunctions,
    },
}

impl NativeBackend {
    /// Build the tier's backend, compile `module` **once** and return both the
    /// holder and the one-time compile latency.
    ///
    /// Only called for a tier [`Tier::is_available`] reports, so the interpreter —
    /// which has nothing to compile — never reaches this function.
    fn compile(tier: Tier, module: &Module) -> Result<(NativeBackend, Duration), BeamError> {
        match tier {
            Tier::BaselineJit => {
                let mut backend = BaselineJit::new();
                let start = Instant::now();
                let compiled = backend.compile(module)?;
                let compile = start.elapsed();
                Ok((NativeBackend::Baseline { backend, compiled }, compile))
            }
            #[cfg(feature = "jit-cranelift")]
            Tier::Cranelift => {
                let mut backend = CraneliftJit::new();
                let start = Instant::now();
                let compiled = backend.compile(module)?;
                let compile = start.elapsed();
                Ok((NativeBackend::Cranelift { backend, compiled }, compile))
            }
            other => panic!("no native backend for tier {}", other.name()),
        }
    }

    /// Run the exported `function/arity` against the **already-compiled** artifact
    /// (no recompilation).
    fn run_compiled(
        &self,
        machine: &mut Machine,
        module: &Module,
        function: Atom,
        arity: u32,
        args: &[Eterm],
        ctx: &mut MinimalContext,
    ) -> Result<Eterm, BeamError> {
        match self {
            NativeBackend::Baseline { backend, compiled } => {
                backend.run_compiled(compiled, machine, module, function, arity, args, ctx)
            }
            #[cfg(feature = "jit-cranelift")]
            NativeBackend::Cranelift { backend, compiled } => {
                backend.run_compiled(compiled, machine, module, function, arity, args, ctx)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The timing harness
// ---------------------------------------------------------------------------

/// One `(workload, tier)` measurement.
struct Measurement {
    /// The timed tier.
    tier: Tier,
    /// The number of timed runs.
    iterations: u32,
    /// The total wall-clock time of the timed runs.
    elapsed: Duration,
    /// The **one-time** [`JitBackend::compile`] latency that produced the artifact
    /// the timed loop ran against: `None` for the interpreter, `Some(compile)` for a
    /// native tier. It is *excluded* from `elapsed` (compilation happens once,
    /// before the warm-up), and is printed only so the reader can see what was
    /// excluded.
    compile: Option<Duration>,
    /// The last run's result.
    result: Eterm,
}

/// Warm up once, then time `iterations` executions of `workload` on `tier`.
///
/// A native tier compiles **once** here (timed separately, outside the loop) and
/// then only ever calls [`JitBackend::run_compiled`].
fn measure(tier: Tier, workload: &Workload) -> Measurement {
    let iterations = workload.kind.iterations();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(workload.bif_hook());

    // Every entry `reset()`s the machine at the start of a run, so one machine can
    // be reused across the whole loop (the timed cost is the call, not the setup).
    match tier {
        Tier::Interpreter => {
            machine
                .run_export(
                    &workload.module,
                    workload.function,
                    workload.arity,
                    &workload.args,
                    &mut ctx,
                )
                .expect("the workload must run on the interpreter");
            let start = Instant::now();
            let mut result = make_small(0);
            for _ in 0..iterations {
                result = machine
                    .run_export(
                        &workload.module,
                        workload.function,
                        workload.arity,
                        &workload.args,
                        &mut ctx,
                    )
                    .expect("the workload must run on the interpreter");
            }
            Measurement {
                tier,
                iterations,
                elapsed: start.elapsed(),
                compile: None,
                result,
            }
        }
        native => {
            // The one compile this tier pays: measured once, *before* the warm-up so
            // it is not in `elapsed`. (The very first compile in a process also pays
            // a one-off `mmap`/`mprotect` of a fresh code buffer; that is part of
            // the one-time cost being reported.)
            let (backend, compile) = NativeBackend::compile(native, &workload.module)
                .expect("the workload must compile on the native tier");

            backend
                .run_compiled(
                    &mut machine,
                    &workload.module,
                    workload.function,
                    workload.arity,
                    &workload.args,
                    &mut ctx,
                )
                .expect("the workload must run on the native tier");

            let start = Instant::now();
            let mut result = make_small(0);
            for _ in 0..iterations {
                result = backend
                    .run_compiled(
                        &mut machine,
                        &workload.module,
                        workload.function,
                        workload.arity,
                        &workload.args,
                        &mut ctx,
                    )
                    .expect("the workload must run on the native tier");
            }
            Measurement {
                tier,
                iterations,
                elapsed: start.elapsed(),
                compile: Some(compile),
                result,
            }
        }
    }
}

/// Print one measurement line, plus the native tiers' one-time compile cost.
fn report(measurement: &Measurement) {
    let per_op = measurement.elapsed.as_secs_f64() / f64::from(measurement.iterations);
    let per_op_ns = per_op * 1e9;
    println!(
        "jit_bench | tier={:<12} iterations={:<6} total={:>9.3} ms | {:>14.1} ns/op | {:>14.1} ops/s",
        measurement.tier.name(),
        measurement.iterations,
        measurement.elapsed.as_secs_f64() * 1e3,
        per_op_ns,
        if per_op > 0.0 { 1.0 / per_op } else { f64::INFINITY },
    );
    if let Some(compile) = measurement.compile {
        println!(
            "jit_bench |   {}: one-time compile = {:.3} us (excluded from the timed loop; \
             the loop is {} run_compiled calls against the artifact that compile produced)",
            measurement.tier.name(),
            compile.as_secs_f64() * 1e6,
            measurement.iterations,
        );
    }
}

/// The tiers to benchmark: the interpreter plus every native tier this build has.
fn benchmarked_tiers() -> Vec<Tier> {
    let mut tiers = vec![Tier::Interpreter];
    for tier in Tier::ALL {
        if tier == Tier::Interpreter || !tier.is_available() {
            continue;
        }
        // The Cranelift backend only exists with the feature; without it the tier is
        // not in the roster anyway, so this only keeps `NativeBackend::compile` total.
        #[cfg(not(feature = "jit-cranelift"))]
        if tier == Tier::Cranelift {
            continue;
        }
        tiers.push(tier);
    }
    tiers
}

/// Time `workload` on every available tier, assert tier agreement, print the table.
fn bench_workload(workload: &Workload) {
    let tiers = benchmarked_tiers();
    println!(
        "jit_bench | workload = {} | tiers = {:?}",
        workload.label,
        tiers.iter().map(Tier::name).collect::<Vec<_>>()
    );

    let mut reference: Option<Measurement> = None;
    for tier in tiers {
        assert!(
            available_tiers().contains(&tier.name()),
            "benchmarked tier {} must be in the build roster",
            tier.name()
        );
        let measurement = measure(tier, workload);
        report(&measurement);
        match &reference {
            None => reference = Some(measurement),
            Some(expected) => assert_eq!(
                measurement.result,
                expected.result,
                "tier {} disagrees with {} on {}",
                tier.name(),
                expected.tier.name(),
                workload.label,
            ),
        }
    }
    println!();
}

#[test]
fn interpreter_and_native_tiers_agree_and_report_timings() {
    // One test (not several) so the workload timings never run concurrently and
    // contaminate each other; every workload is still measured and asserted.
    let fib = fib_workload();
    let kernel = kernel_workload();
    let wide = wide_workload();
    let aggr = aggregate_workload();
    let cmp = compare_workload();
    // The aggregate kernel's shape is pinned: `x(0)` arrives as `1` and every
    // multiplier the reads produce is `1`, so the interpreter's reference result is
    // exactly `1` (a taken fail arm would make it `-1`, and a non-small operand would
    // raise instead of returning).
    assert_eq!(
        measure(Tier::Interpreter, &aggr).result,
        make_small(1),
        "the aggregate kernel's reference result must be the bounded accumulator"
    );
    // The comparison kernel's shape is pinned the same way: its trailing comparison is
    // `x(2)` (3) `<` `x(1)` (20), so the interpreter's reference result must be the
    // module's own `true` atom — a wrong operand order, a wrong operator, or a
    // fabricated boolean would not be.
    assert_eq!(
        measure(Tier::Interpreter, &cmp).result,
        make_atom(atom(&cmp.module, "true")),
        "the comparison kernel's reference result must be the module's `true` atom"
    );
    for workload in [&fib, &kernel, &wide, &aggr, &cmp] {
        bench_workload(workload);
    }
}
