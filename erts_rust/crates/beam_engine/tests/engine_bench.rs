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

//! **Micro-benchmark: the interpreter's `ns` per interpreted instruction.**
//!
//! `tests/jit_bench.rs` answers "how does a native tier compare with the
//! interpreter?"; this file answers the complementary question the interpreter
//! itself never had a number for: **how long does one dispatch step cost on the
//! `Machine::run_export` direct-threaded interpreter?** It is dependency-free
//! (no `criterion`), prints its table under `--nocapture`, and asserts only
//! *correctness* + *structural* invariants — never a performance threshold, which
//! would be flaky in a shared sandbox.
//!
//! # Method — wall clock divided by a known instruction count
//!
//! For every workload we know the **exact number of instructions one run
//! executes** and so can divide:
//!
//! ```text
//!   ns/instruction = elapsed_wall_clock_ns / (iterations * instructions_per_run)
//! ```
//!
//! Each workload is one hand-built [`Module`] (or one committed `erlc` fixture)
//! with a single exported function. The workload is run once untimed (warm-up),
//! then [`SAMPLES`] times for [`iterations`](Workload::iterations) runs each; the
//! **median** sample is reported (and the best sample, so the noise spread is
//! visible). A `Machine` is reused across the whole timing loop — `run_export`
//! `reset()`s at the start of each run, so the timed cost is the call, not
//! `Machine` construction.
//!
//! ## Why the workloads are shaped the way they are
//!
//! `run_export` pays a **fixed per-run setup** that is *not* per-instruction
//! work: `Machine::reset` (the x-register and float-register files), the bottom
//! frame, and — twice per run (`run_label_outcome`'s `resolve_label` and then
//! `run_loop`) — a **module-wide `Label -> (func, pc)` map rebuilt from every
//! function's label table** (`interpreter::build_label_map`). That build is
//! `O(labels in the module)`, so a module with a thousand labels measures its
//! label-map build, not its dispatch loop. Two consequences shape the design:
//!
//! 1. every **kind-isolating** body keeps the label count tiny (the pure no-op
//!    baseline repeats *one* redundant label instruction, which the engine
//!    dispatches as an ordinary no-op but which leaves the label map 2 entries
//!    wide — see [`bench_dispatch_noop`]), so the per-instruction numbers isolate
//!    dispatch; and
//! 2. the setup cost is itself **measured**, by the [`bench_setup_empty`] floor
//!    and by [`bench_dispatch_many_labels`], a body identical to the no-op baseline
//!    except that every no-op carries a *distinct* label — the difference between
//!    the two is the label-map build, i.e. the per-label cost of `run_export`.
//!
//! ## Workloads
//!
//! | workload | body (per run) | instruction kinds isolated |
//! |---|---|---|
//! | [`bench_setup_empty`] | `label; return` | the per-run setup floor (2 instructions) |
//! | [`bench_dispatch_noop`] | *K* redundant labels | pure dispatch (the baseline) |
//! | [`bench_dispatch_many_labels`] | *K* **distinct** labels | the label-map build |
//! | [`bench_move`] | *K* `move` | operand read/write |
//! | [`bench_test_nobranch`] | *K* passing `test` | test evaluation (predicate) |
//! | [`bench_gcbif`] | *K* small-int `gc_bif '+'` | per-BIF argument marshalling |
//! | [`bench_ctrl`] | `allocate;jump;` *R*&times;`call;return;` `deallocate;return` | call/return frame discipline |
//! | [`bench_branch_loop`] | a counted loop: `test` guard + `jump` back-edge | a taken branch (the `test` fail arm) |
//! | [`bench_tail_loop`] | a tail-recursive loop: `test` guard + `call_only` | tail-call back-edge |
//! | [`bench_gate_call_return`] | the same `call;return` chain, tracing OFF | the A.8 call/return gate (its disabled path) |
//! | [`bench_gate_call_only`] | the same `call_only` tail loop, tracing OFF | the A.8 `call_only` gate (its disabled path) |
//! | [`bench_list_walk`] | the body-recursive `sum([H\|T]) -> H + sum(T)` over a 1,000,000-element list | the per-call/per-return frame discipline on a BIF-free walk |
//! | [`bench_real_even`] | the committed `tailcall.beam`'s `even/1` | a real `erlc`-compiled recursion |
//!
//! Every body is designed so its executed instruction count is known **by
//! construction** (the straight-line bodies count as `code.len()`; the loop bodies
//! as `rounds * per_iteration + per_base`). The one real workload's per-pass count
//! is **derived from the loaded IR** by [`tail_loop_shape`] and its number of
//! passes is **counted by the engine itself** ([`CountingHost`] tallies the one
//! `gc_bif '-'/2` each recursive pass makes), and the derived shape is pinned so a
//! fixture that drifts fails loudly instead of silently reporting a wrong divisor.
//!
//! ## What is measured vs inferred
//!
//! * **Measured**: the wall-clock `ns/instruction` of every workload (median of
//!   [`SAMPLES`] samples), and — from the difference between the no-op and
//!   many-labels baselines — the per-run per-label setup cost.
//! * **Inferred**: the per-instruction-kind attribution, from the *difference*
//!   between a kind-isolating workload and the no-op baseline (see
//!   [`report_attribution`]). A difference isolates the marginal cost of that
//!   kind only to the extent the two bodies are otherwise alike; the loop and
//!   control-flow workloads mix kinds, so their deltas are indicative, not exact.
//!
//! Run it with:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test engine_bench -- --nocapture
//! ```
//!
//! The real workload is driven through the shared multi-module test host
//! (`tests/support`, the same one `tests/stdlib_lists.rs` uses). Its fixture is a
//! **tracked file of this repository** and loading it is a pure in-process `.beam`
//! parse, so there is no optional dependency to skip on: a load failure, a shape
//! drift or a run error is a hard failure (see [`bench_real_even`]).

mod support;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use beam_common::{BeamError, Result};
use beam_engine::{ExecutionContext, Machine, MinimalContext};
use beam_loader::{
    Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand, TestKind, NO_FAIL,
};
use beam_term::{is_small, make_small, signed_value, Atom, Eterm, NIL};

use support::ModuleHost;

// ---------------------------------------------------------------------------
// Shape of the measurement
// ---------------------------------------------------------------------------

/// The number of timed samples per workload; the report quotes the **median**.
const SAMPLES: usize = 5;

/// The number of straight-line instructions in each kind-isolating body (the
/// `label; return` wrapper adds two more to the executed count).
const KIND_OPS: usize = 2048;

