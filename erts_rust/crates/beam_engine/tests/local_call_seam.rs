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

//! The **local-call seam** (`ExecutionContext::resolve_local_call`): a host that
//! serves a same-module `(Module, Function, Arity)` as a BIF makes the engine
//! dispatch it through [`ExecutionContext::call_bif`] instead of jumping to the
//! callee's local label — and a host that answers `false` (or installs no resolver
//! at all) keeps the unchanged in-machine local call.
//!
//! # Why the seam exists
//!
//! ERTS's real module shape splits an Erlang wrapper from a non-exported NIF stub:
//! `lib/kernel/src/prim_tty.erl`'s `init/1`/`reader_read/2`/`writer_loop/2` run
//! in-module and chain **locally** into `tty_create/1`/`read_nif/3`/`write_nif/2`/…,
//! whose bodies are `erlang:nif_error(undef)`. The OTP compiler emits a **local**
//! `call` for a non-exported same-module function (only a self-qualified `call_ext`
//! for an *exported* one), and a local call reaches no host seam at all — so without
//! this seam a host could never serve such a stub. `resolve_local_call` is that
//! seam: `Ok(true)` means "I serve this callee as a BIF", `Ok(false)` means "enter
//! it in this machine", byte-identical to the tier before the seam existed.
//!
//! # What these tests pin
//!
//! 1. **Every local call form is served and observed.** The three forms
//!    (`Instr::Call`, `Instr::CallLast`, `Instr::CallOnly`) each reach a
//!    **non-exported** same-module callee, and a served callee is dispatched through
//!    the host's BIF hook exactly once, with the callee's *own* declared identity —
//!    never the call site's operands.
//! 2. **The arm's frame discipline is preserved.** A served `call` continues at the
//!    next instruction, a served `call_only` follows the frame's existing
//!    continuation, and a served `call_last` pops its frame **after** the BIF
//!    succeeded. The resting `frame_depth`/`stack_depth` of a served tail call is
//!    asserted against the interpreter's own declined run — the frame pop is the
//!    discriminant, since a skipped pop would leave the caller's Y slots on the
//!    stack.
//! 3. **Tier equivalence — the deliverable.** The same module is run with the
//!    *serving* context under [`Tier::Interpreter`] **and** [`Tier::BaselineJit`]
//!    via [`Machine::run_with_tier`], and both tiers must agree on the result, on
//!    every observed BIF dispatch, on the resolver queries, and on the resting
//!    frame/stack depths.
//! 4. **A decline is byte-identical to the pre-seam local call.** A resolver that
//!    answers `false`, and an absent resolver (the trait default), both leave the
//!    callee to run in this machine, on **both** tiers, and the callee is never
//!    dispatched as a BIF.
//! 5. **The native probe.** The baseline tier probes the seam on its
//!    `call_only`/`call_last` arms (`src/jit/baseline/emit/mod.rs` →
//!    `jit_local_call`). [`Machine::run_with_tier`] stages the *lazy* artifact
//!    (one function lowered per entry), where a `call_last` whose target is another
//!    function is not statically linkable; the *eager* artifact
//!    ([`JitBackend::compile`] + [`JitBackend::run_compiled`]) compiles every
//!    function up front, so its `call_last` **is** linkable and the emitted probe
//!    runs. Both layouts honour the seam — the eager one is pinned separately.
//!
//! # Hand-built modules
//!
//! There is no oracle for a synthetic module, so every fixture is hand-authored and
//! every expected value follows from the fixture's own code: the served callee's
//! body is `N + 100` (the host hook's `m:helper/1` answer) and the never-served
//! callee's body is the constant `999`.

use std::cell::RefCell;
use std::rc::Rc;

use beam_common::Result;
use beam_engine::jit::baseline::BaselineJit;
use beam_engine::jit::JitBackend;
use beam_engine::{Machine, MinimalContext, Tier};
use beam_loader::{
    Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand,
};
use beam_term::{is_small, make_small, signed_value, Atom, Eterm};

/// The argument every run is seeded with: the served callee answers `ARG + 100`,
/// the never-served one the constant `999`. A non-zero, small value keeps a
/// mis-ordered argument (`NIL` in a stale slot) visible.
const ARG: i64 = 41;

