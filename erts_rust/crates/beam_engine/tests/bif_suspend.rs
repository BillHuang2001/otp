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
//! **The engine-level BIF suspend channel**: park a process on a BIF that cannot
//! complete without waiting, and resume it by **re-executing** the instruction.
//!
//! [`ExecutionContext::call_bif_outcome`] may answer [`BifOutcome::Suspend`] instead of
//! a value. The engine then applies **nothing**: it reports
//! `RunOutcome::Suspend { func, pc }` at the *BIF instruction itself*, and
//! [`Machine::resume_outcome`] re-enters there, so the BIF is re-issued from its own
//! operands. That is the whole contract this suite pins, one property per test:
//!
//! | test | what it proves |
//! |---|---|
//! | [`a_suspending_bif_parks_on_its_own_instruction`] | the reported address is the BIF instruction (`Machine::current_code` too) and the destination register is untouched |
//! | [`a_resumed_bif_is_reissued_with_the_same_arguments`] | resumption re-issues the BIF with **identical** operands (two dispatches) |
//! | [`the_suspended_operands_stay_in_the_machines_root_set`] | the operands are the machine's own state, and the `RootSet` the host is handed *after* the resume contains them |
//! | [`a_suspended_then_resumed_run_equals_a_run_that_never_suspends`] | suspending changes no observable result |
//! | [`both_tiers_report_and_resume_the_same_suspend`] | interpreter and baseline JIT report and resume the same park |
//! | [`a_suspend_through_apply_is_a_typed_gap`] | the non-idempotent `apply/3` site reports a typed error, never a park or a value |
//! | [`the_in_machine_outcome_door_carries_the_suspend`] | a host that opted into the in-machine door is reached through its *outcome* twin |
//! | [`the_non_blocking_entries_report_a_suspend_as_a_typed_error`] | `run_export`/`run_label` fold a park into `error::suspended()` (not `blocked()`) |
//! | [`the_default_doors_keep_a_plain_host_byte_identical`] | a host with only `call_bif` behaves exactly as before |
//! | [`many_suspends_then_a_value`] | three parks then a value: four dispatches, the same operands each time |
//!
//! # The rooting argument, and what a *collecting* leg would add
//!
//! The engine keeps **no** suspended-BIF state: neither the arguments nor the BIF's
//! identity are copied anywhere. The parked state is `(func, pc)` plus the machine's
//! registers, Erlang stack and FR file — which is exactly the frozen
//! [`RootSet`](beam_engine::context::RootSet) (`x[..live]`, the whole stack, the whole
//! FR file) a collecting host is handed at every admission point. Resumption
//! **re-executes** the instruction, which re-reads its operands from that same state.
//! So the operands of a suspended BIF are roots by construction.
//!
//! Both boxes have a **boxed** operand here (a 2-tuple in `x(1)`, a 1-tuple in the Y
//! slot `y(0)`), because an immediate proves nothing about relocation. The published
//! root set is asserted to contain both words.
//!
//! The strongest possible leg — a host that actually **relocates** the heap between
//! the park and the resume and then observes the moved operands — is **not** in this
//! file: relocating a term means writing raw heap words, which needs `unsafe`, and
//! this suite is `unsafe`-free by construction (the equivalent fixture lives in
//! `tests/rooted_alloc.rs`'s `MovingContext`, a different test binary whose subject is
//! the rooted *construction* seam). What is proven here is the precondition that leg
//! would rely on: after the resume the operands are published in `roots.x` /
//! `roots.stack`, so a moving collector may rewrite those slices and the re-executed
//! instruction reads the rewritten words.
use beam_common::{BeamError, Result};
use beam_engine::context::RootSet;
use beam_engine::{
    error, terms, BifOutcome, ExecutionContext, Machine, MinimalContext, RunOutcome, Tier,
};
use beam_loader::{
    Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{make_atom, make_small, Atom as TermAtom, Eterm};
// ---------------------------------------------------------------------------
// The hand-built fixture
// ---------------------------------------------------------------------------
/// The fixture module's name.
const MODULE_NAME: &str = "parkmod";
/// The fixture's only export (`run/0`).
const ENTRY: &str = "run";
/// The imported BIF the fixture's entry dispatches — the host owns its behaviour.
const BIF_MODULE: &str = "host";
const BIF_FUNCTION: &str = "park";
/// The value the hook answers once it stops suspending.
const BIF_RESULT: i64 = 42;
/// The atom left in the BIF's destination register before the call, so "the
/// instruction was not applied" is observable as "the register still holds it".
const SENTINEL: &str = "sentinel";
/// The fixture's imports: index `0` is the parking BIF.
const PARK_IMPORT: ImportRef = ImportRef(0);
/// Index of the `gc_bif` in [`suspending_module`]'s code — the address a park on it
/// must report.
const PARK_PC: u32 = 7;
/// The fixture has one function, so every `func` is `0`.
const PARK_FUNC: u32 = 0;

/// A one-function module whose entry builds two **boxed** operands, parks on a BIF
/// that takes them, allocates once after the resume (a rooted admission point), and
/// returns the operand the collection may have moved.
///
/// ```text
///  0: label 1                        ; func_label
///  1: label 2                        ; entry_label
///  2: allocate 1
///  3: put_tuple2 x(1) {7,8}          ; boxed operand 1 (in an X register)
///  4: put_tuple2 x(3) {9}            ; boxed operand 2 ...
///  5: move x(3) y(0)                 ; ... moved to a Y slot (only `roots.stack` carries it)
///  6: move sentinel x(0)             ; the destination's pre-call value
///  7: gc_bif2 host:park/2 x(1) y(0) -> x(0)      ; <- the parking instruction
///  8: put_tuple2 x(4) {x(0)}         ; a rooted allocation AFTER the resume
///  9: move x(1) x(0)
/// 10: deallocate 1
/// 11: return
/// ```
///
/// Every basic block is built through the loader's own lowering, so the native tier
/// has real blocks to enter.
fn suspending_module() -> (Module, TermAtom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern(MODULE_NAME);
    let sentinel = module.atom_table.intern(SENTINEL);
    module.imports.push(Import {
        module: module.atom_table.intern(BIF_MODULE),
        function: module.atom_table.intern(BIF_FUNCTION),
        arity: 2,
    });
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 3,
        },
        Instr::PutTuple2 {
            dst: Operand::X(1),
            elements: vec![Operand::Integer(7), Operand::Integer(8)],
        },
        Instr::PutTuple2 {
            dst: Operand::X(3),
            elements: vec![Operand::Integer(9)],
        },
        Instr::Move {
            src: Operand::X(3),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Atom(sentinel),
            dst: Operand::X(0),
        },
        Instr::GcBif {
            arity: 2,
            bif: PARK_IMPORT,
            fail: NO_FAIL,
            live: 4,
            args: vec![Operand::X(1), Operand::Y(0)],
            dst: Operand::X(0),
        },
        Instr::PutTuple2 {
            dst: Operand::X(4),
            elements: vec![Operand::X(0)],
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let (blocks, labels) = beam_loader::lower::build_blocks(&code);
    let name = module.atom_table.intern(ENTRY);
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(2),
        module: module.name,
        name,
        arity: 0,
        code,
        blocks,
        labels,
    });
    module.exports.push(Export {
        function: name,
        arity: 0,
        label: Label(2),
    });
    (module, name)
}

