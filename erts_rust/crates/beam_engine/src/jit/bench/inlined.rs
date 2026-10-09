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

//! The workloads whose bodies the baseline tier **lowers natively** (the *opposite*
//! lowering story to the family workloads in [`super::workloads`], every one of which
//! is a `jit_step_one` helper round-trip): `slice:fib/1`, `kernel/2`, `wide/2`,
//! `aggr/2` and `cmp/2`.
//!
//! * **`fib/1`** — the real `.beam` fixture's `fib/1`
//!   (`beam_loader/tests/fixtures/slice.beam`, reused by relative path so there is one
//!   source of truth for the image): a *short* tail-recursive `call`/`gc_bif`/`return`
//!   chain, so the driver loop's per-run setup counts as much as the body.
//! * **`kernel/2`** — a hand-built straight-line chain of [`KERNEL_OPS`] chained
//!   `erlang:'+'/2` `gc_bif`s: exactly the shape the baseline tier's inlined guarded
//!   small-int fast path lowers, sized so execution dominates the per-run setup.
//! * **`wide/2`** — the same style over [`WIDE_REGISTERS`] x-registers, i.e. **more
//!   than the four the emitter caches**, so the direct `machine.x` addressing path is
//!   exercised (a body that used to deopt at the first instruction touching `x(4)`).
//! * **`aggr/2`** — a body of the guarded aggregate reads (`get_list`/`get_hd`/`get_tl`,
//!   `get_tuple_element`, `select_tuple_arity`) feeding a small-int `erlang:'*'/2` chain,
//!   i.e. the opcodes inlined in the same round as the direct-addressing work.
//! * **`cmp/2`** — a body of the eight `erlang` comparison BIFs, the shape both native
//!   tiers' comparison fast paths claim; every operand stays a small integer and every
//!   boolean result goes to a register that is never read back as an operand.
//!
//! Correctness is the deliverable, never "the JIT is faster": see the parent module for
//! the method and the caveats. The result checks live in [`super::checks`].

use beam_loader::{load_file, Instr, Label, Operand, NO_FAIL};
use beam_term::make_small;

use super::checks::{verify_aggr, verify_cmp, verify_fib, verify_kernel, verify_wide};
use super::support::Fixture;
use super::workloads::{Args, Workload};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// The 20th Fibonacci number `slice:fib(20)` computes — pinned so a tier that *agrees*
/// on a wrong answer still fails. Verified against the oracle
/// (`erl -noshell -eval 'io:format("~p~n", [slice:fib(20)]).'`).
pub(super) const FIB_RESULT: i64 = 6765;

/// The number of chained `erlang:'+'/2` `gc_bif`s in the synthetic kernel.
///
/// Large enough that the *execution* of the block (the inlined native arithmetic)
/// dominates the per-run setup, which is the point of the kernel workload.
const KERNEL_OPS: usize = 256;

/// The synthetic kernel's result: both accumulators start at `0`, so every chained
/// `'+'` stays `0` (a non-zero here would mean a wrong operand or destination register).
pub(super) const KERNEL_RESULT: i64 = 0;

/// The number of x-registers the wide kernel walks — twice the number of registers a
/// block caches in machine registers, so half of them are addressed directly in
/// `machine.x`.
const WIDE_REGISTERS: u32 = 8;

/// The number of straight-line steps (one `move` plus one `gc_bif '+'/2`) in the wide
/// kernel. Every value stays a small integer, so the inlined fast path lowers every
/// step.
const WIDE_OPS: usize = 256;

/// The wide kernel's result: every step accumulates into the *next* register, so the
/// epilogue's `x0 = x(WIDE_REGISTERS - 1)` reads the last step's `1`.
pub(super) const WIDE_RESULT: i64 = 1;

/// The number of straight-line steps in the aggregate kernel — one
/// `get_list`/`get_hd`/`get_tl` trio, one `get_tuple_element` and two `gc_bif '*'/2`
/// fast-path multiplies each.
const AGGR_STEPS: usize = 128;

