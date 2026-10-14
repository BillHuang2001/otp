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

//! The workload **type** and the five shapes the baseline tier does **not** inline.
//!
//! Each workload is a [`Workload`] — a [`Module`] assembled with the loader's own block
//! decomposition ([`beam_loader::lower::build_blocks`]), an exported `function/arity`,
//! how to obtain the call arguments, and a value check that pins the result the body is
//! *supposed* to compute (so a tier that "agrees" on a wrong answer still fails).
//!
//! This file owns the five shapes whose lowering story is "every instruction is a
//! `jit_step_one` helper round-trip on a native tier": `build/0`
//! (`put_tuple2`/`put_list`), `map/2` (the map family), `select/1` (`select_val`),
//! `walk/2` (a tail-recursive cons-list walk) and `sum/1` (the **non**-tail-recursive
//! list sum, whose live Y slot forces its `gc_bif '+'/2` off the tier's inlined
//! small-int fast path), plus the two-module `cross/2` shape,
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

use crate::context::{BifHook, Mailbox, MinimalContext};
use crate::terms;

use super::checks::{
    verify_build, verify_call_ext, verify_cross, verify_map, verify_select, verify_sum, verify_walk,
};
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

/// The number of cons cells in the list a `sum/1` run consumes.
///
/// Cell `i` (1-based) holds the small integer `i`, so the sum is the triangular number
/// `SUM_LENGTH * (SUM_LENGTH + 1) / 2` — a figure a wrong length, a wrong cell value,
/// or a head the recursion failed to keep alive in its Y slot cannot produce. It is
/// deliberately the same magnitude as [`WALK_LENGTH`], so the two list walks are
/// comparable.
pub(super) const SUM_LENGTH: usize = 64;

/// The number of cross-module `call_ext_last` transfers one `cross/2` run performs.
///
/// Every transfer increments the accumulator exactly once, so the exported result is
/// this count — a loop that ran a different number of transfers cannot produce it.
pub(super) const CROSS_STEPS: usize = 64;

/// The number of **declined** `erlang:'+'/2` `call_ext` calls one `call_ext/0` run makes.
///
/// Every call adds [`CALL_EXT_INCREMENT`] to the accumulator, and the loop trips when
/// the accumulator reaches `CALL_EXT_ROUNDS * CALL_EXT_INCREMENT`, so the exported
/// result is that product — a run that skipped, repeated or reordered a call cannot
/// produce it.
pub(super) const CALL_EXT_ROUNDS: i64 = 256;

/// The small integer the declined `erlang:'+'/2` `call_ext` adds on every call.
///
/// A value `> 1` (rather than a unit increment) means the accumulator only reaches the
/// loop limit — itself `CALL_EXT_ROUNDS * CALL_EXT_INCREMENT` — after exactly
/// [`CALL_EXT_ROUNDS`] calls, so the pinned result is only reproduced if the loop trips
/// at the right iteration.
pub(super) const CALL_EXT_INCREMENT: i64 = 3;