/// A one-function module whose body is `erlang:apply(host, park, [2, 1])` — the
/// **non-idempotent** dispatch site: `resolve_call_ext_apply` has already unpacked the
/// argument list into `x(0..arity)` by the time the callee is dispatched, so a
/// suspend there cannot be honoured by re-execution.
///
/// ```text
///  0: label 1
///  1: label 2
///  2: allocate 0
///  3: move host     x(0)
///  4: move park     x(1)
///  5: put_list 1 [] x(3)
///  6: put_list 2 x(3) x(2)          ; x(2) = [2,1]
///  7: call_ext 3 erlang:apply/3
///  8: deallocate 0
///  9: return
/// ```
fn apply_module() -> (Module, TermAtom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern("applymod");
    let host = module.atom_table.intern(BIF_MODULE);
    let park = module.atom_table.intern(BIF_FUNCTION);
    module.imports.push(Import {
        module: module.atom_table.intern("erlang"),
        function: module.atom_table.intern("apply"),
        arity: 3,
    });
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(0),
            live: 3,
        },
        Instr::Move {
            src: Operand::Atom(host),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Atom(park),
            dst: Operand::X(1),
        },
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Nil,
            dst: Operand::X(3),
        },
        Instr::PutList {
            head: Operand::Integer(2),
            tail: Operand::X(3),
            dst: Operand::X(2),
        },
        Instr::CallExt {
            target: ImportRef(0),
            arity: 3,
        },
        Instr::Deallocate(0),
        Instr::Return,
    ];
    let (blocks, labels) = beam_loader::lower::build_blocks(&code);
    let name = module.atom_table.intern(ENTRY);
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(2),
        module: module.name,
        name,
        arity: 0,
        code,
        blocks,
        labels,
    });
    module.exports.push(Export {
        function: name,
        arity: 0,
        label: Label(2),
    });
    (module, name)
}