/// The aggregate kernel's result: `x(0)` arrives as `1` and every multiplier the reads
/// produce is `1`, so the accumulator stays `1` (a taken fail arm would make it `-1`).
pub(super) const AGGR_RESULT: i64 = 1;

/// The number of rounds the comparison kernel walks its eight operators.
const CMP_OPS: usize = 64;

/// The eight `erlang` comparison operators the comparison kernel chains, in the order it
/// walks them — one per operator per round (the operator is looked up in each module's
/// import table by name, so the order here only decides the round's shape).
const CMP_OPERATORS: [&str; 8] = ["=:=", "=/=", "==", "/=", "<", "=<", ">", ">="];

/// The index of `"<"` in [`CMP_OPERATORS`] — the operator of the trailing comparison
/// that fixes the comparison kernel's exported result (see [`compare_workload`]).
const CMP_LT: usize = 4;

/// The x-registers the comparison kernel compares: `x(0)`, `x(1)` (the arguments) and
/// `x(2)` (seeded with [`CMP_SEED`]). All three stay small integers for the whole run.
const CMP_REGISTERS: [u32; 3] = [0, 1, 2];

/// The two small-int call arguments of the comparison kernel: `x(0) = 10`, `x(1) = 20`.
const CMP_ARGS: [i64; 2] = [10, 20];

/// The small integer `x(2)` is seeded with (`3`), the last inlined `select_val`
/// alternative and the left operand of the trailing `<`.
const CMP_SEED: i64 = 3;

/// The registers the comparison results are written to (`x(3)`..`x(5)`), which are never
/// read back as operands — a boolean fed forward would leave the small-int shape the
/// fast path requires.
const CMP_DESTINATIONS: [u32; 3] = [3, 4, 5];

/// The five inlined workloads, freshly built.
pub(super) fn inlined_workloads() -> Vec<Workload> {
    vec![
        fib_workload(),
        kernel_workload(),
        wide_workload(),
        aggregate_workload(),
        compare_workload(),
    ]
}

/// `slice:fib/1` over the committed oracle fixture.
fn fib_workload() -> Workload {
    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    let function = module
        .atom_table
        .lookup("fib")
        .unwrap_or_else(|| panic!("atom \"fib\" is not interned in the fixture"));
    Workload::new(
        "slice:fib/1 (20)".to_string(),
        module,
        function,
        1,
        Args::Fixed(vec![make_small(20)]),
        verify_fib,
    )
}

/// The synthetic straight-line kernel: `kernel/2` is [`KERNEL_OPS`] chained
/// `erlang:'+'/2` `gc_bif`s accumulating on `x(0)`/`x(1)`, then `return`.
///
/// The chain accumulates from the `(0, 0)` fixed point. That is forced by the shape: an
/// `'+'/2`-only dependency chain is monotone, so the Fibonacci-style growth a `(1, 1)`
/// start produces would leave the 59-bit small-integer range after ~90 ops and the
/// inlined fast path would deopt (measuring the fallback, not the inline). Starting at
/// zero keeps every intermediate a small integer while executing the same chained BIFs
/// over the same registers, each reading the previous one's result.
fn kernel_workload() -> Workload {
    let fixture = Fixture::with_plus("jit_bench_kernel");
    let mut code = vec![Instr::Label(Label(1))];
    for step in 0..KERNEL_OPS {
        // `x0 = x0 + x1` then `x1 = x1 + x0`, alternating: a serial dependency chain
        // that keeps both registers live across every operation.
        let (a, b, dst) = if step % 2 == 0 {
            (Operand::X(0), Operand::X(1), Operand::X(0))
        } else {
            (Operand::X(1), Operand::X(0), Operand::X(1))
        };
        code.push(fixture.add(a, b, dst, 2));
    }
    code.push(Instr::Return);

    let (module, function) = fixture.finish("kernel", 2, code);
    Workload::new(
        format!("kernel/2 ({KERNEL_OPS} chained gc_bif '+'/2)"),
        module,
        function,
        2,
        Args::Fixed(vec![make_small(0), make_small(0)]),
        verify_kernel,
    )
}