/// The argument `even/1` is called with for the real workload. Must be **even**
/// so the mutually-recursive `even/1`&harr;`odd/1` chain terminates on `true`.
const EVEN_N: i64 = 340;

/// The committed `erlc` fixture the real workload is loaded from.
const TAILCALL_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/tailcall.beam"
);

/// The directory the shared test host falls back to when a module is not
/// registered explicitly (never used here: `tailcall` is registered by path).
const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

// ---------------------------------------------------------------------------
// The BIF hook (the interpreter's `ctx.call_bif`)
// ---------------------------------------------------------------------------

/// `erlang:'+'/2` and `erlang:'-'/2` over small integers — the same
/// implementation `tests/slice.rs` uses for its oracle fixture.
///
/// It is reached by exactly the workloads whose body contains a `gc_bif`
/// ([`bench_gcbif`], [`bench_branch_loop`], [`bench_tail_loop`], and the real
/// `tailcall.beam`, whose `loop/2` decrements its counter with `-`). Every other
/// workload never calls it; a `BadArg` from a workload that should not have
/// called a BIF surfaces loudly through [`measure`]'s `expect`.
fn arith_hook(module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
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
        _ => Err(BeamError::BadArg),
    }
}

// ---------------------------------------------------------------------------
// Synthetic-module scaffolding
// ---------------------------------------------------------------------------

/// Wrap `body` in a one-function module exported as `name/arity`:
/// `[Label(entry)] ++ body ++ [Return]`, the entry label being `Label(1)`.
///
/// The caller prepares `module` beforehand when the body needs imports (the
/// `gc_bif` workloads), so this helper only appends the function, its label map
/// and the export. `Function::blocks` is left empty: the interpreter does not
/// consult it (only a native tier compiles blocks).
fn finish_module(mut module: Module, name: &str, arity: u32, body: Vec<Instr>) -> (Module, Atom) {
    let function = module.atom_table.intern(name);
    module.name = function;
    let entry = Label(1);
    let mut code = Vec::with_capacity(body.len() + 2);
    code.push(Instr::Label(entry));
    code.extend(body);
    code.push(Instr::Return);
    let labels = collect_labels(&code);
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name: function,
        arity,
        code,
        blocks: Vec::new(),
        labels,
    });
    module.exports.push(Export {
        function,
        arity,
        label: entry,
    });
    (module, function)
}

/// The module-wide `Label -> code offset` map the engine rebuilds per run.
///
/// Duplicate `Instr::Label`s collapse (last wins), which is exactly what the
/// no-op baseline relies on (see [`bench_dispatch_noop`]).
fn collect_labels(code: &[Instr]) -> BTreeMap<Label, usize> {
    code.iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect()
}

/// Intern `"erlang"`/`name` into `module` and register the `name/2` import,
/// returning the [`ImportRef`] the body's `gc_bif` needs.
fn intern_arith_import(module: &mut Module, name: &str) -> ImportRef {
    let erlang = module.atom_table.intern("erlang");
    let function = module.atom_table.intern(name);
    module.imports.push(Import {
        module: erlang,
        function,
        arity: 2,
    });
    ImportRef((module.imports.len() - 1) as u32)
}

/// A straight-line `move`-heavy body: `x0 <-> x1`, so `x0` is unchanged (still
/// the call argument) and every instruction writes a register.
fn move_body() -> Vec<Instr> {
    let mut body = Vec::with_capacity(KIND_OPS);
    for step in 0..KIND_OPS {
        body.push(if step % 2 == 0 {
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::X(1),
            }
        } else {
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            }
        });
    }
    body
}

// ---------------------------------------------------------------------------
// The workloads
// ---------------------------------------------------------------------------