/// The served callee's entry label — the local `call`/`call_last`/`call_only`
/// target.
const HELPER: Label = Label(200);

/// The never-served callee's entry label (`other/1`).
const OTHER: Label = Label(300);

/// The `(module, function, arity)` of the served callee.
fn helper_identity() -> (String, String, u32) {
    ("m".to_string(), "helper".to_string(), 1)
}

/// The `(module, function, arity)` of the never-served callee.
fn other_identity() -> (String, String, u32) {
    ("m".to_string(), "other".to_string(), 1)
}

/// The `erlang:'+'/2` import the in-machine callee body dispatches.
fn int_add_identity() -> (String, String, u32) {
    ("erlang".to_string(), "+".to_string(), 2)
}

// ---------------------------------------------------------------------------
// What the host's resolver answers
// ---------------------------------------------------------------------------

/// The three resolver configurations a run can be built with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Install a resolver answering `true` for the served callee (and `false` for
    /// every other same-module callee).
    Serve,
    /// Install a resolver that answers `false` for every callee — the in-machine
    /// jump.
    Decline,
    /// Install **no** resolver: the trait's `Ok(false)` default.
    Absent,
}

// ---------------------------------------------------------------------------
// The shared observation log
// ---------------------------------------------------------------------------

/// Every `(module, function, arity)` a host seam was asked about, in order.
type Log = Rc<RefCell<Vec<(String, String, u32)>>>;

fn new_log() -> Log {
    Rc::new(RefCell::new(Vec::new()))
}

fn snapshot(log: &Log) -> Vec<(String, String, u32)> {
    log.borrow().clone()
}

/// Build the context a run uses: a BIF hook that models `m:helper/1` (`N + 100`)
/// and `erlang:'+'/2` (the in-machine callee's own dispatch), plus the resolver the
/// [`Answer`] selects. Both record into their own log, so the *observation* is the
/// assertion — not the value alone.
fn context_for(answer: Answer, resolver_log: Log, bif_log: Log) -> MinimalContext {
    let mut ctx = MinimalContext::with_bif_hook(Box::new(
        move |module: &str, function: &str, arity: u32, args: &[Eterm]| -> Result<Eterm> {
            bif_log
                .borrow_mut()
                .push((module.to_string(), function.to_string(), arity));
            match (module, function, arity) {
                ("m", "helper", 1) if args.len() == 1 && is_small(args[0]) => {
                    Ok(make_small(signed_value(args[0]) + 100))
                }
                ("erlang", "+", 2) if args.len() == 2 && is_small(args[0]) && is_small(args[1]) => {
                    Ok(make_small(signed_value(args[0]) + signed_value(args[1])))
                }
                _ => Err(beam_engine::unsupported(&format!(
                    "bif {module}:{function}/{arity}"
                ))),
            }
        },
    ));
    match answer {
        Answer::Serve => ctx.set_local_call_resolver(Box::new(
            move |module: &str, function: &str, arity: u32| -> Result<bool> {
                resolver_log
                    .borrow_mut()
                    .push((module.to_string(), function.to_string(), arity));
                // The seam is answered per identity: only the *served* callee is a
                // BIF, every other same-module callee runs in this machine.
                Ok(module == "m" && function == "helper" && arity == 1)
            },
        )),
        Answer::Decline => ctx.set_local_call_resolver(Box::new(
            move |module: &str, function: &str, arity: u32| -> Result<bool> {
                resolver_log
                    .borrow_mut()
                    .push((module.to_string(), function.to_string(), arity));
                Ok(false)
            },
        )),
        Answer::Absent => {}
    }
    ctx
}

// ---------------------------------------------------------------------------
// One run's observable outcome
// ---------------------------------------------------------------------------

/// Everything a run exposes: the result, every resolver query, every BIF dispatch,
/// and the machine's resting frame/stack depths.
struct Report {
    result: Result<Eterm>,
    resolver_queries: Vec<(String, String, u32)>,
    bif_calls: Vec<(String, String, u32)>,
    frame_depth: usize,
    stack_depth: usize,
}