/// The synthetic **wide** kernel: `wide/2` is a straight-line body of [`WIDE_OPS`]
/// `move`+`gc_bif '+'/2` steps over [`WIDE_REGISTERS`] x-registers.
///
/// Each step writes `1` into `x(r)` and accumulates it into `x(r + 1)`, walking all
/// eight registers. The block caches only four of them, so `x(4)`..`x(7)` — reached
/// from the fourth step on — are addressed **directly in `machine.x`** by the inlined
/// `move` and by the fast path's operand loads / result store. The leading block of
/// `move`s seeds all eight registers with `0` so not even the first guard sees a
/// non-small value (a fresh `Machine` starts every register at `NIL`).
fn wide_workload() -> Workload {
    let fixture = Fixture::with_plus("jit_bench_wide");
    let mut code = vec![Instr::Label(Label(1))];
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
        code.push(fixture.add(
            Operand::X(register),
            Operand::X(next),
            Operand::X(next),
            WIDE_REGISTERS,
        ));
    }
    code.push(Instr::Move {
        src: Operand::X(WIDE_REGISTERS - 1),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let (module, function) = fixture.finish("wide", 2, code);
    Workload::new(
        format!("wide/2 ({WIDE_OPS} steps over {WIDE_REGISTERS} x-registers)"),
        module,
        function,
        2,
        Args::Fixed(vec![make_small(0), make_small(0)]),
        verify_wide,
    )
}

/// The synthetic **aggregate** kernel: `aggr/2` is a straight-line body of
/// [`AGGR_STEPS`] steps, each reading the same cons cell and tuple through every
/// aggregate read the baseline tier inlines and feeding each result into a small-int
/// `erlang:'*'/2` `gc_bif` chain:
///
/// ```text
///   x2 = [1 | 2]                             ; put_list        (interpreter step)
///   x3 = {1, 1, 1}                           ; put_tuple2      (interpreter step)
///   select_tuple_arity x3 Fail {3 -> body}   ; inlined switch
/// body:
///   get_list x2 x4 x5                        ; x4 = CAR (1), x5 = CDR (2)
///   get_hd   x2 x4 ; get_tl x2 x5            ; the same reads again
///   get_tuple_element x3 1 x6                ; x6 = element 1 (1)
///   x0 = x0 * x4 ; x0 = x0 * x6              ; '*'/2 fast path
///   ... repeated [`AGGR_STEPS`] times, then `return`
/// ```
///
/// The aggregates are built **once per run** at the top of the body and the two
/// multipliers are deliberately `1`: `x(0)` arrives as `1` (the call argument), so the
/// product stays `1` and can never leave the small-integer range — which is what keeps
/// the measurement on the **inlined** fast path.
fn aggregate_workload() -> Workload {
    let mut fixture = Fixture::new("jit_bench_aggregate");
    // The reads' deopt path builds `{badmatch, Source}`; the workload never takes it,
    // but the atom is interned so the typed reason cannot differ from the oracle's.
    fixture.intern("badmatch");
    let star = fixture.import("erlang", "*", 2);

    let entry = Label(1);
    let body = Label(2);
    let fail = Label(9);

    // `x0 = x0 * x(arg)` — the small-int `'*'/2` fast path the native tiers inline.
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
        // The cons cell `[1 | 2]` and the 3-tuple `{1, 1, 1}` — built once per run.
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Integer(2),
            dst: Operand::X(2),
        },
        Instr::PutTuple2 {
            dst: Operand::X(3),
            elements: vec![Operand::Integer(1), Operand::Integer(1), Operand::Integer(1)],
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
    // The `select_tuple_arity` fail arm: reachable only if the tuple were not arity 3,
    // i.e. never — it exists so the switch is inlined (a `NO_FAIL` switch deopts).
    code.push(Instr::Label(fail));
    code.push(Instr::Move {
        src: Operand::Integer(-1),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let (module, function) = fixture.finish("aggr", 2, code);
    Workload::new(
        format!(
            "aggr/2 ({AGGR_STEPS} steps of get_list/get_hd/get_tl + get_tuple_element + \
             gc_bif '*'/2)"
        ),
        module,
        function,
        2,
        Args::Fixed(vec![make_small(1), make_small(1)]),
        verify_aggr,
    )
}

/// The synthetic **comparison** kernel: `cmp/2` is a straight-line body of [`CMP_OPS`]
/// rounds of the eight `erlang` comparison operators over the three small-int
/// x-registers seeded with [`CMP_ARGS`] / [`CMP_SEED`]:
///
/// ```text
///   x2 = 3                     ; move   (x0 = 10, x1 = 20 arrive as args)
///   x3 = x0 =:= x1             ; bif    (inlined comparison fast path)
///   x4 = x1 =/= x2             ; bif
///   x5 = x2 ==  x0             ; bif
///   x3 = x0 /=  x1 ; x4 = x1 < x2 ; x5 = x2 =< x0 ; x3 = x0 > x1 ; x4 = x1 >= x2
///   ... repeated [`CMP_OPS`] times,
///   x0 = x2 < x1               ; the export's result — 3 < 20, i.e. `true`
///   return
/// ```
///
/// Two invariants keep the measurement on the inlined path, both by construction: every
/// **operand** register (`x(0)`..`x(2)`) holds a small integer for the whole run, and
/// every **result** goes to one of [`CMP_DESTINATIONS`], never read back as an operand.
/// The operator/operand pairing (`x(k)` against `x(k+1)`, cyclically) makes the rounds
/// produce a mix of `true`/`false`, while the trailing comparison's `true` is the
/// export's result — so the check sees a real comparison result (the module's own
/// `true` atom), not a trivially-agreeing accumulator.
fn compare_workload() -> Workload {
    assert_eq!(
        CMP_OPERATORS[CMP_LT], "<",
        "CMP_LT must index the `<` operator the trailing comparison uses"
    );

    let mut fixture = Fixture::new("jit_bench_compare");
    // The comparison fast paths materialize the **module's own** `true`/`false` atom
    // words and refuse to inline a comparison when either is absent (they never
    // fabricate an atom), so both must be interned — and the interpreter's hook needs
    // them for the same reason (see `Workload::bif_hook`).
    fixture.intern("true");
    fixture.intern("false");

    let mut operators = Vec::with_capacity(CMP_OPERATORS.len());
    for name in CMP_OPERATORS {
        operators.push(fixture.import("erlang", name, 2));
    }

    let mut code = vec![Instr::Label(Label(1))];
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
    // The export's result: `x(2)` (3) `<` `x(1)` (20) is `true`. Writing `x(0)` is safe
    // here and only here — it is an operand register, but this is the last instruction
    // before the `return`.
    code.push(Instr::Bif {
        arity: 2,
        bif: operators[CMP_LT],
        fail: NO_FAIL,
        args: vec![Operand::X(CMP_REGISTERS[2]), Operand::X(CMP_REGISTERS[1])],
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let (module, function) = fixture.finish("cmp", 2, code);
    Workload::new(
        format!(
            "cmp/2 ({CMP_OPS} rounds of the eight comparison BIFs over \
             {} small-int x-registers)",
            CMP_REGISTERS.len()
        ),
        module,
        function,
        2,
        Args::Fixed(vec![make_small(CMP_ARGS[0]), make_small(CMP_ARGS[1])]),
        verify_cmp,
    )
}
