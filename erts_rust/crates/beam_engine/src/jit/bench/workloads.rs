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

//! The workload **type** and the four shapes the baseline tier does **not** inline.
//!
//! Each workload is a [`Workload`] — a [`Module`] assembled with the loader's own block
//! decomposition ([`beam_loader::lower::build_blocks`]), an exported `function/arity`,
//! how to obtain the call arguments, and a value check that pins the result the body is
//! *supposed* to compute (so a tier that "agrees" on a wrong answer still fails).
//!
//! This file owns the four shapes whose lowering story is "every instruction is a
//! `jit_step_one` helper round-trip on a native tier": `build/0`
//! (`put_tuple2`/`put_list`), `map/2` (the map family), `select/1` (`select_val`) and
//! `walk/2` (a tail-recursive cons-list walk), plus the two-module `cross/2` shape,
//! whose transfer is neither: its `call_ext_last` deopts, but the *driver* then lands
//! the call cross-module. The shapes the tier *inlines* are in [`super::inlined`],
//! and [`workloads`] is the concatenation of all three.
//!
//! The harness half (the parent module) owns the tiers, the timing and the reporting;
//! this file owns the workload type and the deopted programs. See the parent for the
//! method and the caveats (mean `ns/op`, shared/high-load sandbox, and the fact that
//! correctness — never "the JIT is faster" — is the only thing asserted).

use std::sync::Arc;

use beam_common::Result;
use beam_loader::{
    HeapNeed, ImportRef, Instr, Label, MapWriteArgs, Module, Operand, TestKind, NO_FAIL,
};
use beam_term::{make_atom, make_small, Atom, Eterm, NIL};

use crate::context::{BifHook, MinimalContext};
use crate::terms;

use super::checks::{verify_build, verify_cross, verify_map, verify_select, verify_walk};
use super::support::{arith_hook, Fixture};

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

/// The number of cross-module `call_ext_last` transfers one `cross/2` run performs.
///
/// Every transfer increments the accumulator exactly once, so the exported result is
/// this count — a loop that ran a different number of transfers cannot produce it.
pub(super) const CROSS_STEPS: usize = 64;

/// The module the cross-module workload is **entered** through (the caller side).
const PING_MODULE: &str = "jit_bench_cross_a";

/// Its exported tail-call loop.
const PING_FUNCTION: &str = "ping";

/// The module the cross-module workload tail-calls (the callee side).
const PONG_MODULE: &str = "jit_bench_cross_b";

/// The callee's exported tail-call loop — the one that bounces back into [`PING_MODULE`].
const PONG_FUNCTION: &str = "pong";
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
pub(super) enum Args {
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
    verify: fn(&Workload, Eterm) -> std::result::Result<(), String>,
    /// The cross-module call pair this workload drives, if any (see [`CrossCall`]).
    cross: Option<CrossCall>,
}

/// The two-module cross-module tail-call pair a [`Workload`] may carry.
///
/// A `call_ext_last` is not in a native tier's inline set, so it always **deopts** to
/// one interpreter step — but that step resolves the import through the host's
/// cross-module seam ([`ExecutionContext::resolve_external_call`]) and hands the
/// driver a [`Step::Enter`](crate::Step), so the transfer itself is a driver-mediated
/// **module-context switch**. This type is what lets the harness answer that seam for
/// the pair of modules the `cross/2` workload ping-pongs between.
///
/// [`ExecutionContext::resolve_external_call`]: crate::ExecutionContext::resolve_external_call
pub(super) struct CrossCall {
    /// The callee module handle — the same `Arc<Module>` the driver must be able to
    /// `Arc::ptr_eq`-dedup as one module context, so it is kept alive here.
    pub(super) module: Arc<Module>,
    /// The `module` name the workload module's `call_ext_last` import names.
    pub(super) import_module: &'static str,
    /// The `function` name that import names.
    pub(super) import_function: &'static str,
    /// The arity of that import (both directions of the pair use the same arity).
    pub(super) import_arity: u32,
    /// The callee's export **entry** label (the label *after* `func_info`) — the exact
    /// label a `run_export` would enter.
    pub(super) entry: Label,
}

impl Workload {
    /// Assemble a workload. The constructor exists so a workload defined outside this
    /// file (see [`super::inlined`]) can build one without naming its private
    /// `args`/`verify` fields.
    pub(super) fn new(
        label: String,
        module: Module,
        function: Atom,
        arity: u32,
        args: Args,
        verify: fn(&Workload, Eterm) -> std::result::Result<(), String>,
    ) -> Workload {
        Workload {
            label,
            module,
            function,
            arity,
            args,
            verify,
            cross: None,
        }
    }