/// Run `export/1` of `module` on `tier` over a fresh machine and a fresh context
/// built from `answer`.
fn run_arm(module: &Module, export: &str, arg: i64, answer: Answer, tier: Tier) -> Report {
    let resolver_log = new_log();
    let bif_log = new_log();
    let mut ctx = context_for(answer, Rc::clone(&resolver_log), Rc::clone(&bif_log));
    let mut machine = Machine::new();
    let result = machine.run_with_tier(
        module,
        atom(module, export),
        1,
        &[make_small(arg)],
        &mut ctx,
        tier,
    );
    Report {
        result,
        resolver_queries: snapshot(&resolver_log),
        bif_calls: snapshot(&bif_log),
        frame_depth: machine.frame_depth(),
        stack_depth: machine.stack_depth(),
    }
}

/// Run `export/1` of `module` on the **eager** baseline artifact — every function
/// compiled up front, so a `call_last` to another function *is* statically linkable
/// and the emitted `jit_local_call` probe runs.
fn run_arm_eager(module: &Module, export: &str, arg: i64, answer: Answer) -> Report {
    let resolver_log = new_log();
    let bif_log = new_log();
    let mut ctx = context_for(answer, Rc::clone(&resolver_log), Rc::clone(&bif_log));
    let mut machine = Machine::new();
    let mut backend = BaselineJit::new();
    let compiled = backend
        .compile(module)
        .expect("the synthetic fixture compiles on the baseline tier");
    let result = backend.run_compiled(
        &compiled,
        &mut machine,
        module,
        atom(module, export),
        1,
        &[make_small(arg)],
        &mut ctx,
    );
    Report {
        result,
        resolver_queries: snapshot(&resolver_log),
        bif_calls: snapshot(&bif_log),
        frame_depth: machine.frame_depth(),
        stack_depth: machine.stack_depth(),
    }
}

/// The same run on both tiers.
fn run_both(module: &Module, export: &str, arg: i64, answer: Answer) -> (Report, Report) {
    (
        run_arm(module, export, arg, answer, Tier::Interpreter),
        run_arm(module, export, arg, answer, Tier::BaselineJit),
    )
}

/// The run's small-integer result, or a panic naming the typed error.
fn value_of(report: &Report) -> i64 {
    match &report.result {
        Ok(term) => {
            assert!(
                is_small(*term),
                "the fixture's result is a small integer, got {term:?}"
            );
            signed_value(*term)
        }
        Err(err) => panic!("the run failed: {err:?}"),
    }
}

/// The same value, or the same typed error.
fn assert_same_result(interpreted: &Result<Eterm>, jitted: &Result<Eterm>, what: &str) {
    match (interpreted, jitted) {
        (Ok(interpreted), Ok(jitted)) => {
            assert_eq!(jitted, interpreted, "{what}: the tiers disagree on the value")
        }
        (Err(interpreted), Err(jitted)) => assert_eq!(
            format!("{jitted:?}"),
            format!("{interpreted:?}"),
            "{what}: the tiers disagree on the typed error"
        ),
        _ => panic!("{what}: one tier returned and the other raised: {interpreted:?} vs {jitted:?}"),
    }
}

/// **THE tier-equivalence assertion**: the interpreter and the native baseline tier
/// agree on the result, on every observed BIF dispatch, on every resolver query, and
/// on the machine's resting frame/stack depths.
fn assert_tiers_agree(interpreted: &Report, jitted: &Report, what: &str) {
    assert_same_result(&interpreted.result, &jitted.result, what);
    assert_eq!(
        jitted.resolver_queries, interpreted.resolver_queries,
        "{what}: the resolver was asked different questions on the two tiers"
    );
    assert_eq!(
        jitted.bif_calls, interpreted.bif_calls,
        "{what}: the observed BIF dispatches differ between the tiers"
    );
    assert_eq!(
        jitted.frame_depth, interpreted.frame_depth,
        "{what}: the resting frame depth differs between the tiers"
    );
    assert_eq!(
        jitted.stack_depth, interpreted.stack_depth,
        "{what}: the resting stack depth differs between the tiers"
    );
}