/// The module the declined-`call_ext` workload is entered through.
const CALL_EXT_MODULE: &str = "jit_bench_call_ext";

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
    /// The mailbox factory a workload whose body `send`s and `receive`s needs, if any
    /// (see [`Mailbox`]). A factory rather than an instance because the harness calls it
    /// once per measurement, so every run starts from an empty mailbox.
    mailbox: Option<fn() -> Box<dyn Mailbox>>,
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
            mailbox: None,
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

    /// Attach the mailbox factory this workload's `send`/`receive` body needs.
    ///
    /// A separate builder, like [`Workload::with_cross`], so the workloads that never
    /// `send` or `receive` keep building through [`Workload::new`] unchanged and keep
    /// the receive seam's typed `unsupported` default.
    pub(super) fn with_mailbox(mut self, make: fn() -> Box<dyn Mailbox>) -> Workload {
        self.mailbox = Some(make);
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

    /// Install this workload's mailbox, if it has one.
    ///
    /// The mailbox is the host half of the receive seam ([`Mailbox`]): a workload whose
    /// body `send`s and `receive`s installs a factory here and the harness calls it once
    /// per measurement, so every run starts from an empty mailbox. A workload without one
    /// installs nothing, so its context keeps the seam's typed `unsupported` default.
    pub(super) fn install_mailbox(&self, ctx: &mut MinimalContext) {
        if let Some(make) = self.mailbox {
            ctx.set_mailbox(make());
        }
    }
}
/// Every workload the tier table measures: the shapes the baseline tier **deopts**
/// ([`build_workload`]/[`map_workload`]/[`select_workload`]/[`walk_workload`]/[`sum_workload`])
/// followed by the shapes it **inlines** (`super::inlined`), the cross-module transfer
/// ([`cross_workload`]), the declined `call_ext` seam ([`call_ext_workload`]) and the
/// `send` + `receive` message-passing loopback
/// ([`super::send_recv::send_recv_workload`]).
pub(super) fn workloads() -> Vec<Workload> {
    let mut all = vec![
        build_workload(),
        map_workload(),
        select_workload(),
        walk_workload(),
        sum_workload(),
    ];
    all.extend(super::inlined::inlined_workloads());
    all.push(cross_workload());
    all.push(call_ext_workload());
    all.push(super::send_recv::send_recv_workload());
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
        mailbox: None,
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
        mailbox: None,
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
        mailbox: None,
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
        mailbox: None,
    }
}