    /// Attach the cross-module call pair this workload drives.
    ///
    /// A separate builder so the five workloads that need no resolver keep building
    /// through [`Workload::new`] unchanged.
    pub(super) fn with_cross(mut self, cross: CrossCall) -> Workload {
        self.cross = Some(cross);
        self
    }
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
        (self.verify)(self, result)
    }

    /// The BIF hook this workload's runs need — the pluggable body of the
    /// interpreter's `ctx.call_bif`.
    ///
    /// Every arithmetic workload uses the small-int [`arith_hook`]. The comparison
    /// kernel needs one thing more: the result of a comparison is the module's **own**
    /// `true`/`false` atom word, and an atom index only means something inside the
    /// [`AtomTable`] it was interned in (each workload owns a private table), so the two
    /// words can only come from *this* module's table. They are handed to the hook only
    /// when the module interns **both** — the hook panics rather than fabricate a
    /// missing one, so a half-supplied pair must not reach it.
    ///
    /// [`AtomTable`]: beam_term::AtomTable
    pub(super) fn bif_hook(&self) -> BifHook {
        match (
            self.module.atom_table.lookup("true").map(make_atom),
            self.module.atom_table.lookup("false").map(make_atom),
        ) {
            (Some(truthy), Some(falsy)) => arith_hook(Some(truthy), Some(falsy)),
            _ => arith_hook(None, None),
        }
    }

    /// Install this workload's cross-module-call resolver, if it has one.
    ///
    /// The resolver is the host half of the `call_ext*` seam
    /// ([`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call)):
    /// it answers `(module, function, arity)` with the callee's shared `Arc<Module>`
    /// plus its export entry label, or `Ok(None)` to leave the call on the
    /// synchronous `call_bif` path (which is what every other import — `erlang`'s `-`
    /// — must keep getting, since the arithmetic hook is the reference for that).
    ///
    /// A ping-pong needs **both** directions answered, and the workload module itself
    /// is not an `Arc`, so the resolver also holds a handle to it (and to its own
    /// export entry), derived from `self`. A workload without a [`CrossCall`] installs
    /// nothing, so its context keeps the trait's `Ok(None)` default.
    ///
    /// [`ExecutionContext::resolve_external_call`]: crate::ExecutionContext::resolve_external_call
    pub(super) fn install_cross(&self, ctx: &mut MinimalContext) {
        let Some(cross) = &self.cross else {
            return;
        };
        let callee = Arc::clone(&cross.module);
        let entry = cross.entry;
        let import_module = cross.import_module;
        let import_function = cross.import_function;
        let import_arity = cross.import_arity;
        // The callee's own `call_ext_last` names the entry module, so the resolver
        // needs a handle to it too — a genuine A -> B -> A ping-pong, every hop a
        // cross-module `Step::Enter` transfer. The names are copied out because the
        // resolver must be `'static` (it outlives this borrow).
        let home = Arc::new(self.module.clone());
        let home_entry = self.module.export(self.function, self.arity);
        let home_module = self.module.atom_name(self.module.name).map(str::to_string);
        let home_function = self.module.atom_name(self.function).map(str::to_string);

        ctx.set_external_resolver(Box::new(move |module, function, arity| {
            if arity != import_arity {
                return Ok(None);
            }
            if module == import_module && function == import_function {
                return Ok(Some((Arc::clone(&callee), entry)));
            }
            if home_module.as_deref() == Some(module) && home_function.as_deref() == Some(function)
            {
                if let Some(label) = home_entry {
                    return Ok(Some((Arc::clone(&home), label)));
                }
            }
            Ok(None)
        }));
    }
}
/// Every workload the tier table measures: the shapes the baseline tier **deopts**
/// ([`build_workload`]/[`map_workload`]/[`select_workload`]/[`walk_workload`]) followed
/// by the shapes it **inlines** (`super::inlined`) and the cross-module transfer
/// ([`cross_workload`]).
pub(super) fn workloads() -> Vec<Workload> {
    let mut all = vec![
        build_workload(),
        map_workload(),
        select_workload(),
        walk_workload(),
    ];
    all.extend(super::inlined::inlined_workloads());
    all.push(cross_workload());
    all
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
        cross: None,
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
        cross: None,
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
        cross: None,
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
        cross: None,
    }
}