// ---------------------------------------------------------------------------
// Minimal synthetic-module scaffolding
// ---------------------------------------------------------------------------

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// A builder for synthetic modules whose functions the baseline tier can compile.
///
/// Running the loader's own [`beam_loader::lower::build_blocks`] over each body is
/// what makes the emitter (which walks a function's **blocks**) emit anything at
/// all — an empty `blocks` vector compiles to nothing.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    /// Register `module:function/arity` as import `ImportRef(n)` (`n` = its index).
    fn import(&mut self, module_name: &str, function_name: &str, arity: u32) -> ImportRef {
        let module = self.module.atom_table.intern(module_name);
        let function = self.module.atom_table.intern(function_name);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef((self.module.imports.len() - 1) as u32)
    }

    /// Append a function; returns its entry label (its first label).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.module.atom_table.intern(name);
        let module = self.module.name;
        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks,
            labels,
        });
        entry
    }

    /// Export `name/arity` at `label`.
    fn export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.module.atom_table.intern(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// The one module every test runs: three callers of the same **non-exported**
/// `helper/1` (one per local call form), one caller of the never-served `other/1`,
/// and the two callees.
///
/// ```text
/// run_direct/1:      allocate 0,1; call helper/1; deallocate 0; return
/// run_tail_last/1:   allocate 2,1; <use both Y slots>; call_last helper/1 2
/// run_tail_only/1:   call_only helper/1
/// run_other/1:       allocate 0,1; call other/1; deallocate 0; return
///
/// helper/1  (NOT exported): allocate 0,2; move 100 -> x1;
///                           call_ext erlang:'+'/2; deallocate 0; return
/// other/1   (NOT exported): move 999 -> x0; return
/// ```
///
/// `helper/1` is the callee the seam resolves; it is never exported, so a local
/// `call`/`call_last`/`call_only` is the **only** way to reach it — which is exactly
/// the shape the seam exists for. Its in-machine body computes `N + 100`, so the
/// declined route is byte-identical to the served one (whose BIF hook answers the
/// same value).
fn fixture_module() -> Module {
    let mut fixture = Fixture::new("m");
    let add = fixture.import("erlang", "+", 2);

    // `run_direct/1` — a non-tail local `call`. It allocates a (0-Y) frame so the
    // continuation the call stores has a frame to live in and the returned value
    // flows out through it.
    let direct = fixture.add_function(
        "run_direct",
        1,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 1,
            },
            Instr::Call {
                target: HELPER,
                arity: 1,
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );

    // `run_tail_last/1` — a local tail call that pops a **two-slot** frame. Both Y
    // slots are written, so a skipped (or mis-sized) pop is observable in
    // `stack_depth`.
    let tail_last = fixture.add_function(
        "run_tail_last",
        1,
        vec![
            Instr::Label(Label(110)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(2),
                live: 1,
            },
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::Y(1),
            },
            Instr::Move {
                src: Operand::Y(0),
                dst: Operand::X(0),
            },
            Instr::CallLast {
                arity: 1,
                target: HELPER,
                deallocate: 2,
            },
            Instr::Return,
        ],
    );

    // `run_tail_only/1` — a frame-less local tail call (`call_only`): nothing is
    // stored, so the callee's completion follows the frame's existing continuation
    // (the run entry's `Halt`).
    let tail_only = fixture.add_function(
        "run_tail_only",
        1,
        vec![
            Instr::Label(Label(120)),
            Instr::CallOnly {
                arity: 1,
                target: HELPER,
            },
        ],
    );

    // `run_other/1` — the same non-tail shape as `run_direct/1`, but its callee is
    // one the serving resolver answers `false` for.
    let other = fixture.add_function(
        "run_other",
        1,
        vec![
            Instr::Label(Label(130)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 1,
            },
            Instr::Call {
                target: OTHER,
                arity: 1,
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );

    // The served callee: `helper(N) -> N + 100`, exactly what the host's BIF hook
    // answers for `m:helper/1`.
    fixture.add_function(
        "helper",
        1,
        vec![
            Instr::Label(HELPER),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 2,
            },
            Instr::Move {
                src: Operand::Integer(100),
                dst: Operand::X(1),
            },
            Instr::CallExt { target: add, arity: 2 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );

    // The never-served callee: a leaf returning the constant `999`, so the
    // in-machine route is observable (never the hook's `N + 100`).
    fixture.add_function(
        "other",
        1,
        vec![
            Instr::Label(OTHER),
            Instr::Move {
                src: Operand::Integer(999),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );

    fixture.export("run_direct", 1, direct);
    fixture.export("run_tail_last", 1, tail_last);
    fixture.export("run_tail_only", 1, tail_only);
    fixture.export("run_other", 1, other);
    fixture.finish()
}

// ---------------------------------------------------------------------------
// (1) the non-tail local `call`
// ---------------------------------------------------------------------------

#[test]
fn a_served_local_call_dispatches_the_callee_through_the_bif_hook_on_both_tiers() {
    let module = fixture_module();
    let (interpreted, jitted) = run_both(&module, "run_direct", ARG, Answer::Serve);

    for (tier, report) in [("interpreter", &interpreted), ("baseline", &jitted)] {
        assert_eq!(
            value_of(report),
            ARG + 100,
            "{tier}: the hook's value must land in x(0) and flow out of the run"
        );
        assert_eq!(
            report.resolver_queries,
            vec![helper_identity()],
            "{tier}: the seam is asked once, with the callee's own declared identity"
        );
        assert_eq!(
            report.bif_calls,
            vec![helper_identity()],
            "{tier}: a served callee is dispatched through `call_bif` exactly once \
             (and the callee's own body never runs)"
        );
        // A non-tail `call` stores its continuation on the enter path and continues
        // at the next instruction on the synchronous value, so the caller's frame is
        // popped by its own `deallocate` and only the run entry's bottom frame stays.
        assert_eq!(report.frame_depth, 1, "{tier}: the caller's own frame is popped");
        assert_eq!(report.stack_depth, 0, "{tier}");
    }

    assert_tiers_agree(&interpreted, &jitted, "run_direct");
}

// ---------------------------------------------------------------------------
// (2) the local tail call `call_last`
// ---------------------------------------------------------------------------

#[test]
fn a_served_local_tail_call_last_pops_the_callers_frame_on_both_tiers() {
    let module = fixture_module();

    // The reference: the interpreter's own **declined** (in-machine) run. Its
    // resting state is what the served route must reproduce exactly.
    let declined = run_arm(&module, "run_tail_last", ARG, Answer::Decline, Tier::Interpreter);
    assert_eq!(value_of(&declined), ARG + 100, "the in-machine callee");
    assert_eq!(
        declined.frame_depth, 1,
        "only the run entry's bottom frame remains after the tail call"
    );
    assert_eq!(
        declined.stack_depth, 0,
        "`call_last`'s two Y slots are popped before the callee runs"
    );

    let (interpreted, jitted) = run_both(&module, "run_tail_last", ARG, Answer::Serve);

    for (tier, report) in [("interpreter", &interpreted), ("baseline", &jitted)] {
        assert_eq!(value_of(report), ARG + 100, "{tier}: the hook's value");
        assert_eq!(
            report.bif_calls,
            vec![helper_identity()],
            "{tier}: the served `call_last` target is a BIF dispatch"
        );
        assert_eq!(report.resolver_queries, vec![helper_identity()], "{tier}");
        // The frame discipline — the discriminant. A skipped or mis-sized pop would
        // leave `run_tail_last/1`'s two Y slots on the stack (`stack_depth == 2`) and
        // its frame live (`frame_depth == 2`).
        assert_eq!(
            report.frame_depth, declined.frame_depth,
            "{tier}: a served `call_last` must leave the interpreter's frame stack"
        );
        assert_eq!(
            report.stack_depth, declined.stack_depth,
            "{tier}: a served `call_last` must leave the interpreter's Erlang stack"
        );
        assert_eq!(report.frame_depth, 1, "{tier}: only the bottom frame remains");
        assert_eq!(report.stack_depth, 0, "{tier}: the two Y slots are gone");
    }

    assert_tiers_agree(&interpreted, &jitted, "run_tail_last");
}

// ---------------------------------------------------------------------------
// (3) the frame-less local tail call `call_only`
// ---------------------------------------------------------------------------

#[test]
fn a_served_local_tail_call_only_follows_the_frames_existing_continuation_on_both_tiers() {
    let module = fixture_module();
    let declined = run_arm(
        &module,
        "run_tail_only",
        ARG,
        Answer::Decline,
        Tier::Interpreter,
    );
    assert_eq!(value_of(&declined), ARG + 100, "the in-machine callee");
    assert_eq!(
        declined.frame_depth, 1,
        "`call_only` stores no frame, so the run entry's bottom frame is what the \
         callee's `return` follows"
    );
    assert_eq!(declined.stack_depth, 0);

    let (interpreted, jitted) = run_both(&module, "run_tail_only", ARG, Answer::Serve);

    for (tier, report) in [("interpreter", &interpreted), ("baseline", &jitted)] {
        assert_eq!(value_of(report), ARG + 100, "{tier}: the hook's value");
        assert_eq!(
            report.bif_calls,
            vec![helper_identity()],
            "{tier}: the served `call_only` target is a BIF dispatch"
        );
        assert_eq!(report.resolver_queries, vec![helper_identity()], "{tier}");
        assert_eq!(
            report.frame_depth, declined.frame_depth,
            "{tier}: a served `call_only` stores nothing, like the interpreter's"
        );
        assert_eq!(report.stack_depth, declined.stack_depth, "{tier}");
        assert_eq!(report.frame_depth, 1, "{tier}");
        assert_eq!(report.stack_depth, 0, "{tier}");
    }

    assert_tiers_agree(&interpreted, &jitted, "run_tail_only");
}

// ---------------------------------------------------------------------------
// (4) a resolver that declines — the unchanged in-machine local call
// ---------------------------------------------------------------------------

#[test]
fn a_declining_resolver_keeps_the_in_machine_local_call_on_both_tiers() {
    let module = fixture_module();

    // The served reference for `run_direct/1`: the hook's `N + 100`.
    let served = run_arm(&module, "run_direct", ARG, Answer::Serve, Tier::Interpreter);
    assert_eq!(value_of(&served), ARG + 100);

    // Every local call form, on both tiers, with a resolver that answers `false`.
    for export in ["run_direct", "run_tail_last", "run_tail_only"] {
        let (interpreted, jitted) = run_both(&module, export, ARG, Answer::Decline);

        // The value is the in-machine callee's own `N + 100` — byte-identical to the
        // served route, which is the whole point of a decline.
        assert_eq!(value_of(&interpreted), ARG + 100, "{export}: the in-machine callee");
        assert_same_result(&interpreted.result, &jitted.result, export);

        // The seam *was* asked (a declining host is still consulted) ...
        assert_eq!(interpreted.resolver_queries, vec![helper_identity()], "{export}");
        assert_eq!(jitted.resolver_queries, vec![helper_identity()], "{export}");

        // ... but the callee was NEVER dispatched as a BIF: the only dispatch either
        // tier sees is the in-machine body's own `erlang:'+'/2`.
        assert!(
            !interpreted.bif_calls.contains(&helper_identity()),
            "{export}: a declined callee must never reach `call_bif`"
        );
        assert!(
            !jitted.bif_calls.contains(&helper_identity()),
            "{export}: a declined callee must never reach `call_bif`"
        );
        assert_eq!(
            interpreted.bif_calls,
            vec![int_add_identity()],
            "{export}: the in-machine body's own BIF is the only dispatch"
        );
        assert_eq!(jitted.bif_calls, interpreted.bif_calls, "{export}");

        // The frame discipline of a decline matches the served route's.
        assert_eq!(interpreted.frame_depth, served.frame_depth, "{export}");
        assert_eq!(interpreted.stack_depth, served.stack_depth, "{export}");

        assert_tiers_agree(&interpreted, &jitted, export);
    }
}

// ---------------------------------------------------------------------------
// (5) no resolver at all — the trait's `Ok(false)` default
// ---------------------------------------------------------------------------

#[test]
fn an_absent_resolver_is_byte_identical_to_a_declining_resolver_on_both_tiers() {
    let module = fixture_module();

    for export in ["run_direct", "run_tail_last", "run_tail_only"] {
        let absent = run_arm(&module, export, ARG, Answer::Absent, Tier::Interpreter);
        let absent_jit = run_arm(&module, export, ARG, Answer::Absent, Tier::BaselineJit);
        let declined = run_arm(&module, export, ARG, Answer::Decline, Tier::Interpreter);

        assert!(
            absent.resolver_queries.is_empty(),
            "{export}: with no resolver installed the trait default answers without consulting anyone"
        );
        assert_eq!(value_of(&absent), ARG + 100, "{export}: the in-machine callee");
        assert_same_result(&absent.result, &declined.result, export);
        assert!(
            !absent.bif_calls.contains(&helper_identity()),
            "{export}: no resolver means no BIF dispatch of the callee"
        );
        assert_eq!(absent.frame_depth, declined.frame_depth, "{export}");
        assert_eq!(absent.stack_depth, declined.stack_depth, "{export}");

        assert_tiers_agree(&absent, &absent_jit, export);
    }
}

// ---------------------------------------------------------------------------
// (6) the seam is answered per identity, not "serve everything"
// ---------------------------------------------------------------------------

#[test]
fn the_seam_serves_only_the_answered_callee_identity() {
    let module = fixture_module();

    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        // `run_other/1` reaches `other/1`, which the serving resolver answers `false`
        // for, so it runs in this machine and returns its own constant.
        let report = run_arm(&module, "run_other", ARG, Answer::Serve, tier);
        assert_eq!(
            value_of(&report),
            999,
            "{tier:?}: the never-served callee's own constant, not the hook's N + 100"
        );
        assert_eq!(
            report.resolver_queries,
            vec![other_identity()],
            "{tier:?}: the seam is asked about the callee's own identity"
        );
        assert!(
            report.bif_calls.is_empty(),
            "{tier:?}: a declined callee dispatches no BIF (its body has none)"
        );
    }
}

// ---------------------------------------------------------------------------
// (7) the native probe on the eager baseline artifact
// ---------------------------------------------------------------------------

#[test]
fn the_eager_baseline_artifact_probes_the_seam_for_local_tail_calls() {
    // The eager artifact lowers **every** function up front, so a `call_last` whose
    // target is another function is statically linkable and the emitter's
    // `jit_local_call` probe runs (`src/jit/baseline/emit/mod.rs`); a `call_only` is
    // probed unconditionally. (The lazy artifact behind
    // `Machine::run_with_tier`/`Tier::BaselineJit` lowers one function per entry, so
    // the `call_last` target is not yet compiled when the caller is lowered: there the
    // seam is honoured through the per-instruction interpreter step instead of the
    // probe. Both routes are interpreter-identical; this test pins the probe.)
    let module = fixture_module();

    for export in ["run_tail_last", "run_tail_only"] {
        let served = run_arm_eager(&module, export, ARG, Answer::Serve);
        assert_eq!(
            value_of(&served),
            ARG + 100,
            "{export}: the probe must serve the callee on the eager artifact"
        );
        assert_eq!(
            served.bif_calls,
            vec![helper_identity()],
            "{export}: a served tail call is a BIF dispatch on the eager artifact"
        );
        assert_eq!(
            served.resolver_queries,
            vec![helper_identity()],
            "{export}: the probe asks the seam for the callee's own identity"
        );

        // The eager artifact's run must match the interpreter's for the same export.
        let interpreted = run_arm(&module, export, ARG, Answer::Serve, Tier::Interpreter);
        assert_same_result(&served.result, &interpreted.result, export);
        assert_eq!(served.bif_calls, interpreted.bif_calls, "{export}");
        assert_eq!(served.resolver_queries, interpreted.resolver_queries, "{export}");
        assert_eq!(served.frame_depth, interpreted.frame_depth, "{export}");
        assert_eq!(served.stack_depth, interpreted.stack_depth, "{export}");

        // And a decline on the eager artifact is the in-machine call again.
        let declined = run_arm_eager(&module, export, ARG, Answer::Decline);
        assert_eq!(value_of(&declined), ARG + 100, "{export}: the in-machine callee");
        assert!(
            !declined.bif_calls.contains(&helper_identity()),
            "{export}: a declined tail call never probes into a BIF dispatch"
        );
        assert_eq!(declined.bif_calls, vec![int_add_identity()], "{export}");
        assert_eq!(declined.frame_depth, served.frame_depth, "{export}");
        assert_eq!(declined.stack_depth, served.stack_depth, "{export}");
    }
}