/// A one-function module whose body is a single zero-arity BIF and a `return`:
/// `run/0 -> host:plain()`.
fn plain_module() -> (Module, TermAtom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern("plainmod");
    module.imports.push(Import {
        module: module.atom_table.intern(BIF_MODULE),
        function: module.atom_table.intern("plain"),
        arity: 0,
    });
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::GcBif {
            arity: 0,
            bif: ImportRef(0),
            fail: NO_FAIL,
            live: 1,
            args: Vec::new(),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let (blocks, labels) = beam_loader::lower::build_blocks(&code);
    let name = module.atom_table.intern(ENTRY);
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(2),
        module: module.name,
        name,
        arity: 0,
        code,
        blocks,
        labels,
    });
    module.exports.push(Export {
        function: name,
        arity: 0,
        label: Label(2),
    });
    (module, name)
}

// ---------------------------------------------------------------------------
// The parking host
// ---------------------------------------------------------------------------
/// One `(module, function, arity, args)` BIF dispatch, as the host saw it.
type Call = (String, String, u32, Vec<Eterm>);

/// A [`RootSet`] as it was published to the host at one allocation, copied out so the
/// assertions can run after the host has moved on.
struct PublishedRoots {
    /// `roots.live_len()` — the declared live X-register count.
    live: usize,
    /// `roots.x[..live_len()]`, the live X registers.
    x: Vec<Eterm>,
    /// The whole Erlang stack (`roots.stack`).
    stack: Vec<Eterm>,
    /// The whole FR file (`roots.fr`).
    fr: Vec<Eterm>,
}

/// A test host whose BIF can **wait**: it answers [`BifOutcome::Suspend`] for its
/// first `suspends_left` dispatches and the value [`BIF_RESULT`] afterwards.
///
/// It records every dispatch (so "the same operands" is checkable), every published
/// [`RootSet`] (so "the operands are roots" is checkable), and which of the two
/// outcome doors each dispatch took.
struct ParkHost {
    /// The pointer-stable test heap the engine's term builders allocate in.
    arena: MinimalContext,
    /// How many more dispatches answer [`BifOutcome::Suspend`].
    suspends_left: usize,
    /// Every dispatch, in order, from whichever door took it.
    calls: Vec<Call>,
    /// Every [`RootSet`] published at an allocation, in order.
    roots: Vec<PublishedRoots>,
    /// Opt into the in-machine door ([`ExecutionContext::wants_bif_in_machine`]).
    in_machine: bool,
    /// Dispatches that arrived through [`ExecutionContext::call_bif_outcome`].
    plain_door_calls: usize,
    /// Dispatches that arrived through
    /// [`ExecutionContext::call_bif_in_machine_outcome`].
    in_machine_door_calls: usize,
}

impl ParkHost {
    /// A host that suspends `suspends_left` times before answering the value.
    fn new(suspends_left: usize) -> ParkHost {
        ParkHost {
            arena: MinimalContext::new(),
            suspends_left,
            calls: Vec::new(),
            roots: Vec::new(),
            in_machine: false,
            plain_door_calls: 0,
            in_machine_door_calls: 0,
        }
    }

    /// The same host, opted into the **in-machine** door.
    fn in_machine(suspends_left: usize) -> ParkHost {
        ParkHost {
            in_machine: true,
            ..ParkHost::new(suspends_left)
        }
    }

    /// The shared answer: record the dispatch, then suspend while the budget lasts.
    fn answer(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> BifOutcome {
        self.calls
            .push((module.to_string(), function.to_string(), arity, args.to_vec()));
        if self.suspends_left > 0 {
            self.suspends_left -= 1;
            return BifOutcome::Suspend;
        }
        BifOutcome::Return(make_small(BIF_RESULT))
    }

    /// Copy the published root set out of the borrowed slices.
    fn record_roots(&mut self, roots: &RootSet<'_>) {
        let live = roots.live_len();
        self.roots.push(PublishedRoots {
            live,
            x: roots.x[..live].to_vec(),
            stack: roots.stack.to_vec(),
            fr: roots.fr.to_vec(),
        });
    }
}

impl ExecutionContext for ParkHost {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        // The plain door cannot carry a suspend, so it never suspends here. The
        // engine only reaches it when the host did not override the outcome door.
        self.calls
            .push((module.to_string(), function.to_string(), arity, args.to_vec()));
        Ok(make_small(BIF_RESULT))
    }

