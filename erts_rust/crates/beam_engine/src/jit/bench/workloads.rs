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

//! The measured workloads: four hand-built synthetic modules over the opcode shapes the
//! baseline tier does **not** inline.
//!
//! Each workload is a [`Workload`] — a [`Module`] assembled with the loader's own block
//! decomposition ([`beam_loader::lower::build_blocks`]), an exported `function/arity`,
//! how to obtain the call arguments, and a value check that pins the result the body is
//! *supposed* to compute (so a tier that "agrees" on a wrong answer still fails).
//!
//! The harness half (the parent module) owns the tiers, the timing and the reporting;
//! this file owns only the programs. See the parent for the method and the caveats
//! (mean `ns/op`, shared/high-load sandbox, and the fact that correctness — never
//! "the JIT is faster" — is the only thing asserted).

use beam_common::Result;
use beam_loader::{Instr, Label, MapWriteArgs, Module, Operand, TestKind, NO_FAIL};
use beam_term::{make_small, Atom, Eterm, NIL};

use crate::context::MinimalContext;
use crate::terms;

use super::checks::{verify_build, verify_map, verify_select, verify_walk};
use super::support::Fixture;

// ---------------------------------------------------------------------------
// Workload parameters
// ---------------------------------------------------------------------------

/// The `put_tuple2`/`put_list` pairs the build body constructs per run.
pub(super) const BUILD_ROUNDS: usize = 64;

/// The map-op rounds per run (one `has_map_fields` + one three-pair
/// `get_map_elements` + one `put_map_assoc` + three small-int adds each).
pub(super) const MAP_ROUNDS: usize = 32;

/// The `select_val` rounds per run (one inlined switch + one deopted switch each).
pub(super) const SELECT_ROUNDS: usize = 32;

/// The number of cons cells in the list the walk workload traverses.
pub(super) const WALK_LENGTH: usize = 64;

/// The number of timed runs per `(workload, tier)`, and the warm-up count.
///
/// One warm-up run precedes the timed runs for every tier; the count below is the
/// **timed** count, and is the same for every tier (no tier pays a per-call compile
/// inside the loop — compilation happens once, before the warm-up).
pub(super) const TIMED_RUNS: u32 = 3_000;

/// The run count the (fast, non-ignored) agreement test uses.
pub(super) const AGREEMENT_RUNS: u32 = 4;

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------
/// How a workload's call arguments are produced for a run.
enum Args {
    /// Immediate-only arguments: identical in every context, so they can be shared.
    Fixed(Vec<Eterm>),
    /// Arguments that are **heap terms** and therefore must be built in the run's own
    /// context (a `MinimalContext` owns the arena its terms live in, so a term built
    /// in one context must never be handed to another).
    InContext(fn(&mut MinimalContext) -> Result<Vec<Eterm>>),
}

/// A benchmark workload: a module, an exported `function/arity`, its arguments and a
/// shape check for the result.
pub(super) struct Workload {
    /// The human-readable label printed with the numbers.
    pub(super) label: String,
    /// The module holding the export.
    pub(super) module: Module,
    /// The exported function's name atom (interned in `module`).
    pub(super) function: Atom,
    /// The exported arity.
    pub(super) arity: u32,
    /// How the call arguments are obtained.
    args: Args,
    /// A shape check pinned to the workload (never a tautology: it pins the value the
    /// body is *supposed* to compute, so a tier that "agrees" on a wrong answer still
    /// fails).
    verify: fn(Eterm) -> std::result::Result<(), String>,
}

impl Workload {
    /// The call arguments for a run, obtained **in the run's own context**.
    ///
    /// A heap term (the map, the list) must be built in the context that will own it:
    /// a [`MinimalContext`] frees its arena on drop and two contexts have separate
    /// heaps, so a term built elsewhere would dangle.
    pub(super) fn arguments(&self, ctx: &mut MinimalContext) -> Result<Vec<Eterm>> {
        match &self.args {
            Args::Fixed(values) => Ok(values.clone()),
            Args::InContext(build) => build(ctx),
        }
    }

    /// Pin `result` to this workload's intended value.
    pub(super) fn verify(&self, result: Eterm) -> std::result::Result<(), String> {
        (self.verify)(result)
    }
}

/// The four workloads, freshly built.
pub(super) fn workloads() -> Vec<Workload> {
    vec![
        build_workload(),
        map_workload(),
        select_workload(),
        walk_workload(),
    ]
}