/// The **setup floor**: an empty body, so one run executes exactly two
/// instructions (`label`, `return`) and everything measured above that is
/// `run_export`'s fixed per-run cost (reset + two label-map builds + the bottom
/// frame). It is the number to subtract from the other workloads' raw `ns/run`.
fn bench_setup_empty() -> Report {
    let (module, function) = finish_module(Module::default(), "bench_empty", 2, Vec::new());
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let report = measure("setup/empty", 2, 20_000, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(
        report.result,
        make_small(1),
        "the empty body must return its first argument"
    );
    report
}

/// The **dispatch baseline**: [`KIND_OPS`] redundant `label` instructions.
///
/// Every one is dispatched by the core loop (a `label` is a metadata no-op) and
/// the body has no branches, so the run executes exactly `KIND_OPS + 2`
/// instructions. The trick is that all *K* labels are the **same** label, so the
/// per-run label map stays 2 entries wide and the measurement isolates dispatch
/// rather than the `O(labels)` map build ([`bench_dispatch_many_labels`] measures
/// that separately).
fn bench_dispatch_noop() -> Report {
    let shared = Label(2);
    let body = vec![Instr::Label(shared); KIND_OPS];
    let (module, function) = finish_module(Module::default(), "bench_noop", 2, body);
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let report = measure("dispatch/noop (shared label)", KIND_OPS + 2, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(1));
    report
}

/// The **label-map probe**: [`KIND_OPS`] *distinct* label instructions.
///
/// Its body is instruction-for-instruction the **same shape and length** as
/// [`bench_dispatch_noop`] (a sequence of `label` no-ops, `KIND_OPS + 2`
/// instructions per run, the same iteration count), but each carries a label of
/// its own, so the module has `KIND_OPS + 1` labels and the two per-run
/// `build_label_map` calls must build that many `BTreeMap` entries. Because the
/// two workloads execute the same number of instructions, the difference between
/// their `ns/run` **is** the per-run label-map build cost (no dispatch-volume
/// confound) — reported by [`report_attribution`].
fn bench_dispatch_many_labels() -> Report {
    let body: Vec<Instr> = (0..KIND_OPS)
        .map(|index| Instr::Label(Label(2 + index as u32)))
        .collect();
    let (module, function) = finish_module(Module::default(), "bench_many_labels", 2, body);
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let report = measure(
        "dispatch/many-labels",
        KIND_OPS + 2,
        800,
        || machine.run_export(&module, function, 2, &args, &mut ctx),
    );
    assert_eq!(report.result, make_small(1));
    report
}

/// A straight-line `move` body (operand read + write + store).
fn bench_move() -> Report {
    let (module, function) = finish_module(Module::default(), "bench_move", 2, move_body());
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let report = measure("move/2", KIND_OPS + 2, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(
        report.result,
        make_small(1),
        "the move body leaves x0 untouched"
    );
    report
}

/// A straight-line body of **passing** tests: `is_integer(x0)` on a small-int
/// register succeeds, so the fail arm is never taken and the body is
/// straight-line. This isolates the *test evaluation* cost (the predicate) from
/// the *taken branch* cost ([`bench_branch_loop`]).
fn bench_test_nobranch() -> Report {
    let body = vec![
        Instr::Test {
            kind: TestKind::IsInteger,
            fail: NO_FAIL,
            args: vec![Operand::X(0)],
        };
        KIND_OPS
    ];
    let (module, function) = finish_module(Module::default(), "bench_test", 2, body);
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let report = measure("test/2 (no branch)", KIND_OPS + 2, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(1));
    report
}

/// A straight-line `gc_bif '+'/2` chain: `x0 = x0 + 1`, [`KIND_OPS`] times.
///
/// Every call goes through `ExecutionContext::call_bif` (the argument `Vec` +
/// the `Box`ed hook), so this isolates the **per-BIF marshalling** cost. The
/// result is `1 + KIND_OPS`, which also proves every one of the [`KIND_OPS`] BIFs
/// actually ran.
fn bench_gcbif() -> Report {
    let mut module = Module::default();
    let plus = intern_arith_import(&mut module, "+");
    let body = vec![
        Instr::GcBif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            live: 2,
            args: vec![Operand::X(0), Operand::Integer(1)],
            dst: Operand::X(0),
        };
        KIND_OPS
    ];
    let (module, function) = finish_module(module, "bench_gcbif", 2, body);
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let report = measure("gc_bif '+'/2", KIND_OPS + 2, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(
        report.result,
        make_small(1 + KIND_OPS as i64),
        "every gc_bif must have added one"
    );
    report
}

/// The number of `call`/`return` rounds in the chain built by [`ctrl_module`],
/// shared by [`bench_ctrl`] and its gate-isolation twin [`bench_gate_call_return`].
const CTRL_ROUNDS: usize = 680;

/// The **`call`/`return` chain body** shared by [`bench_ctrl`] and
/// [`bench_gate_call_return`]: `allocate; jump Ltop; label Ltop;` then
/// [`CTRL_ROUNDS`] `call Lsub`, then `deallocate; return; label Lsub; return`.
///
/// A non-tail `call` writes its continuation into the current frame and jumps; the
/// callee's `return` reads it back — so each round costs `call`, `label`, `return`,
/// and the whole body is exactly `3 * ROUNDS + 6` instructions. The `allocate`
/// guarantees the `call` writes *its own* frame's continuation slot rather than the
/// bottom frame's `Halt`, so the trailing `deallocate; return` reaches `Halt` and
/// finishes.
fn ctrl_body() -> Vec<Instr> {
    let ltop = Label(2);
    let lsub = Label(3);
    let mut body = vec![
        Instr::Allocate {
            stack_need: HeapNeed::Words(0),
            live: 1,
        },
        Instr::Jump { target: ltop },
        Instr::Label(ltop),
    ];
    for _ in 0..CTRL_ROUNDS {
        body.push(Instr::Call {
            arity: 0,
            target: lsub,
        });
    }
    body.push(Instr::Deallocate(0));
    body.push(Instr::Return);
    body.push(Instr::Label(lsub));
    body.push(Instr::Return);
    body
}

/// A one-function module exported as `name/2` whose body is [`ctrl_body`]; returns
/// the module, its function atom and the exact per-run instruction count.
fn ctrl_module(name: &str) -> (Module, Atom, usize) {
    let (module, function) = finish_module(Module::default(), name, 2, ctrl_body());
    (module, function, 3 * CTRL_ROUNDS + 6)
}

/// A **control-flow chain**: `allocate; jump;` then `ROUNDS` &times;
/// `call(Lsub); label(Lsub); return`, then `deallocate; return`.
///
/// A non-tail `call` writes its continuation into the current frame and jumps;
/// the callee's `return` reads it back — so each round costs `call`, `label`,
/// `return`, and the whole body is exactly `3 * ROUNDS + 6` instructions. The
/// `allocate` guarantees the `call` writes *its own* frame's continuation slot
/// rather than the bottom frame's `Halt`, so the trailing `deallocate; return`
/// reaches `Halt` and finishes.
fn bench_ctrl() -> Report {
    let (module, function, instructions) = ctrl_module("bench_ctrl");
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let report = measure("ctrl/2 (call/return chain)", instructions, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(1));
    report
}

/// **Gate-isolation row (A.8): the `call`/`return` chain with the tracing gate OFF.**
///
/// By construction this is [`bench_ctrl`]'s body — the densest local
/// `call`+`return` chain the frame discipline allows (one `call` and one `return`
/// per three instructions) — so every instruction it dispatches that carries the
/// per-run tracing gate is exercised `680` times per run. It is measured with the
/// default untraced [`MinimalContext`], i.e. on the gate's **disabled** path (the
/// path the per-run `Machine::trace_on` cache exists to make free): the untraced
/// `trace_enabled()` answer is latched once per run and each call/return then reads
/// a plain `bool`, so this row's `ns/instruction` must match [`bench_ctrl`]'s — and
/// the pre-change `ctrl/2` figure — rather than pay a vtable call per instruction.
fn bench_gate_call_return() -> Report {
    let (module, function, instructions) = ctrl_module("bench_gate_ctrl");
    let args = [make_small(1), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let report = measure("gate/call+return", instructions, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(report.result, make_small(1));
    report
}

/// A **counted loop whose branch is taken**: the loop guard is a `test` whose
/// fail arm jumps to the loop body, and the body ends in an unconditional `jump`
/// back to the guard.
///
/// ```text
///   label(E)                                  ; entry
/// label(Ltop)                                 ; <- the back-edge target
///   test is_eq_exact x0 0 -> Lbody            ; the *conditional* jump
///   jump Lend
/// label(Lbody)
///   gc_bif '-' x0 = x0 - 1
///   gc_bif '+' x1 = x1 + 1
///   jump Ltop
/// label(Lend)
///   move x1 -> x0
///   return
/// ```
///
/// Per iteration: `label`, `test`, `label`, two `gc_bif`s, `jump` (6
/// instructions); the terminating pass adds `label`, `test`, `jump`, `label`,
/// `move`, `return` (6), plus the entry `label`. Total `6 * N + 7`, and `x1`
/// counts the iterations, so the result (`N`) proves the loop ran exactly `N`
/// times.
fn bench_branch_loop() -> Report {
    const N: i64 = 340;
    let ltop = Label(2);
    let lbody = Label(3);
    let lend = Label(4);
    let mut module = Module::default();
    let minus = intern_arith_import(&mut module, "-");
    let plus = intern_arith_import(&mut module, "+");
    let body = vec![
        Instr::Label(ltop),
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: lbody,
            args: vec![Operand::X(0), Operand::Integer(0)],
        },
        Instr::Jump { target: lend },
        Instr::Label(lbody),
        Instr::GcBif {
            arity: 2,
            bif: minus,
            fail: NO_FAIL,
            live: 2,
            args: vec![Operand::X(0), Operand::Integer(1)],
            dst: Operand::X(0),
        },
        Instr::GcBif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            live: 2,
            args: vec![Operand::X(1), Operand::Integer(1)],
            dst: Operand::X(1),
        },
        Instr::Jump { target: ltop },
        Instr::Label(lend),
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
    ];
    let (module, function) = finish_module(module, "bench_branch_loop", 2, body);
    let args = [make_small(N), make_small(0)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let instructions = 6 * N as usize + 7;
    let report = measure("branch-loop/2 (test + jump)", instructions, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(
        report.result,
        make_small(N),
        "the accumulator must count all {N} iterations"
    );
    report
}

/// The number of recursive passes in the `call_only` tail loop built by
/// [`tail_loop_module`], shared by [`bench_tail_loop`] and its gate-isolation twin
/// [`bench_gate_call_only`]. Must be non-negative (it is the guard argument).
const TAIL_LOOP_N: i64 = 409;

/// A one-function **`call_only` tail-recursive loop** module exported as `name/2`,
/// shared by [`bench_tail_loop`] and [`bench_gate_call_only`].
///
/// ```text
///   label(E)
///   test is_eq_exact x0 0 -> Lbody
///   move x1 -> x0
///   return
/// label(Lbody)
///   gc_bif '-' x0 = x0 - 1
///   call_only E
/// ```
///
/// Per recursive pass: `label`, `test`, `label`, `gc_bif`, `call_only` (5); the
/// base pass is `label`, `test`, `move`, `return` (4). Total `5 * N + 4`, and the
/// result is the second argument carried through untouched.
fn tail_loop_module(name: &str) -> (Module, Atom) {
    let entry = Label(1);
    let lbody = Label(2);
    let mut module = Module::default();
    let minus = intern_arith_import(&mut module, "-");
    let body = vec![
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: lbody,
            args: vec![Operand::X(0), Operand::Integer(0)],
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(lbody),
        Instr::GcBif {
            arity: 2,
            bif: minus,
            fail: NO_FAIL,
            live: 2,
            args: vec![Operand::X(0), Operand::Integer(1)],
            dst: Operand::X(0),
        },
        Instr::CallOnly {
            arity: 2,
            target: entry,
        },
    ];
    finish_module(module, name, 2, body)
}

/// A **tail-recursive loop** in the exact shape `tailcall.beam`'s `loop/2` uses:
/// a `test` guard whose fail arm is the recursive arm, and a `call_only` back to
/// the entry label (see [`tail_loop_module`]).
fn bench_tail_loop() -> Report {
    let (module, function) = tail_loop_module("bench_tail_loop");
    let args = [make_small(TAIL_LOOP_N), make_small(7)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let (rec, base) = tail_loop_shape(&module.functions[0]).expect("the tail loop shape");
    assert_eq!(
        (rec, base),
        (5, 4),
        "the synthetic tail loop's per-pass counts are known by construction"
    );
    let instructions = TAIL_LOOP_N as usize * rec + base;
    let report = measure("tail-loop/2 (test + call_only)", instructions, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(
        report.result,
        make_small(7),
        "the tail loop returns the untouched accumulator"
    );
    report
}

/// **Gate-isolation row (A.8): the `call_only` tail loop with the tracing gate OFF.**
///
/// By construction this is [`bench_tail_loop`]'s body, whose one `call_only`
/// back-edge per recursive pass carries the per-run tracing gate `409` times per
/// run. It is measured with the default untraced [`MinimalContext`], i.e. on the
/// gate's **disabled** path: the untraced `trace_enabled()` answer is latched once
/// per run and each `call_only` then reads a plain `bool`, so this row's
/// `ns/instruction` must match [`bench_tail_loop`]'s — and the pre-change
/// `tail-loop/2` figure — rather than pay a vtable call per instruction.
fn bench_gate_call_only() -> Report {
    let (module, function) = tail_loop_module("bench_gate_tail_loop");
    let args = [make_small(TAIL_LOOP_N), make_small(7)];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let (rec, base) = tail_loop_shape(&module.functions[0]).expect("the tail loop shape");
    assert_eq!(
        (rec, base),
        (5, 4),
        "the gate probe must have the same per-pass shape as the tail loop"
    );
    let instructions = TAIL_LOOP_N as usize * rec + base;
    let report = measure("gate/call_only", instructions, 800, || {
        machine.run_export(&module, function, 2, &args, &mut ctx)
    });
    assert_eq!(
        report.result,
        make_small(7),
        "the gate probe returns the untouched accumulator"
    );
    report
}

/// The element count of the list the **BIF-free list-walk** workload traverses
/// once per run.
const LIST_WALK_N: usize = 1_000_000;

/// Build a `[CAR, CDR]` list of `LIST_WALK_N` small integers (`1` each) in a
/// **term arena** and hand back the arena plus the list term.
///
/// The arena is [`beam_term::Heap`], the fixed-capacity bump term arena: the list
/// is built once, before the timing loop, and stays alive as long as the caller
/// holds the returned `Heap`. The engine reads a list through its tagged
/// cons-cell pointer, so the traversal reads this arena directly and the workload
/// measures the walk, not term construction.
fn list_walk_input(n: usize) -> (beam_term::Heap, Eterm) {
    let mut heap = beam_term::Heap::new(2 * n);
    let mut list = NIL;
    for _ in 0..n {
        list = heap
            .cons(make_small(1), list)
            .expect("the arena is sized for the whole list");
    }
    (heap, list)
}

/// The **body-recursive list walk** `sum([H|T]) -> H + sum(T); sum([]) -> 0.` in
/// the shape the compiler emits for it, exported as `name/1`:
///
/// ```text
///   label(E)                             ; entry == callable entry label
///   test is_nonempty_list -> Lbase [x0]  ; the head-clause guard
///   allocate 1, 1                        ; one Y slot, one live x register
///   get_list x0 -> y0, x0                ; head into y0, tail into x0
///   call 1, E                            ; the recursive call (NOT a tail call)
///   gc_bif '+' 2 [y0, x0] -> x0          ; H + sum(T)
///   deallocate 1
///   return
/// label(Lbase)
///   test is_nil -> Lexit [x0]            ; `{f,1}` in the real module (func_info)
///   move 0 -> x0
///   return
/// label(Lexit)                           ; unreachable for a well-formed walk
///   return
/// ```
///
/// The walk is **BIF-free apart from one small-int `'+'/2` per element**: no
/// `test_heap`/`allocate_heap`, no term is built, and every other instruction is
/// the frame/call machinery itself. The base-case `is_nil` guard's fail label is a
/// bare `return` (the real module's `{f,1}` jumps to `func_info` and raises
/// `function_clause`; this module has no `func_info`, so a malformed walk is made
/// to *terminate with a wrong value* — which the result assertion below catches —
/// rather than to recurse forever).
fn list_walk_module(name: &str) -> (Module, Atom) {
    let entry = Label(1);
    let lbase = Label(3);
    let lexit = Label(4);
    let mut module = Module::default();
    let plus = intern_arith_import(&mut module, "+");
    let body = vec![
        Instr::Test {
            kind: TestKind::IsNonemptyList,
            fail: lbase,
            args: vec![Operand::X(0)],
        },
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 1,
        },
        Instr::GetList {
            source: Operand::X(0),
            head: Operand::Y(0),
            tail: Operand::X(0),
        },
        Instr::Call {
            arity: 1,
            target: entry,
        },
        Instr::GcBif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            live: 1,
            args: vec![
                Operand::Y(0),
                Operand::Typed {
                    reg: beam_loader::Reg::X(0),
                    ty: beam_loader::TypeIndex(0),
                },
            ],
            dst: Operand::X(0),
        },
        Instr::DeallocateReturn(1),
        Instr::Label(lbase),
        Instr::Test {
            kind: TestKind::IsNil,
            fail: lexit,
            args: vec![Operand::X(0)],
        },
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(lexit),
        Instr::Return,
    ];
    finish_module(module, name, 1, body)
}

/// The **per-iteration instruction count** of [`list_walk_module`]'s walk, by
/// construction.
///
/// Per element the walk **descends** `label(E)`, `test`, `allocate`, `get_list`,
/// `call` (5 instructions) and, on the way back out, runs `gc_bif '+'` and the
/// fused `deallocate_return` (2) — `7 * N`; the single base case adds `label(E)`,
/// `test`, `label(Lbase)`, `test`, `move`, `return` (6).
///
/// The synthetic body bypasses the loader, so it spells the frame pop + return as
/// the loader's superinstruction `Instr::DeallocateReturn` directly — the shape the
/// post-fusion loader produces and the interpreter dispatches on.
fn list_walk_instructions(n: usize) -> usize {
    7 * n + 6
}

/// **The BIF-free recursive list walk**: `sum/1` over a 1,000,000-element list,
/// one element per iteration of `test`/`allocate`/`get_list`/`call` going down
/// and `gc_bif`/`deallocate`/`return` coming back up.
///
/// The body is deliberately the cheapest recursive shape the compiler emits: the
/// only work per element other than the frame/call machinery is one small-int
/// `'+'/2`, and the traversal **builds no term and requests no heap**, so what
/// this row measures is dominated by the per-call/per-return engine path rather
/// than by dispatch, BIF marshalling or allocation. It is the interpreter's
/// counterpart to the tail-call rows: 680 `call`/`return` rounds in
/// [`bench_ctrl`] there, one million here.
fn bench_list_walk() -> Report {
    // The arena must outlive every timed run; it is never touched by the walk.
    let (_arena, list) = list_walk_input(LIST_WALK_N);
    let (module, function) = list_walk_module("bench_list_walk");
    let args = [list];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let instructions = list_walk_instructions(LIST_WALK_N);
    let report = measure("list-walk/1 (recursive)", instructions, 1, || {
        machine.run_export(&module, function, 1, &args, &mut ctx)
    });
    assert_eq!(
        report.result,
        make_small(LIST_WALK_N as i64),
        "the walk must add exactly one per element"
    );
    report
}

/// A [`ModuleHost`] wrapper that **counts every BIF call it serves**, so the real
/// workload's instruction divisor is measured by the engine instead of assumed.
///
/// `tailcall:even/1` and its mutual partner `odd/1` each execute exactly one
/// `gc_bif '-'/2` per recursive pass and none on the base pass, so `bif_calls`
/// after a run *is* the number of recursive passes — an independent check on the
/// divisor [`tail_loop_shape`] derives from the IR (and the only way to notice a
/// fixture whose recursion count differs from what was requested).
///
/// Every other [`ExecutionContext`] method forwards to the wrapped host
/// unchanged, so the count is the only observable difference.
struct CountingHost {
    host: ModuleHost,
    /// The number of BIF calls served since the counter was last reset.
    bif_calls: u64,
}

impl CountingHost {
    /// Wrap `host` with a zeroed counter.
    fn new(host: ModuleHost) -> CountingHost {
        CountingHost {
            host,
            bif_calls: 0,
        }
    }
}

impl ExecutionContext for CountingHost {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.bif_calls += 1;
        self.host.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.host.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.host.alloc_words(n)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        self.host.apply_fun(fun, args)
    }

    /// This bench host never pre-empts (it inherits the default no-op `reduce`), so
    /// it opts out of the per-instruction reduction hook like `MinimalContext` — the
    /// measured rows then exercise the driver's `Machine::reduce_on` opt-out path.
    fn wants_reduce(&self) -> bool {
        false
    }
}

/// The **real workload**: the committed `tailcall.beam` fixture's mutually
/// recursive `even/1` (`even(N) -> odd(N-1)`, `odd(N) -> even(N-1)`), driven
/// through the shared multi-module test host (`tests/support`).
///
/// `even/1` is pure recursion + one `gc_bif '-'/2` per step and allocates
/// nothing, so the host's pointer-stable arena does not grow across the timing
/// loop.
///
/// # The divisor
///
/// `total = recursive_passes * rec + base`, where `rec`/`base` are the
/// instructions executed by one recursive pass and by the single base pass,
/// **derived from the loaded IR** by [`tail_loop_shape`] and required to be the
/// same for `even/1` and `odd/1` (the two halves of the recursion). The count of
/// recursive passes is **not assumed**: it is read off [`CountingHost`], which
/// counts the `gc_bif '-'/2` call each pass makes, and asserted to equal
/// [`EVEN_N`]. The derived `rec`/`base` are additionally pinned below, so a
/// fixture that drifted fails the build loudly rather than silently reporting a
/// wrong divisor.
///
/// Returns the report; **every** failure here panics (see below).
///
/// # Why there is no skip
///
/// The fixture is a **tracked file in this repository** and loading it is a pure
/// in-process `.beam` parse ([`ModuleHost::load`]) — no `erl`/`erlc`, no code-path
/// lookup, no optional dependency. There is therefore no "the dependency is
/// absent" state to skip on: a load failure, an unrecognised shape, a shape
/// mismatch or a run error is a broken fixture or an engine regression. This lane
/// exists precisely to measure a real `erlc`-compiled recursion, so quietly not
/// measuring it while the test stayed green was the bug: a `None` here used to
/// fold three distinct harness failures into "no benchmark today".
fn bench_real_even() -> Report {
    let mut inner = ModuleHost::new(FIXTURES_DIR);
    inner.register("tailcall", TAILCALL_BEAM);
    let module = inner.load("tailcall").unwrap_or_else(|error| {
        panic!("cannot load the committed fixture {TAILCALL_BEAM}: {error:?}")
    });
    let expected = inner.intern("true");

    // Both halves of the mutual recursion must have the same per-pass shape, or
    // the single `(rec, base)` pair below would not describe the whole chain.
    let shape_of = |name: &str| -> Option<(usize, usize)> {
        let atom = module.atom_table.lookup(name)?;
        module
            .functions
            .iter()
            .find(|f| f.name == atom && f.arity == 1)
            .and_then(tail_loop_shape)
    };
    let (Some(even_shape), Some(odd_shape)) = (shape_of("even"), shape_of("odd")) else {
        panic!(
            "`even/1`/`odd/1` in {TAILCALL_BEAM} have no recognised loop shape, so the real \
             workload's divisor cannot be derived (there is no safe fallback: a guessed divisor \
             would silently mis-report ns/instruction)"
        );
    };
    assert_eq!(
        even_shape, odd_shape,
        "`even/1` and `odd/1` are the two halves of the same recursion, so they must share a \
         per-pass shape"
    );
    let (rec, base) = even_shape;
    // Pinned: the observed shape of this fixture's two recursion halves. A drift
    // (a recompiled fixture, a changed lowering) therefore fails loudly. Checked
    // *after* the shape and the pass count have been printed, so a failure shows
    // the observed values.
    const EXPECTED_SHAPE: (usize, usize) = (6, 4);

    let function = module
        .atom_table
        .lookup("even")
        .expect("`even` resolved a moment ago");
    let args = [make_small(EVEN_N)];

    // Probe run: the engine itself counts the recursion.
    let mut host = CountingHost::new(inner);
    let mut machine = Machine::new();
    host.bif_calls = 0;
    let probe = machine
        .run_export(&module, function, 1, &args, &mut host)
        .unwrap_or_else(|error| panic!("tailcall:even({EVEN_N}) failed to run: {error:?}"));
    assert_eq!(
        probe, expected,
        "tailcall:even({EVEN_N}) must return true (the oracle's value)"
    );
    let recursive_passes = host.bif_calls;
    assert_eq!(
        recursive_passes, EVEN_N as u64,
        "one `gc_bif '-'/2` per recursive pass, so even({EVEN_N}) must perform {EVEN_N} of them"
    );

    let instructions = recursive_passes as usize * rec + base;
    eprintln!(
        "engine_bench | real workload divisor: even/1 == odd/1 shape (rec={rec}, base={base}) x \
         {recursive_passes} recursive passes + 1 base pass = {instructions} instructions/run"
    );
    assert_eq!(
        (rec, base),
        EXPECTED_SHAPE,
        "tailcall.beam's even/1 & odd/1 must keep their pinned per-pass shape"
    );
    let report = measure(
        format!("real tailcall:even/1 ({EVEN_N})"),
        instructions,
        400,
        || {
            host.bif_calls = 0;
            machine.run_export(&module, function, 1, &args, &mut host)
        },
    );
    assert_eq!(
        report.result, expected,
        "tailcall:even({EVEN_N}) must return true (the oracle's value)"
    );
    assert_eq!(
        host.bif_calls, EVEN_N as u64,
        "every timed run must perform the same {EVEN_N} recursive passes"
    );
    report
}

// ---------------------------------------------------------------------------
// Deriving a loop's instruction count from the IR
// ---------------------------------------------------------------------------

/// Derive the `(per_recursive_pass, per_base_pass)` instruction counts of a
/// simple, **single-guard** recursive function from its loaded code.
///
/// The shape recognised is the one every hand-written Erlang loop compiles to:
/// an entry `label`, a leading `test` whose fail arm is the recursive (or the
/// terminal) arm, one straight-line arm ending in `return`, and the other arm
/// ending in a tail call (`call_only`/`call_last`). Concretely:
///
/// * `entry_at` — the index of `Instr::Label(entry_label)`;
/// * `guard_at` — the first `Test` at or after `entry_at`, and `fail_at` its
///   fail label's index;
/// * `base_len` — `entry_at ..= the first Return after the guard`, inclusive;
/// * `rec_len` — `fail_at ..= the first tail call` plus the two entry-path
///   instructions (`entry_at ..= guard_at`) the recursive pass also executes.
///
/// `None` means the shape was not recognised (a caller must then skip loudly, not
/// guess a divisor). The derived values are pinned at each call site so a fixture
/// that drifts fails the build.
fn tail_loop_shape(function: &Function) -> Option<(usize, usize)> {
    let code = &function.code;
    let entry_at = code
        .iter()
        .position(|instr| matches!(instr, Instr::Label(label) if *label == function.entry_label))?;
    let (guard_at, fail_label) = code.iter().enumerate().skip(entry_at).find_map(|(index, instr)| {
        match instr {
            Instr::Test { fail, .. } if *fail != NO_FAIL => Some((index, *fail)),
            _ => None,
        }
    })?;
    let fail_at = *function.labels.get(&fail_label)?;
    let return_at = code
        .iter()
        .enumerate()
        .skip(guard_at)
        .find_map(|(index, instr)| matches!(instr, Instr::Return).then_some(index))?;
    let call_at = code.iter().enumerate().skip(entry_at).find_map(|(index, instr)| {
        matches!(instr, Instr::CallOnly { .. } | Instr::CallLast { .. }).then_some(index)
    })?;
    let base_len = return_at - entry_at + 1;
    let rec_len = (call_at - fail_at + 1) + (guard_at - entry_at + 1);
    Some((rec_len, base_len))
}

/// Wrap raw `code` in the [`Function`] value [`tail_loop_shape`] takes (the
/// negative/positive controls below build shapes by hand, without a module).
fn probe_function(code: Vec<Instr>) -> Function {
    let mut module = Module::default();
    let name = module.atom_table.intern("probe");
    module.name = name;
    let entry_label = Label(1);
    let labels = collect_labels(&code);
    Function {
        func_label: entry_label,
        entry_label,
        module: name,
        name,
        arity: 1,
        code,
        blocks: Vec::new(),
        labels,
    }
}

/// The can-fail control for [`tail_loop_shape`], the one helper the **real**
/// workload's divisor is derived from.
///
/// `None` is the helper's "not the recognised loop shape" answer, and its callers
/// treat that as the *only* safe outcome (they refuse to guess a divisor). An
/// `Option`-returning classifier whose `None` arm is never reached is a
/// cannot-fail guard — and the real lane's whole divisor would then rest on an
/// unexercised branch. So this asserts both arms: three shapes that must be
/// **rejected**, and one that must be decomposed **exactly** as documented.
///
/// It is a plain function (not a `#[test]`) called from the single test in this
/// file: this file deliberately runs one test so no workload ever times
/// concurrently with another, and this control does no timing.
fn assert_tail_loop_shape_classifier() {
    let guard = |fail: Label| Instr::Test {
        kind: TestKind::IsEqExact,
        fail,
        args: vec![Operand::X(0), Operand::Integer(0)],
    };
    let tail = || Instr::CallOnly {
        arity: 1,
        target: Label(1),
    };

    assert_eq!(
        tail_loop_shape(&probe_function(vec![
            Instr::Label(Label(1)),
            Instr::Return,
        ])),
        None,
        "a body with no `test` at all is not a guarded loop"
    );
    assert_eq!(
        tail_loop_shape(&probe_function(vec![
            Instr::Label(Label(1)),
            guard(NO_FAIL),
            Instr::Return,
            tail(),
        ])),
        None,
        "a `test` with fail == NO_FAIL raises instead of branching, so it cannot \
         delimit a recursive arm"
    );
    assert_eq!(
        tail_loop_shape(&probe_function(vec![
            Instr::Label(Label(1)),
            guard(Label(2)),
            Instr::Return,
            Instr::Label(Label(2)),
        ])),
        None,
        "a guarded body with no `call_only`/`call_last` has no recursive pass"
    );

    // The positive arm, decomposed by hand from the documented formula:
    //   entry(0) guard(1) | base = entry..=return(2)                    => base_len = 3
    //   fail(3)  tail(4)  | rec  = fail..=tail (2) + entry..=guard (2)  => rec_len  = 4
    assert_eq!(
        tail_loop_shape(&probe_function(vec![
            Instr::Label(Label(1)),
            guard(Label(2)),
            Instr::Return,
            Instr::Label(Label(2)),
            tail(),
        ])),
        Some((4, 3)),
        "rec = (tail - fail + 1) + (guard - entry + 1), base = return - entry + 1"
    );
}

// ---------------------------------------------------------------------------
// The timing harness
// ---------------------------------------------------------------------------

/// One workload's measurement: the per-sample wall-clock totals and the last
/// run's result (asserted by the workload's builder).
struct Report {
    /// The printed workload name.
    label: String,
    /// The exact number of instructions one run executes.
    instructions_per_run: usize,
    /// The number of runs per sample.
    iterations: u64,
    /// The per-sample wall clock, in sample order.
    samples: Vec<Duration>,
    /// The last run's result.
    result: Eterm,
}

impl Report {
    /// Nanoseconds per **run** for one sample.
    fn ns_per_run(&self, sample: Duration) -> f64 {
        sample.as_secs_f64() * 1e9 / self.iterations as f64
    }

    /// Nanoseconds per **instruction** for one sample.
    fn ns_per_instruction(&self, sample: Duration) -> f64 {
        self.ns_per_run(sample) / self.instructions_per_run as f64
    }

    /// The fastest sample (the least-noise estimate).
    fn best(&self) -> Duration {
        *self
            .samples
            .iter()
            .min()
            .expect("every report has at least one sample")
    }

    /// The median sample.
    fn median(&self) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    }

    /// The reported `ns/instruction` (median sample).
    fn median_ns_per_instruction(&self) -> f64 {
        self.ns_per_instruction(self.median())
    }

    /// The reported `ns/run` (median sample).
    fn median_ns_per_run(&self) -> f64 {
        self.ns_per_run(self.median())
    }
}

/// Warm up once, then time [`SAMPLES`] samples of `iterations` runs each.
///
/// The closure is the workload's run; a `BeamError` panics loudly (a workload the
/// engine cannot execute is a bug in this file, not a benchmark result).
fn measure(
    label: impl Into<String>,
    instructions_per_run: usize,
    iterations: u64,
    mut run: impl FnMut() -> Result<Eterm>,
) -> Report {
    assert!(
        instructions_per_run > 0 && iterations > 0,
        "a report must divide by a positive count"
    );
    let mut result = run().expect("the warm-up run must succeed");
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..iterations {
            result = run().expect("every timed run must succeed");
        }
        samples.push(start.elapsed());
    }
    Report {
        label: label.into(),
        instructions_per_run,
        iterations,
        samples,
        result,
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Print the measurement table (workload, iterations, instructions/run,
/// `ns/run`, `ns/instruction` for the median and best samples).
fn report_table(reports: &[Report]) {
    println!(
        "engine_bench | interpreter `Machine::run_export` — ns per interpreted instruction \
         (median of {SAMPLES} samples; best sample in brackets)"
    );
    println!(
        "engine_bench | {:<28} {:>7} {:>9} {:>12} {:>12} {:>12}",
        "workload", "iters", "instr/run", "ns/run", "ns/instr", "ns/instr(best)"
    );
    for report in reports {
        println!(
            "engine_bench | {:<28} {:>7} {:>9} {:>12.1} {:>12.2} {:>12.2}",
            report.label,
            report.iterations,
            report.instructions_per_run,
            report.median_ns_per_run(),
            report.median_ns_per_instruction(),
            report.ns_per_instruction(report.best()),
        );
    }
    println!();
}

/// Print the per-instruction-kind attribution and the setup decomposition.
///
/// The attribution is a **difference of medians**: each kind-isolating workload's
/// `ns/instruction` minus the pure-dispatch baseline
/// ([`bench_dispatch_noop`]) is the marginal cost of that instruction kind over
/// dispatching a no-op label.
///
/// Both numbers are **floor-adjusted**: the per-run setup cost [`bench_setup_empty`]
/// measures (`Machine::reset` + the bottom frame + the two per-run `build_label_map`
/// calls) is subtracted from every workload's `ns/run` first, because it is a
/// constant per run and would otherwise inflate the cheap workloads' per-instruction
/// figures (it is ~400 ns/run against bodies of 2050 instructions). The residual
/// approximation is the label-count difference (the loop bodies carry four labels,
/// the baseline two), worth a few ns/run.
///
/// The label-map decomposition uses [`bench_dispatch_many_labels`], whose body has
/// the baseline's *exact* instruction count, so their `ns/run` difference is the
/// label-map build alone.
fn report_attribution(reports: &[Report]) {
    let by_label = |needle: &str| reports.iter().find(|r| r.label.starts_with(needle));
    let floor_ns_per_run = by_label("setup/empty")
        .expect("the setup floor is always measured")
        .median_ns_per_run();
    // The per-instruction cost of a workload once its per-run setup floor is removed.
    let adjusted = |report: &Report| -> f64 {
        (report.median_ns_per_run() - floor_ns_per_run).max(0.0)
            / report.instructions_per_run as f64
    };
    let baseline = adjusted(
        by_label("dispatch/noop").expect("the dispatch baseline is always measured"),
    );

    println!(
        "engine_bench | attribution — marginal ns/instruction over the dispatch baseline; every \
         figure is floor-adjusted by the {floor_ns_per_run:.1} ns/run setup floor"
    );
    println!(
        "engine_bench |   pure dispatch (no-op label)        {baseline:>8.2} ns/instr"
    );
    for (needle, note) in [
        ("move/2", "operand read + write + register store"),
        ("test/2", "test predicate evaluation (no branch)"),
        ("gc_bif", "per-BIF argument marshalling through the context"),
        ("ctrl/2", "call/return frame discipline (mixed)"),
        ("gate/call+return", "A.8 call/return gate, tracing OFF (mixed)"),
        ("branch-loop/2", "taken test branch + jump back-edge (mixed)"),
        ("tail-loop/2", "tail-call back-edge (mixed)"),
        ("gate/call_only", "A.8 call_only gate, tracing OFF (mixed)"),
        ("list-walk/1", "recursive list walk: per-call/per-return frame discipline (mixed)"),
        ("real", "a real erlc-compiled recursion (mixed)"),
    ] {
        if let Some(report) = by_label(needle) {
            let ns = adjusted(report);
            println!(
                "engine_bench |   {:<33} {:>8.2} ns/instr  (+{:>7.2} vs baseline)  {}",
                report.label,
                ns,
                ns - baseline,
                note,
            );
        }
    }

    if let (Some(noop), Some(many)) = (
        by_label("dispatch/noop"),
        by_label("dispatch/many-labels"),
    ) {
        // The two bodies have identical instruction counts and iteration counts,
        // so their `ns/run` difference is the label-map build alone. The
        // many-labels body carries `KIND_OPS` distinct body labels plus the entry
        // label; the no-op baseline carries the entry label plus its single
        // *shared* body label.
        assert_eq!(
            noop.instructions_per_run, many.instructions_per_run,
            "the label-map probe must have the baseline's instruction count"
        );
        let many_labels = KIND_OPS + 1;
        let noop_labels = 2;
        let extra_labels = (many_labels - noop_labels) as f64;
        let extra_ns_per_run = many.median_ns_per_run() - noop.median_ns_per_run();
        let per_label_per_run = extra_ns_per_run / extra_labels;
        println!(
            "engine_bench | label-map build: {many_labels} labels vs the baseline's {noop_labels} \
             (+{extra_labels:.0}, the same {instr} instructions/run and {iters} iterations) costs \
             {extra_ns_per_run:+.1} ns/run = {per_label_per_run:.3} ns per extra label per run = \
             {per_call:.3} ns per `build_label_map` call (two calls per run)",
            instr = many.instructions_per_run,
            iters = many.iterations,
            per_call = per_label_per_run / 2.0,
        );
    }
    println!();
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[test]
fn interpreter_reports_ns_per_instruction() {
    // The real lane's divisor comes from a shape classifier; prove that
    // classifier can reject a non-loop before trusting its answer (no timing, so
    // it cannot contaminate the samples below).
    assert_tail_loop_shape_classifier();

    // One test (not several) so the workloads never run concurrently and
    // contaminate each other's timing.
    let mut reports = vec![
        bench_setup_empty(),
        bench_dispatch_noop(),
        bench_dispatch_many_labels(),
        bench_move(),
        bench_test_nobranch(),
        bench_gcbif(),
        bench_ctrl(),
        bench_gate_call_return(),
        bench_branch_loop(),
        bench_tail_loop(),
        bench_gate_call_only(),
        bench_list_walk(),
    ];
    // The real `erlc` fixture lane is unconditional: `bench_real_even` panics
    // rather than skipping, so this lane can never silently vanish.
    reports.push(bench_real_even());

    report_table(&reports);
    report_attribution(&reports);

    // Structural invariants only — never a performance threshold (that would be
    // flaky in a shared sandbox; the printed numbers are the deliverable).
    //
    // The label-set guard below is a *harness* invariant, not a measurement: the
    // table/attribution loops print (and `report_attribution` silently skips) only
    // what they find, so a workload quietly dropping out of the list above would
    // otherwise go unnoticed.
    for label in [
        "setup/empty",
        "dispatch/noop",
        "dispatch/many-labels",
        "move/2",
        "test/2",
        "gc_bif",
        "ctrl/2",
        "gate/call+return",
        "branch-loop/2",
        "tail-loop/2",
        "gate/call_only",
        "list-walk/1",
        "real tailcall:even/1",
    ] {
        assert!(
            reports.iter().any(|r| r.label.starts_with(label)),
            "the workload `{label}` must be measured, measured: {:?}",
            reports.iter().map(|r| r.label.as_str()).collect::<Vec<_>>()
        );
    }
    for report in &reports {
        let ns = report.median_ns_per_instruction();
        assert!(
            ns.is_finite() && ns > 0.0,
            "{} must report a positive ns/instruction, got {ns}",
            report.label
        );
        assert!(
            (0.01..100_000.0).contains(&ns),
            "{} reported an implausible {ns} ns/instruction",
            report.label
        );
    }

    // The setup floor is a real cost: a body of two instructions cannot be
    // faster than a positive wall-clock run, and every straight-line workload
    // must be slower than the empty body per run.
    let empty = reports
        .iter()
        .find(|r| r.label == "setup/empty")
        .expect("the setup floor is measured");
    for report in &reports {
        if report.label == "setup/empty" {
            continue;
        }
        assert!(
            report.median_ns_per_run() >= empty.median_ns_per_run() * 0.5,
            "{} ({:.1} ns/run) must pay at least the empty-body setup ({:.1} ns/run)",
            report.label,
            report.median_ns_per_run(),
            empty.median_ns_per_run(),
        );
    }
}