    fn call_bif_outcome(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<BifOutcome> {
        self.plain_door_calls += 1;
        Ok(self.answer(module, function, arity, args))
    }

    fn wants_bif_in_machine(&self) -> bool {
        self.in_machine
    }

    fn call_bif_in_machine_outcome(
        &mut self,
        _machine: &Machine,
        _entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<BifOutcome> {
        self.in_machine_door_calls += 1;
        Ok(self.answer(module, function, arity, args))
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }

    /// Record the live root set, then serve the words from the arena.
    ///
    /// This is the engine's per-built-term admission point, so it is where a
    /// collecting host would relocate the heap — and therefore the exact place the
    /// suspended operands must be visible.
    fn alloc_words_rooted(&mut self, n: usize, roots: RootSet<'_>) -> Result<*mut Eterm> {
        self.record_roots(&roots);
        self.arena.alloc_words(n)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
/// The interned value of `name` in `module`.
fn atom(module: &Module, name: &str) -> TermAtom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} is interned in the fixture module"))
}

/// The typed reason atom of an engine raise.
fn reason(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .unwrap_or_else(|| panic!("the engine's typed error names its reason"))
            .to_string(),
        other => panic!("expected a typed raise, got {other:?}"),
    }
}

/// Run the fixture's `run/0` to its **first** park (or result).
fn first_slice(
    module: &Module,
    name: TermAtom,
    ctx: &mut ParkHost,
) -> Result<(RunOutcome, Machine)> {
    let mut machine = Machine::new();
    let outcome = machine.run_export_outcome(module, name, 0, &[], ctx)?;
    Ok((outcome, machine))
}

/// Drive `module`'s `run/0` from entry to `RunOutcome::Returned`, resuming at every
/// reported park — the host's whole pump loop.
fn drive_to_value(
    module: &Module,
    name: TermAtom,
    ctx: &mut ParkHost,
    tier: Tier,
) -> Result<(Eterm, Machine, usize)> {
    let mut machine = Machine::new();
    let mut outcome = machine.run_export_outcome_with_tier(module, name, 0, &[], ctx, tier)?;
    let mut parks = 0usize;
    // A fixture that parks more than a handful of times is a harness bug, not a
    // runtime property: bound the loop rather than spin.
    while let RunOutcome::Suspend { func, pc } = outcome {
        parks += 1;
        assert!(
            parks <= 8,
            "the fixture parked more than 8 times; the host's pump must make progress"
        );
        outcome = machine.resume_outcome_with_tier(module, func, pc, ctx, tier)?;
    }
    match outcome {
        RunOutcome::Returned(value) => Ok((value, machine, parks)),
        other => panic!("the resumed run must finish with a value, got {other:?}"),
    }
}

/// Whether two terms are **structurally** equal (`=:=`), resolving atom names through
/// `module`'s table.
///
/// A boxed term's raw word is its heap address, so two runs that build the same tuple
/// in two different contexts hold different words for the same value: every
/// cross-run comparison here is structural, never raw.
fn same_term(module: &Module, a: Eterm, b: Eterm) -> bool {
    beam_term::eq(a, b, &module.atom_table)
}

/// Assert `value` is the 2-tuple `{7, 8}` — the boxed operand the fixture built.
fn assert_operand_tuple(value: Eterm, what: &str) {
    assert_eq!(
        terms::tuple_arity(value).unwrap_or_else(|err| panic!("{what} must be a tuple: {err:?}")),
        2,
        "{what} must be the 2-tuple the fixture built"
    );
    assert_eq!(
        terms::tuple_element(value, 0).expect("{what} element 0"),
        make_small(7),
        "{what}: element 0 must survive the park"
    );
    assert_eq!(
        terms::tuple_element(value, 1).expect("{what} element 1"),
        make_small(8),
        "{what}: element 1 must survive the park"
    );
}

// ---------------------------------------------------------------------------
// (1) the park address
// ---------------------------------------------------------------------------
/// A suspend parks on the **BIF instruction itself**, with nothing applied.
///
/// The destination register is the crispest proof: it held a sentinel before the
/// call, and a suspend must leave it holding the sentinel — a value would have
/// overwritten it, and a `pc + 1` resume would have skipped the BIF entirely.
#[test]
fn a_suspending_bif_parks_on_its_own_instruction() {
    let (module, name) = suspending_module();
    let sentinel = atom(&module, SENTINEL);
    let mut ctx = ParkHost::new(1);
    let (outcome, machine) =
        first_slice(&module, name, &mut ctx).expect("the fixture must park, not fail");

    assert_eq!(
        outcome,
        RunOutcome::Suspend {
            func: PARK_FUNC,
            pc: PARK_PC
        },
        "a BIF suspend must report the BIF instruction's own address (func {PARK_FUNC}, pc {PARK_PC}), \
         not the next one: resumption re-executes that instruction"
    );
    assert_eq!(
        machine.current_code(),
        Some((PARK_FUNC, PARK_PC)),
        "the machine's recorded execution position must be the parking instruction, so a host can \
         read it back through the introspection accessors"
    );
    assert_eq!(
        machine.x(0),
        Some(make_atom(sentinel)),
        "a suspended instruction is NOT applied: x(0) must still hold its pre-call value, never the \
         value the abandoned attempt would have computed"
    );
    assert_eq!(
        ctx.calls.len(),
        1,
        "the host must see exactly one dispatch before the park"
    );
}

// ---------------------------------------------------------------------------
// (2) the re-issued call
// ---------------------------------------------------------------------------
/// Resumption **re-issues** the BIF with exactly the operands the parked call saw.
///
/// The engine copies no arguments: the second dispatch reads them from the machine's
/// own registers and stack, so "the same `Eterm`s" is the observable proof that the
/// park left the operands in place.
#[test]
fn a_resumed_bif_is_reissued_with_the_same_arguments() {
    let (module, name) = suspending_module();
    let mut ctx = ParkHost::new(1);
    let (outcome, mut machine) =
        first_slice(&module, name, &mut ctx).expect("the fixture must park, not fail");
    let RunOutcome::Suspend { func, pc } = outcome else {
        panic!("expected a BIF park, got {outcome:?}");
    };
    let resumed = machine
        .resume_outcome(&module, func, pc, &mut ctx)
        .expect("the resumed BIF returns a value, so the run must finish");

    assert_eq!(
        ctx.calls.len(),
        2,
        "a park plus a resume is exactly two dispatches of the same BIF: one that suspended and one \
         re-issued from the instruction"
    );
    assert_eq!(
        ctx.calls[0].0, BIF_MODULE,
        "the parked call must name the imported BIF's module"
    );
    assert_eq!(
        ctx.calls[0].1, BIF_FUNCTION,
        "the parked call must name the imported BIF's function"
    );
    assert_eq!(
        ctx.calls[0].2, 2,
        "the parked call must be dispatched at the import's declared arity"
    );
    assert_eq!(
        ctx.calls[0].3, ctx.calls[1].3,
        "the re-issued call must see the SAME operands: the engine holds no suspended-BIF state, so \
         it re-reads them from x(1)/y(0) and a difference would mean the park wrote something"
    );
    assert_eq!(
        ctx.calls.len(),
        ctx.plain_door_calls,
        "with the in-machine gate off, every dispatch goes through the plain outcome door"
    );
    assert_eq!(
        resumed,
        RunOutcome::Returned(ctx.calls[0].3[0]),
        "the resumed run must finish with the fixture's return value (it returns the x(1) operand), \
         so the re-issued call's value reached x(0) and the following instructions ran"
    );
}

// ---------------------------------------------------------------------------
// (3) the rooting argument
// ---------------------------------------------------------------------------
/// **The crux.** A suspended BIF's operands are the machine's own root state.
///
/// Box (a): after the park, the operands are still readable from the machine —
/// `x(1)` (an X register) and `y(0)` (a Y slot, i.e. `roots.stack`) — because the
/// engine copied them nowhere.
///
/// Box (b): the first allocation **after** the resume publishes a [`RootSet`] that
/// contains both words, so a moving collector may relocate them and the re-executed
/// instruction (which re-reads its operands) sees the moved terms. The destination
/// register is still unwritten at the moment that root set is published.
#[test]
fn the_suspended_operands_stay_in_the_machines_root_set() {
    let (module, name) = suspending_module();
    let sentinel = atom(&module, SENTINEL);
    let mut ctx = ParkHost::new(1);
    let (outcome, mut machine) =
        first_slice(&module, name, &mut ctx).expect("the fixture must park, not fail");
    let RunOutcome::Suspend { func, pc } = outcome else {
        panic!("expected a BIF park, got {outcome:?}");
    };

    // The two operands the parked instruction will re-read.
    let x_operand = ctx.calls[0].3[0];
    let stack_operand = ctx.calls[0].3[1];
    assert_operand_tuple(
        x_operand,
        "the X-register operand of the parked BIF dispatch",
    );
    assert_eq!(
        terms::tuple_arity(stack_operand).expect("the stack operand is a tuple"),
        1,
        "the Y-slot operand of the parked BIF dispatch must be the 1-tuple the fixture built"
    );

    // (a) the operands live in the machine, not in a Rust local.
    assert_eq!(
        machine.x(1),
        Some(x_operand),
        "(a) x(1) must still hold the parked BIF's X-register operand: it is the machine's state, \
         which is what a resume re-reads"
    );
    assert_eq!(
        machine.y(0),
        Some(stack_operand),
        "(a) y(0) must still hold the parked BIF's Y-slot operand (the engine publishes Y slots \
         through `roots.stack`, never a saved argument vector)"
    );
    assert_eq!(
        machine.stack_depth(),
        1,
        "(a) the fixture's one Y slot must still be on the Erlang stack after the park"
    );
    assert_eq!(
        machine.x(0),
        Some(make_atom(sentinel)),
        "(a) the destination register must be unwritten at the park"
    );

    // (b) the root set published after the resume contains both operands.
    let before = ctx.roots.len();
    let resumed = machine
        .resume_outcome(&module, func, pc, &mut ctx)
        .expect("the resumed BIF returns a value, so the run must finish");
    assert_eq!(
        ctx.roots.len(),
        before + 1,
        "(b) the `put_tuple2` after the resume must publish exactly one more root set; a different \
         count means the admission point moved and this test no longer inspects the right one"
    );
    let published = ctx.roots.last().expect("a published root set");
    assert!(
        published.x[..published.live].contains(&x_operand),
        "(b) the X-register operand must be inside `roots.x[..live_len]` ({} live words): that is \
         the slice a moving collector rewrites, so the re-executed BIF reads the relocated term",
        published.live
    );
    assert!(
        published.stack.contains(&stack_operand),
        "(b) the Y-slot operand must be inside `roots.stack`: a Y slot is a root only through the \
         stack slice"
    );
    assert!(
        !published.fr.contains(&x_operand),
        "(b) the operand must not be reported as a float-register slot: `roots.fr` holds the FR \
         file, which the fixture never touches"
    );

    // The resumed run returned the (rootable) operand, so the re-execution really
    // ran on the machine's own state.
    let RunOutcome::Returned(value) = resumed else {
        panic!(
            "the resumed run must finish with a value, got {resumed:?} — a second park would mean \
             the hook's budget is wrong, not that the engine parked"
        );
    };
    assert_operand_tuple(value, "the resumed run's return value");
    assert_eq!(
        machine.x(1),
        Some(x_operand),
        "the operand word must be unchanged when nothing relocated it (this host is not a \
         collector): the park itself never rewrites a root"
    );
}

// ---------------------------------------------------------------------------
// (4) suspension is not observable in the result
// ---------------------------------------------------------------------------
/// A park/resume cycle changes **nothing** observable.
///
/// The same module runs under two hosts — one that never suspends and one that
/// suspends twice — and the two runs must agree on the outcome and on every register
/// the fixture touched.
#[test]
fn a_suspended_then_resumed_run_equals_a_run_that_never_suspends() {
    let (module, name) = suspending_module();

    let mut eager = ParkHost::new(0);
    let (eager_value, eager_machine, eager_parks) =
        drive_to_value(&module, name, &mut eager, Tier::Interpreter)
            .expect("the never-suspending host runs straight through");
    assert_eq!(
        eager_parks, 0,
        "a host that never suspends must never park: the channel must not invent one"
    );

    let mut parking = ParkHost::new(2);
    let (parked_value, parked_machine, parked_parks) =
        drive_to_value(&module, name, &mut parking, Tier::Interpreter)
            .expect("the parking host resumes to the same value");
    assert_eq!(
        parked_parks, 2,
        "the parking host must observe exactly the two parks it asked for"
    );
    assert_eq!(
        parking.calls.len(),
        3,
        "two parks plus one value is three dispatches of the same BIF"
    );

    assert!(
        same_term(&module, eager_value, parked_value),
        "suspending must not change the value: the resumed run re-executes the same instructions, \
         and the re-issued BIF's value lands in the same register (eager {eager_value:?}, parked \
         {parked_value:?})"
    );
    assert_operand_tuple(parked_value, "the parked run's value");
    for reg in 0..5u32 {
        let eager_reg = eager_machine.x(reg).expect("a register");
        let parked_reg = parked_machine.x(reg).expect("a register");
        assert!(
            same_term(&module, eager_reg, parked_reg),
            "x({reg}) must be structurally identical between the two runs \
             ({eager_reg:?} vs {parked_reg:?}): a park is a scheduling event, never a register-file \
             difference"
        );
    }
    assert_eq!(
        eager_machine.stack_depth(),
        parked_machine.stack_depth(),
        "both runs must end with the same Erlang stack depth"
    );
}

// ---------------------------------------------------------------------------
// (5) tier parity
// ---------------------------------------------------------------------------
/// Both execution tiers report the **same** park and resume it identically.
///
/// Tier parity is the pattern of `tests/receive.rs`: whatever the interpreter reports,
/// the native baseline tier must report, including the resume — a tier that could park
/// but not resume would strand a host that picked it.
#[test]
fn both_tiers_report_and_resume_the_same_suspend() {
    let (module, name) = suspending_module();
    let mut outcomes = Vec::new();
    let mut values = Vec::new();
    // A term belongs to the `MinimalContext` that built it, and dropping a
    // `MinimalContext` returns its carrier blocks to `beam_alloc`'s process-global
    // segment pool, where the next arena in the process reclaims and overwrites them.
    // Both tiers' resumed values are compared *after* the loop below, so every tier's
    // arena is hoisted here and kept alive until the last read (`drop(hosts)`).
    let mut hosts: Vec<ParkHost> = Vec::new();
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let mut ctx = ParkHost::new(1);
        let (outcome, mut machine) =
            first_slice(&module, name, &mut ctx).expect("the fixture must park on every tier");
        outcomes.push((tier, outcome));
        let RunOutcome::Suspend { func, pc } = outcome else {
            panic!("{tier:?}: expected a BIF park, got {outcome:?}");
        };
        let resumed = machine
            .resume_outcome_with_tier(&module, func, pc, &mut ctx, tier)
            .unwrap_or_else(|err| panic!("{tier:?}: the resumed run must finish, got {err:?}"));
        assert_eq!(
            ctx.calls.len(),
            2,
            "{tier:?}: a park and a resume are two dispatches on every tier"
        );
        assert_eq!(
            ctx.calls[0].3, ctx.calls[1].3,
            "{tier:?}: the re-issued call must see the same operands on every tier"
        );
        match resumed {
            RunOutcome::Returned(value) => values.push((tier, value)),
            other => panic!("{tier:?}: the resumed run must return a value, got {other:?}"),
        }
        // Keep this tier's arena alive: the value just stashed out of the iteration
        // lives on this host's heap.
        hosts.push(ctx);
    }
    let (interpreter_tier, interpreter_outcome) = outcomes[0];
    let (jit_tier, jit_outcome) = outcomes[1];
    assert_eq!(
        interpreter_outcome, jit_outcome,
        "the {interpreter_tier:?} and {jit_tier:?} tiers must report the SAME park address: the \
         block ABI's suspend sentinel carries the instruction's own (func, pc)"
    );
    assert!(
        same_term(&module, values[0].1, values[1].1),
        "the two tiers must resume to the same value ({} vs {}): the tier seam may change the code \
         path, never the result",
        values[0].0.name(),
        values[1].0.name()
    );
    assert_operand_tuple(values[0].1, "the interpreter's resumed value");
    // The last read of either tier's term is above: the per-tier arenas can go now.
    drop(hosts);
}