/// `cross/2` — [`CROSS_STEPS`] cross-module `call_ext_last` transfers between **two**
/// modules, a `ping/2` ⇄ `pong/2` ping-pong.
///
/// ```text
/// entry:
///   allocate 0
///   select_val x0 Fail=step {0 => base}    ; DEOPTED: a resolved-label switch
/// step:
///   x1 = x1 + 1                            ; inlined small-int '+'
///   x0 = x0 - 1                            ; DEOPTED small-int '-'/2
///   call_ext_last 2 <other>/2 0            ; DEOPTED: resolved through the host seam
/// base:
///   x0 = x1
///   return
/// ```
///
/// This is the one workload whose cost is not an instruction-lowering story: the
/// `call_ext_last` cannot be inlined (a compiled block's module, label map and
/// literal area are all bound for **one** module — see `super::super::CONTEXT.md`), so
/// it deopts to one interpreter step, which resolves the import through the host's
/// cross-module seam and hands the driver a `Step::Enter`. The driver then performs
/// the module-context switch. On the baseline tier the callee's blocks are resolved
/// through the driver's `NativeCode::module_blocks`
/// ([`crate::jit::cache::cached_module_blocks`]), which compiles the callee into the
/// per-thread [`CodeCache`](crate::jit::cache::CodeCache) on first sight — so the
/// callee runs **natively** too, and the workload isolates the *transfer* itself.
///
/// The pair is a genuine A → B → A ping-pong: `jit_bench_cross_a:ping/2` tail-calls
/// `jit_bench_cross_b:pong/2`, which tail-calls `jit_bench_cross_a:ping/2` again, each
/// hop crossing the module boundary. Every transfer counts itself in `x(1)` exactly
/// once, so the exported result is [`CROSS_STEPS`] — and the loop terminates because
/// `x(0)` (the remaining transfers) is decremented on every hop and the switch at the
/// top branches to `base` at zero.
///
/// [`Fixture::with_plus`] installs `erlang:'+'/2` (index 0, the inlined increment);
/// `erlang:'-'/2` is index 1 and the cross-module target of the module at hand is
/// index 2. Both modules share [`cross_body`], so the two bodies are the same shape
/// with their own imports.
fn cross_workload() -> Workload {
    // The callee side: `jit_bench_cross_b:pong/2` bounces back into `jit_bench_cross_a`.
    let mut pong = Fixture::with_plus(PONG_MODULE);
    pong.import("erlang", "-", 2);
    let back_to_ping = pong.import(PING_MODULE, PING_FUNCTION, 2);
    let code = cross_body(&pong, back_to_ping);
    let (pong_module, pong_function) = pong.finish(PONG_FUNCTION, 2, code);
    let entry = pong_module
        .export(pong_function, 2)
        .expect("the cross workload exports the callee's entry");
    let callee = Arc::new(pong_module);

    // The caller side: the module the run is entered through.
    let mut ping = Fixture::with_plus(PING_MODULE);
    ping.import("erlang", "-", 2);
    let to_pong = ping.import(PONG_MODULE, PONG_FUNCTION, 2);
    let code = cross_body(&ping, to_pong);
    let (module, function) = ping.finish(PING_FUNCTION, 2, code);

    Workload::new(
        format!(
            "cross/2 ({CROSS_STEPS} cross-module call_ext_last transfers between \
             {PING_MODULE} and {PONG_MODULE})"
        ),
        module,
        function,
        2,
        Args::Fixed(vec![make_small(CROSS_STEPS as i64), make_small(0)]),
        verify_cross,
    )
    .with_cross(CrossCall {
        module: callee,
        import_module: PONG_MODULE,
        import_function: PONG_FUNCTION,
        import_arity: 2,
        entry,
    })
}

/// The ping-pong body both modules of the `cross/2` pair share.
///
/// `cross` is the module's own cross-module import (index 2 of its import table: the
/// other module's exported function). `x(0)` counts the transfers still to make and
/// `x(1)` counts the ones already made, so the entry's switch sends an exhausted `x(0)`
/// to `base` and every other value into `step` — where the cross-module tail call is
/// taken with the decremented counters. The `allocate 0` gives every entered body a
/// frame for its `call_ext_last ... 0` to pop.
///
/// The `select_val` has a **resolved-label** `fail` (so it is the deopted shape, not
/// the inlined immediate one), and every `gc_bif` carries `NO_FAIL`, i.e. a BIF error
/// raises rather than branching — the arithmetic hook answers both `+` and `-`.
fn cross_body(fixture: &Fixture, cross: ImportRef) -> Vec<Instr> {
    let step = Label(2);
    let base = Label(3);
    vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(0),
            live: 2,
        },
        Instr::SelectVal {
            value: Operand::X(0),
            fail: step,
            pairs: vec![(Operand::Integer(0), base)],
        },
        Instr::Label(step),
        // `x1 = x1 + 1` — the `erlang:'+'/2` import `Fixture::with_plus` installs at 0.
        fixture.add(Operand::X(1), Operand::Integer(1), Operand::X(1), 2),
        // `x0 = x0 - 1` — the `erlang:'-'/2` import at index 1.
        Instr::GcBif {
            arity: 2,
            bif: ImportRef(1),
            fail: NO_FAIL,
            live: 2,
            args: vec![Operand::X(0), Operand::Integer(1)],
            dst: Operand::X(0),
        },
        Instr::CallExtLast {
            arity: 2,
            target: cross,
            deallocate: 0,
        },
        Instr::Label(base),
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
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