/// `sum/1` — the **non-tail-recursive** cons-list sum, i.e. the compiled lowering of
///
/// ```text
///   sum([])     -> 0;
///   sum([H|T])  -> H + sum(T).
/// ```
///
/// ```text
///   test is_nonempty_list Fail base [x0]   ; a `test` jumps on FAILURE, so this
///                                          ; branches to `base` exactly when x0 is []
///   get_list x0 x1 x0                      ; x1 = head, x0 = tail
///   allocate 2 2                           ; the frame this call needs (2 Y slots)
///   move x1 y0                             ; the head survives the recursion in Y(0)
///   call 1 entry                           ; NON-tail: the continuation is saved in the frame
///   y1 = y0 + x0                           ; gc_bif '+'/2 — DEOPTED (a Y operand)
///   move y1 x0
///   deallocate 2
///   return
/// base:
///   x0 = 0
///   return
/// ```
///
/// Where `walk/2` recurses **tail** (so its `'+'` is over two cached x-registers and the
/// tier inlines it natively), this one genuinely needs the caller's frame across the
/// recursive call — which is exactly the shape `src/jit/baseline/emit/arith.rs`'s
/// `loadable()` refuses: a `gc_bif '+'/2` whose operand is an `Operand::Y(k)` is not a
/// small-int fast-path candidate, so the **whole instruction** deopts to one
/// `jit_step_one` interpreter step. That is the gap this workload exists to measure (the
/// `sum/1` figure is the BEFORE baseline for closing it). The list
/// `[1, 2, …, SUM_LENGTH]` is built once per measurement in the run's own context by
/// [`sum_args`].
///
/// The `gc_bif` writes its result to a Y slot as well (`{y,1}`), a second reason the
/// fast path refuses the instruction; the interpreter accepts both the Y operand and the
/// Y destination, so the tier agreement check pins the exact triangular sum.
fn sum_workload() -> Workload {
    let fixture = Fixture::with_plus("jit_bench_sum");
    let entry = Label(1);
    let base = Label(3);
    let code = vec![
        Instr::Label(entry),
        Instr::Test {
            kind: TestKind::IsNonemptyList,
            fail: base,
            args: vec![Operand::X(0)],
        },
        Instr::GetList {
            source: Operand::X(0),
            head: Operand::X(1),
            tail: Operand::X(0),
        },
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 2,
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::Y(0),
        },
        Instr::Call {
            arity: 1,
            target: entry,
        },
        // `y1 = y0 + x0` — the Y operand (and the Y destination) put this `gc_bif`
        // outside the tier's inlined small-int fast path.
        Instr::GcBif {
            arity: 2,
            bif: ImportRef(0),
            fail: NO_FAIL,
            live: 1,
            args: vec![Operand::Y(0), Operand::X(0)],
            dst: Operand::Y(1),
        },
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
        Instr::Label(base),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];

    let (module, function) = fixture.finish("sum", 1, code);
    Workload {
        label: format!(
            "sum/1 (non-tail-recursive list sum + a Y-operand '+'/2 over {SUM_LENGTH} cells)"
        ),
        module,
        function,
        arity: 1,
        args: Args::InContext(sum_args),
        verify: verify_sum,
        cross: None,
        mailbox: None,
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

/// `call_ext/0` — [`CALL_EXT_ROUNDS`] **declined** `erlang:'+'/2` `call_ext` calls in a
/// counted loop.
///
/// ```text
/// entry:
///   x0 = 0                                 ; move (the accumulator)
///   x1 = CALL_EXT_INCREMENT                ; move (the addend)
///   allocate 0 2                           ; the frame the non-tail call_ext needs
/// loop:
///   call_ext 2 erlang:'+'/2                ; resolved through the host seam, DECLINED
///   test is_ge Fail=loop x0 limit          ; loop while x0 < limit
///   deallocate 0
///   return
/// ```
///
/// This is the workload for the **declined** `call_ext*` path — a `call_ext` whose host
/// answer is "run it synchronously in this process" (`Ok(None)`, the common BIF shape
/// and the `MinimalContext` default) rather than a module this machine can enter. On the
/// baseline tier the `call_ext` is not a plain deopt: it is lowered through the native
/// `jit_ext_call` helper, which consults the host's cross-module seam **once**; on the
/// decline the same instruction is then handed to `jit_step_ext` with that already-computed
/// answer, so `Machine::call_ext_dispatch` never re-asks the seam. Before that helper
/// existed the tier resolved the same import **twice** (once in `jit_ext_call`, once
/// inside the `jit_step_one` fallback), so measuring the decline from now on is exactly
/// what keeps the one-consultation property honest at the benchmark level. The
/// deterministic per-instruction pin lives in
/// `src/jit/baseline/tests.rs::the_baseline_tier_agrees_with_the_interpreter_on_the_declined_call_ext_seam_count`;
/// this workload is its end-to-end, hot-loop counterpart.
///
/// The import is `erlang:'+'/2` (`Fixture::with_plus`'s index-0 import), the same shape
/// the per-instruction pin uses: it is answered by the harness's [`arith_hook`] on the
/// synchronous `call_bif` route, so every call is a **declined** `call_ext` that lands on
/// the BIF. It is deliberately *synthetic* — `erlc` emits `+`/2 as a `gc_bif`, not a
/// `call_ext` — because the instruction under measurement is the `call_ext` **seam**, not
/// the arithmetic: the seam is identical for any declined import, and `'+'/2` is the one
/// the harness's `Fixture` interns and its hook answers.
///
/// The frame discipline is load-bearing. A `run_export` body pushes a bottom `Halt`
/// frame; a non-tail `call_ext` would overwrite *that* frame's continuation, so the loop
/// `allocate`s its own frame first and `deallocate`s it before its final `return`. Each
/// call adds [`CALL_EXT_INCREMENT`] to the accumulator, the `test is_ge` trips once it
/// reaches `CALL_EXT_ROUNDS * CALL_EXT_INCREMENT`, so the exported result is that product
/// and the loop made exactly [`CALL_EXT_ROUNDS`] calls (a `test` branches on the
/// *negation* of its predicate, so `is_ge` loops while `x0 <` the limit).
fn call_ext_workload() -> Workload {
    let fixture = Fixture::with_plus(CALL_EXT_MODULE);
    let entry = Label(1);
    let loop_label = Label(2);
    let limit = CALL_EXT_ROUNDS * CALL_EXT_INCREMENT;
    let code = vec![
        Instr::Label(entry),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(CALL_EXT_INCREMENT),
            dst: Operand::X(1),
        },
        // The frame the non-tail `call_ext` needs (see the doc above): allocated once,
        // outside the loop, and popped before the final `return`.
        Instr::Allocate {
            stack_need: HeapNeed::Words(0),
            live: 2,
        },
        Instr::Label(loop_label),
        // `x0 = x0 + x1` — a `call_ext` the host's seam declines (`Ok(None)`), i.e. the
        // duplicate-resolution shape `jit_step_ext` removes.
        Instr::CallExt {
            arity: 2,
            target: ImportRef(0),
        },
        // A `test` jumps on FAILURE, so `is_ge` loops while `x0 < limit` and falls
        // through once the accumulator reaches it.
        Instr::Test {
            kind: TestKind::IsGe,
            fail: loop_label,
            args: vec![Operand::X(0), Operand::Integer(limit)],
        },
        Instr::Deallocate(0),
        Instr::Return,
    ];

    let (module, function) = fixture.finish("call_ext", 0, code);
    Workload::new(
        format!(
            "call_ext/0 ({CALL_EXT_ROUNDS} declined erlang:'+'/2 call_ext calls in a loop)"
        ),
        module,
        function,
        0,
        Args::Fixed(Vec::new()),
        verify_call_ext,
    )
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

/// The `sum/1` argument: the `[1, 2, …, SUM_LENGTH]` list, built in `ctx`.
///
/// The cells are prepended from the tail, so cell `i` holds `i` (1-based); distinct cell
/// values mean the pinned triangular sum is only reproduced if every head is carried
/// across the recursive call correctly.
fn sum_args(ctx: &mut MinimalContext) -> Result<Vec<Eterm>> {
    let mut list = NIL;
    for value in (1..=SUM_LENGTH as i64).rev() {
        list = terms::make_cons(ctx, make_small(value), list)?;
    }
    Ok(vec![list])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `sum/1` workload exists to measure exactly one gap: a `gc_bif '+'/2` whose
    /// operand is an `Operand::Y(k)`, which the baseline tier's small-int fast path
    /// refuses (`src/jit/baseline/emit/arith.rs::loadable`), so the whole instruction
    /// deopts to one interpreter step. Drop that Y operand and the workload would keep
    /// passing the tier-agreement check while silently measuring nothing, so the shape is
    /// pinned here. The Y **destination** is the second half of the same refusal.
    #[test]
    fn the_sum_workload_keeps_its_y_slot_operand_and_destination() {
        let workload = sum_workload();
        let mut y_operands = 0;
        let mut y_destinations = 0;
        for function in &workload.module.functions {
            for instr in &function.code {
                let Instr::GcBif { args, dst, .. } = instr else {
                    continue;
                };
                if args.iter().any(|operand| matches!(operand, Operand::Y(_))) {
                    y_operands += 1;
                }
                if matches!(dst, Operand::Y(_)) {
                    y_destinations += 1;
                }
            }
        }
        assert_eq!(
            y_operands, 1,
            "sum/1 must keep exactly one `gc_bif` with a Y-slot operand"
        );
        assert_eq!(
            y_destinations, 1,
            "sum/1 must keep exactly one `gc_bif` writing to a Y slot"
        );
    }

    /// The `call_ext/0` workload exists to measure exactly one path: a `call_ext` the
    /// host **declines** (the synchronous BIF route through `jit_step_ext`). Swap it for
    /// a `gc_bif` — or drop the import — and the workload would keep passing the
    /// tier-agreement check while silently measuring nothing, so the shape is pinned
    /// here: exactly one `call_ext 2 erlang:'+'/2`, no `gc_bif` anywhere, and the one
    /// import is the `erlang:'+'/2` [`Fixture::with_plus`] installs.
    #[test]
    fn the_call_ext_workload_keeps_a_declined_call_ext_in_its_loop() {
        let workload = call_ext_workload();
        let mut call_exts = 0;
        let mut gc_bifs = 0;
        for function in &workload.module.functions {
            for instr in &function.code {
                match instr {
                    Instr::CallExt { arity, target }
                        if *arity == 2 && matches!(target, ImportRef(0)) =>
                    {
                        call_exts += 1;
                    }
                    Instr::GcBif { .. } => gc_bifs += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(
            call_exts, 1,
            "call_ext/0 must keep exactly one `call_ext 2` in its loop"
        );
        assert_eq!(
            gc_bifs, 0,
            "the declined path must stay a `call_ext`, never a `gc_bif`"
        );

        let import = workload
            .module
            .imports
            .first()
            .expect("the module imports the one BIF its `call_ext` names");
        assert_eq!(workload.module.atom_name(import.module), Some("erlang"));
        assert_eq!(workload.module.atom_name(import.function), Some("+"));
        assert_eq!(import.arity, 2);
    }
}