// ---------------------------------------------------------------------------
// (6) the class-B site
// ---------------------------------------------------------------------------
/// A suspend behind `erlang:apply/3` is a **typed gap**, never a park or a value.
///
/// `apply/3` rewrites `x(0..arity)` before dispatching the callee, so re-executing the
/// instruction on resume would apply a *different* call. The engine must say so.
#[test]
fn a_suspend_through_apply_is_a_typed_gap() {
    let (module, name) = apply_module();
    let mut ctx = ParkHost::new(1);
    let mut machine = Machine::new();
    let err = machine
        .run_export_outcome(&module, name, 0, &[], &mut ctx)
        .expect_err("a suspend at the non-idempotent apply/3 site must be an error");

    assert_eq!(
        reason(err),
        "unsupported_opcode: bif suspend at a non-idempotent dispatch site: apply/3 rewrites x(0..arity)",
        "the apply/3 site must report the documented typed error: it cannot honour a suspend, and \
         the alternative (a park) would re-apply a clobbered register file on resume"
    );
    assert_eq!(
        ctx.calls.len(),
        1,
        "the callee was dispatched exactly once: the engine reports the gap instead of retrying"
    );
    assert_eq!(
        ctx.calls[0].0,
        BIF_MODULE,
        "the gap must come from the callee BIF the host tried to dispatch"
    );
}