/// `build/0` — [`BUILD_ROUNDS`] `put_tuple2` + `put_list` pairs, then `return`.
///
/// ```text
///   x1 = []                              ; move (the list accumulator)
///   x2 = {1, 2, 3, 4}                    ; put_tuple2   (one interpreter step)
///   x1 = [1 | x1]                        ; put_list     (one interpreter step)
///   ... repeated BUILD_ROUNDS times,
///   x0 = x1                              ; the list `[1, 1, …]`, BUILD_ROUNDS long
///   return
/// ```
///
/// The tuple is dead (nothing reads it back) and the list is only *built* — this body
/// measures the two **constructors**, which are exactly the instructions a native tier
/// hands to `jit_step_one` one at a time. The result is the built list, so the check
/// pins both the length and every element.
fn build_workload() -> Workload {
    let fixture = Fixture::with_plus("jit_bench_build");
    let mut code = vec![Instr::Label(Label(1))];
    code.push(Instr::Move {
        src: Operand::Nil,
        dst: Operand::X(1),
    });
    for round in 0..BUILD_ROUNDS {
        code.push(Instr::PutTuple2 {
            dst: Operand::X(2),
            elements: vec![
                Operand::Integer(round as i64 + 1),
                Operand::Integer(2),
                Operand::Integer(3),
                Operand::Integer(4),
            ],
        });
        code.push(Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::X(1),
            dst: Operand::X(1),
        });
    }
    code.push(Instr::Move {
        src: Operand::X(1),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let (module, function) = fixture.finish("build", 0, code);
    Workload {
        label: format!("build/0 ({BUILD_ROUNDS} put_tuple2 + {BUILD_ROUNDS} put_list)"),
        module,
        function,
        arity: 0,
        args: Args::Fixed(Vec::new()),
        verify: verify_build,
    }
}

/// `map/2` — [`MAP_ROUNDS`] rounds of the map instructions over a flat map.
///
/// ```text
///   has_map_fields  fail  x0 #{1, 2, 3}          ; deopt
///   get_map_elements fail x0 {1->x2, 2->x3, 3->x4} ; deopt (three pairs)
///   x5 = x0#{4 => 1}                             ; put_map_assoc — deopt + new map
///   x1 = x1 + x2 ; x1 = x1 + x3 ; x1 = x1 + x4    ; inlined small-int adds
///   ... repeated MAP_ROUNDS times,
///   x0 = x1
///   return
/// ```
///
/// `x(0)` is the source map (`#{1 => 1, 2 => 2, 3 => 3}`), built once per measurement
/// in the run's own context by [`map_args`]; `x(1)` is a small-int accumulator bounded
/// by `MAP_ROUNDS * 6`, so every `'+'` stays in the small-int fast path.
fn map_workload() -> Workload {
    let fixture = Fixture::with_plus("jit_bench_map");
    let fail = Label(9);

    let mut code = vec![Instr::Label(Label(1))];
    for _ in 0..MAP_ROUNDS {
        code.push(Instr::HasMapFields {
            fail,
            source: Operand::X(0),
            keys: vec![
                Operand::Integer(1),
                Operand::Integer(2),
                Operand::Integer(3),
            ],
        });
        code.push(Instr::GetMapElements {
            fail,
            source: Operand::X(0),
            pairs: vec![
                (Operand::Integer(1), Operand::X(2)),
                (Operand::Integer(2), Operand::X(3)),
                (Operand::Integer(3), Operand::X(4)),
            ],
        });
        code.push(Instr::PutMapAssoc(Box::new(MapWriteArgs {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(5),
            live: 6,
            pairs: vec![(Operand::Integer(4), Operand::X(2))],
        })));
        code.push(fixture.add(Operand::X(1), Operand::X(2), Operand::X(1), 5));
        code.push(fixture.add(Operand::X(1), Operand::X(3), Operand::X(1), 5));
        code.push(fixture.add(Operand::X(1), Operand::X(4), Operand::X(1), 5));
    }
    code.push(Instr::Move {
        src: Operand::X(1),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);
    // The (unreachable) fail arm: every key the body reads is present in the source
    // map, so a `-1` here would mean a family read went wrong.
    code.push(Instr::Label(fail));
    code.push(Instr::Move {
        src: Operand::Integer(-1),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let (module, function) = fixture.finish("map", 2, code);
    Workload {
        label: format!(
            "map/2 ({MAP_ROUNDS} rounds of has_map_fields + get_map_elements(3) + put_map_assoc)"
        ),
        module,
        function,
        arity: 2,
        args: Args::InContext(map_args),
        verify: verify_map,
    }
}

/// `select/1` — a body dominated by `select_val`, mixing the inlined and deopted
/// shapes.
///
/// ```text
///   x0 = 3 ; x2 = 0                        ; move
/// round:
///   select_val x0 Fail{0,1,2,3 -> arm}     ; inlined (immediates + resolved labels)
/// arm:
///   x2 = x2 + x0                           ; inlined small-int add
///   select_val x0 Fail{x1 -> cont}         ; DEOPTED (a register alternative)
/// cont:
///   ... repeated SELECT_ROUNDS times,
///   x0 = x2                                ; 3 * SELECT_ROUNDS
///   return
/// ```
///
/// The scrutinee is the constant `3`, i.e. the **last** inlined alternative, so the
/// native path's compare chain is actually walked before it matches. The deopted
/// switch's alternative is a register, which `constant_word` refuses, so the whole
/// instruction stays on the helper — the mix is by construction, not by chance. The
/// accumulator is what the result check pins.
fn select_workload() -> Workload {
    let fixture = Fixture::with_plus("jit_bench_select");
    let entry = Label(1);
    let mut code = vec![
        Instr::Label(entry),
        Instr::Move {
            src: Operand::Integer(3),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(2),
        },
    ];
    let mut next = 2u32;
    for _ in 0..SELECT_ROUNDS {
        let arm = Label(next);
        next += 1;
        let cont = Label(next);
        next += 1;
        // Inlined: four immediate alternatives and a resolved fail label.
        code.push(Instr::SelectVal {
            value: Operand::X(0),
            fail: arm,
            pairs: (0..4).map(|value| (Operand::Integer(value), arm)).collect(),
        });
        code.push(Instr::Label(arm));
        code.push(fixture.add(Operand::X(2), Operand::X(0), Operand::X(2), 3));
        // Deopted: a register alternative is not an immediate constant.
        code.push(Instr::SelectVal {
            value: Operand::X(0),
            fail: cont,
            pairs: vec![(Operand::X(1), cont)],
        });
        code.push(Instr::Label(cont));
    }
    code.push(Instr::Move {
        src: Operand::X(2),
        dst: Operand::X(0),
    });
    code.push(Instr::Return);

    let (module, function) = fixture.finish("select", 1, code);
    Workload {
        label: format!("select/1 ({SELECT_ROUNDS} rounds: one inlined + one deopted select_val)"),
        module,
        function,
        arity: 1,
        args: Args::Fixed(vec![make_small(0)]),
        verify: verify_select,
    }
}

/// `walk/2` — a hot tail-recursive loop over a cons list: an `is_nonempty_list` guard
/// + `get_list` + a small-int `'+'/2` + `call_only`.
///
/// ```text
///   x1 = 0
///   jump loop
/// loop:
///   test is_nonempty_list Fail base [x0]   ; a `test` jumps on FAILURE, so this
///                                          ; branches to `base` exactly when x0 is []
///   get_list x0 x2 x0                      ; x2 = head, x0 = tail
///   x1 = x1 + x2                           ; inlined small-int add
///   call_only 2 loop                       ; the tail call
/// base:
///   x0 = x1
///   return
/// ```
///
/// The guard is `is_nonempty_list` (not `is_nil`) because a BEAM `test` instruction
/// branches on the *negation* of the named predicate — `is_nil` would continue only on
/// `[]` — which is the same shape `erlc` emits for `sum([H|T], Acc)`.
///
/// The list `[1, 1, …]` ([`WALK_LENGTH`] long) is built once per measurement in the
/// run's own context by [`walk_args`], so the timed loop is the walk only — one
/// iteration per element, with a guard and a tail call, i.e. control flow the
/// straight-line `aggr/2` workload of `tests/jit_bench.rs` never exercises.
fn walk_workload() -> Workload {
    let fixture = Fixture::with_plus("jit_bench_walk");
    let loop_label = Label(4);
    let base = Label(5);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(1),
        },
        Instr::Jump { target: loop_label },
        Instr::Label(loop_label),
        Instr::Test {
            kind: TestKind::IsNonemptyList,
            fail: base,
            args: vec![Operand::X(0)],
        },
        Instr::GetList {
            source: Operand::X(0),
            head: Operand::X(2),
            tail: Operand::X(0),
        },
        fixture.add(Operand::X(1), Operand::X(2), Operand::X(1), 3),
        Instr::CallOnly {
            arity: 2,
            target: loop_label,
        },
        Instr::Label(base),
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];

    let (module, function) = fixture.finish("walk", 2, code);
    Workload {
        label: format!(
            "walk/2 (is_nonempty_list guard + get_list + '+'/2 + call_only over {WALK_LENGTH} cells)"
        ),
        module,
        function,
        arity: 2,
        args: Args::InContext(walk_args),
        verify: verify_walk,
    }
}

// ---------------------------------------------------------------------------
// Argument preparation (heap terms must live in the run's own context)
// ---------------------------------------------------------------------------

/// The `map/2` arguments: the flat map `#{1 => 1, 2 => 2, 3 => 3}`, built in `ctx`.
fn map_args(ctx: &mut MinimalContext) -> Result<Vec<Eterm>> {
    let map = terms::make_flatmap(
        ctx,
        &[
            (make_small(1), make_small(1)),
            (make_small(2), make_small(2)),
            (make_small(3), make_small(3)),
        ],
    )?;
    Ok(vec![map, make_small(0)])
}

/// The `walk/2` arguments: the `[1, 1, …]` list of [`WALK_LENGTH`] cells, built in
/// `ctx`, plus the `0` accumulator.
fn walk_args(ctx: &mut MinimalContext) -> Result<Vec<Eterm>> {
    let mut list = NIL;
    for _ in 0..WALK_LENGTH {
        list = terms::make_cons(ctx, make_small(1), list)?;
    }
    Ok(vec![list, make_small(0)])
}