// ---------------------------------------------------------------------------
// (7) the in-machine door's outcome twin
// ---------------------------------------------------------------------------
/// A host that opted into the in-machine door is reached through its **outcome** twin.
///
/// The engine calls exactly one door per dispatch: with `wants_bif_in_machine` `true`
/// it is `call_bif_in_machine_outcome`, so a host that overrode only the *plain*
/// outcome door would never see (or answer) a suspend.
#[test]
fn the_in_machine_outcome_door_carries_the_suspend() {
    let (module, name) = suspending_module();
    let mut ctx = ParkHost::in_machine(1);
    let (outcome, mut machine) =
        first_slice(&module, name, &mut ctx).expect("the fixture must park, not fail");
    assert_eq!(
        outcome,
        RunOutcome::Suspend {
            func: PARK_FUNC,
            pc: PARK_PC
        },
        "the in-machine outcome door must carry a suspend exactly like the plain one"
    );
    assert_eq!(
        ctx.in_machine_door_calls, 1,
        "with the gate on, the dispatch must arrive through `call_bif_in_machine_outcome`: the \
         engine never falls back to the plain door for a opted-in host"
    );
    assert_eq!(
        ctx.plain_door_calls, 0,
        "the plain outcome door must not be reached at all once the host opted in"
    );
    let RunOutcome::Suspend { func, pc } = outcome else {
        panic!("expected a park");
    };
    assert!(
        matches!(
            machine
                .resume_outcome(&module, func, pc, &mut ctx)
                .expect("the resumed run must finish"),
            RunOutcome::Returned(_)
        ),
        "the in-machine host's resume must finish with a value"
    );
    assert_eq!(
        ctx.in_machine_door_calls, 2,
        "the resume re-issues the BIF through the same in-machine door"
    );
}

// ---------------------------------------------------------------------------
// (8) the non-blocking entries
// ---------------------------------------------------------------------------
/// The non-blocking entries fold a park into `error::suspended()` — not `blocked()`.
///
/// A host reading the reason atom must be able to tell "the mailbox was empty" from
/// "this BIF needs pumping": the two have different release conditions.
#[test]
fn the_non_blocking_entries_report_a_suspend_as_a_typed_error() {
    let (module, name) = suspending_module();

    let mut ctx = ParkHost::new(1);
    let mut machine = Machine::new();
    let export_err = machine
        .run_export(&module, name, 0, &[], &mut ctx)
        .expect_err("`run_export` cannot suspend a process");
    let export_reason = reason(export_err);
    assert_eq!(
        export_reason,
        "unsupported_opcode: run: process suspended by a BIF (host must resume)",
        "`run_export` must fold the park into the typed `suspended()` error"
    );
    assert_ne!(
        export_reason,
        reason(error::blocked()),
        "a BIF suspend must stay distinguishable from an empty-mailbox `receive`: they are \
         released by different events"
    );

    let mut ctx = ParkHost::new(1);
    let mut machine = Machine::new();
    let label_err = machine
        .run_label(&module, Label(2), 0, &[], &mut ctx)
        .expect_err("`run_label` cannot suspend a process");
    assert_eq!(
        reason(label_err),
        export_reason,
        "`run_label` must fold the park into the same typed error as `run_export`"
    );
}

// ---------------------------------------------------------------------------
// (9) the defaulted doors
// ---------------------------------------------------------------------------
/// A host that implements only `call_bif` behaves exactly as it did before the channel
/// existed: the defaulted outcome door forwards, wraps, and the instruction applies.
#[test]
fn the_default_doors_keep_a_plain_host_byte_identical() {
    let (module, name) = plain_module();
    // No outcome override anywhere: `MinimalContext`'s plain hook is the whole host.
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|module, function, arity, args| {
        assert_eq!(
            (module, function, arity, args.len()),
            (BIF_MODULE, "plain", 0, 0),
            "the fixture dispatches exactly one zero-arity BIF, named as the import table says"
        );
        Ok(make_small(BIF_RESULT))
    }));
    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&module, name, 0, &[], &mut ctx)
        .expect("a returning BIF must not disturb the run");
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(BIF_RESULT)),
        "the defaulted outcome door must forward to `call_bif` and wrap the value in \
         `BifOutcome::Return`, leaving the run byte-identical"
    );
    assert_eq!(
        machine.x(0),
        Some(make_small(BIF_RESULT)),
        "the destination register must be written: `dst` then `pc + 1` on the default path"
    );
}

// ---------------------------------------------------------------------------
// (10) repeated parks
// ---------------------------------------------------------------------------
/// Three parks then a value: four dispatches, the same operands every time, one value.
///
/// A host pumps its waiting resource as many times as it needs, and the engine must
/// stay re-entrant at the parked instruction without accumulating state.
#[test]
fn many_suspends_then_a_value() {
    let (module, name) = suspending_module();
    let mut ctx = ParkHost::new(3);
    let (value, machine, parks) = drive_to_value(&module, name, &mut ctx, Tier::Interpreter)
        .expect("three parks then a value must complete");

    assert_eq!(parks, 3, "the host asked for three parks and must see three");
    assert_eq!(
        ctx.calls.len(),
        4,
        "three suspends plus one value is four dispatches of the same BIF"
    );
    for (index, call) in ctx.calls.iter().enumerate().skip(1) {
        assert_eq!(
            call.3, ctx.calls[0].3,
            "dispatch {index} must see the same operands as the first: every resume re-executes the \
             instruction from the machine's own state, so nothing may drift"
        );
    }
    assert_operand_tuple(value, "the value after three parks");
    assert_eq!(
        machine.x(0),
        Some(value),
        "the run finished with `x(0)` holding the returned value"
    );
    assert!(
        machine.pending_exception().is_none(),
        "a park is not an exception: the machine must end with no pending exception"
    );
    // The fixture returns `x(1)`, so the park left `x(1)` alone across all three cycles.
    let operand = machine
        .x(1)
        .expect("the machine always has an x(1) register");
    assert_operand_tuple(operand, "x(1) at the end of the run");
}
