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
//! The `call_ext*`-encoded `erlang:apply/3` seam, driven through the **real
//! driver loop**.
//!
//! # Why the seam exists
//!
//! `erlang:apply/3` is a **floor** module in the runtime, so
//! [`ExecutionContext::resolve_external_call`] declines it and a `call_ext*`
//! would otherwise fold into the host's *synchronous* `apply` BIF — which runs the
//! callee on a fresh machine, so a `receive` inside the callee dies in the
//! uncatchable typed [`blocked`](beam_engine::blocked) gap at the call site instead
//! of suspending *this* process. C instead resolves the export and jumps into its
//! code in the same process (`beam_common.c` `fixed_apply` → `apply()`), so the
//! callee's own `wait` suspends the caller.
//!
//! `Machine::resolve_call_ext_apply` (the crate-internal seam) reproduces that for
//! `call_ext`/`call_ext_only`/`call_ext_last` of `erlang:apply/3` only.
//!
//! # What this suite pins
//!
//! 1. **A blocking callee suspends the CALLER and resumes to the oracle's value.**
//!    The caller's `run/3` seeds `x(0..3)` with `M`/`F`/`Args` and `call_ext`s
//!    `erlang:apply/3`; the resolved target is a cross-module callee whose body is a
//!    bare `receive`. `run_export_outcome` must report [`RunOutcome::Wait`] (the
//!    *calling* process suspends **in-machine**) and `resume_outcome` must return
//!    the injected message — not the folded `blocked()` error.
//! 2. **`erlang:apply/2` is NOT special-cased.** `erlang:apply/2` is
//!    `apply(Fun, Args)` — a *fun* applied to an argument list — which C compiles to
//!    the dedicated `i_apply_fun` instruction (`emu/ops.tab:872`) and never routes
//!    through `apply()`/`fixed_apply` (whose special case is
//!    `beam_common.c:1554-1557`, arity **3** only). A `g/2` that is
//!    `call_ext_only 2 erlang:apply/2` must reach
//!    `call_bif("erlang", "apply", 2, [Fun, Args])` — the host's own `apply/2` —
//!    with exactly those two arguments. (Before the fix the seam fabricated
//!    `Module:Function()` from them, so the BIF was never reached.)
//! 3. **Every shape error is the catchable `badarg` the oracle produces**: a non-atom
//!    `M` or `F`, and a non-list / non-proper `Args`, caught by a live `try` frame
//!    (`x(1)` = the `badarg` atom); with **no** catch frame the typed error
//!    propagates unchanged.
//! 4. **The nested `apply of apply/3` unwrap** resolves to the same target as the
//!    flat form.
//! 5. **A real `.beam` fixture** (`tests/fixtures/applyrecv.{erl,beam}`, compiled
//!    with the system `erlc` — see the `.erl` for the `beam_disasm` shape and the
//!    exact oracle transcript) reproduces the oracle's value end to end: the engine
//!    suspends in the callee, then resumes to `42`. A second test re-runs the same
//!    scenario under the live system `erl` and **skips loudly** when there is no
//!    runnable oracle.
//!
//! The synthetic modules are the smallest bytecode shapes that reproduce the frame
//! discipline; the entry module names every atom its landing may resolve
//! (`badarg`/`error`/`exit`/`throw`) so a caught raise is byte-identical to C's.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;

use beam_common::{BeamError, ExitReason, Raise, Result};
use beam_engine::context::BifHook;
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext, RunOutcome};
use beam_loader::{
    load_file, Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand,
};
use beam_term::{make_atom, make_small, Atom, Eterm, NIL};

/// The committed oracle fixture (`erlc +deterministic applyrecv.erl`).
const APPLYRECV_BEAM: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/applyrecv.beam");
/// The fixture directory (appended with `-pz`, never `-pa`).
const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

// ---------------------------------------------------------------------------
// Minimal synthetic-module scaffolding
// ---------------------------------------------------------------------------

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A builder for one synthetic module with its own atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        // Every atom a landing may resolve: the three class names (the raise
        // landing installs one in `x(3)`) and the `badarg` reason the `apply`
        // shape errors raise. `erlang`/`apply` are interned per module by the
        // `import` below (a fixture that also names `c`/`recv` interns them itself).
        for name in ["error", "exit", "throw", "EXIT", "badarg"] {
            module.atom_table.intern(name);
        }
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    fn import(&mut self, module_name: &str, function_name: &str, arity: u32) -> ImportRef {
        let module = self.atom(module_name);
        let function = self.atom(function_name);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef((self.module.imports.len() - 1) as u32)
    }

    /// Append a function; its entry label is the first label in `code`.
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
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

/// `module:name`'s atom as the loader's own [`Atom`] — a table **index**, not a
/// term — resolved through `module`'s own table.
///
/// [`atom_term`] builds the *term* form; an API that takes the raw index
/// (`Module::export`, `atom_name`) needs this one. A name the fixture forgot to
/// intern is a test bug, so it panics rather than falling back to another table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in the fixture"))
}

/// `module:name`'s atom term, resolved through `module`'s own table.
fn atom_term(module: &Module, name: &str) -> Eterm {
    make_atom(atom(module, name))
}

// ---------------------------------------------------------------------------
// The test host: the cross-module seam + the BIF floor + a tiny mailbox/arena
// ---------------------------------------------------------------------------

/// A BIF floor backed by a [`MinimalContext`] (so the tests get its reserved-heap
/// allocator for building argument terms), plus the cross-module resolver and the
/// mailbox the receive seam drives.
///
/// * `callees` — the modules the seam resolves, keyed by name. A `call_ext*` to a
///   name in this list returns `Some((Arc<Module>, export_label))`; anything else
///   returns `Ok(None)` (the synchronous path).
/// * `messages`/`scan` — a faithful FIFO mailbox: `receive_next` peeks at the scan
///   position, `receive_skip` advances it, `receive_remove` unlinks the current
///   message and repositions at the first, `receive_reset` repositions at the first.
/// * `core` — the BIF floor **and** the term arena (`alloc_words`), so a term the
///   test builds stays valid for the life of the context.
struct TestCtx {
    core: MinimalContext,
    callees: Vec<(String, Arc<Module>)>,
    messages: Vec<Eterm>,
    scan: usize,
}

impl TestCtx {
    fn new(callees: Vec<(String, Arc<Module>)>, bif: BifHook) -> TestCtx {
        let mut core = MinimalContext::new();
        core.set_bif_hook(bif);
        TestCtx {
            core,
            callees,
            messages: Vec::new(),
            scan: 0,
        }
    }

    /// Queue `message` as if it had been sent to this mailbox.
    fn push(&mut self, message: Eterm) {
        self.messages.push(message);
    }
}

impl ExecutionContext for TestCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.core.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.core.alloc_words(n)
    }

    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        for (name, callee) in &self.callees {
            if name != module {
                continue;
            }
            let Some(function) = callee.atom_table.lookup(function) else {
                continue;
            };
            if let Some(label) = callee.export(function, arity) {
                return Ok(Some((Arc::clone(callee), label)));
            }
        }
        Ok(None)
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(self.messages.get(self.scan).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.scan += 1;
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        if self.scan < self.messages.len() {
            self.messages.remove(self.scan);
        }
        self.scan = 0;
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.scan = 0;
        Ok(())
    }
}

/// A BIF floor that rejects everything.
fn rejecting() -> BifHook {
    Box::new(|module, function, _arity, _args| {
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    })
}

/// A recording BIF floor: logs every `(module, function, arity, args)` it is asked
/// for and answers `erlang:apply/2` with `42`.
type BifLog = Rc<RefCell<Vec<(String, String, u32, Vec<Eterm>)>>>;

fn recording_apply2(log: BifLog) -> BifHook {
    Box::new(move |module, function, arity, args| {
        log.borrow_mut().push((
            module.to_string(),
            function.to_string(),
            arity,
            args.to_vec(),
        ));
        if module == "erlang" && function == "apply" && arity == 2 {
            return Ok(make_small(42));
        }
        Err(beam_engine::unsupported(&format!(
            "bif {module}:{function}/{arity}"
        )))
    })
}

/// The reason of a typed engine raise, or a panic naming what arrived instead.
fn reason(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .expect("a typed engine error names its reason")
            .to_string(),
        other => panic!("expected a typed raise, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Synthetic modules
// ---------------------------------------------------------------------------

/// The caller `m:run/3` — `run(M, F, A) -> erlang:apply(M, F, A)`, the **non-tail**
/// `call_ext` form (so the callee's return reports back into `m`'s own frame):
///
/// ```text
/// m:run/3:
///     allocate 0
///     call_ext 3 erlang:apply/3      ; stores the continuation in m's frame
///     deallocate 0
///     return
/// ```
///
/// `x(0)`/`x(1)`/`x(2)` are seeded from the export arguments (`M`/`F`/`Args`).
fn caller_apply3_module() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    // The callee name the resolver answers, plus the atoms the nested-unwrap test
    // builds a runtime `[M, F, Args]` list from.
    for name in ["erlang", "apply", "c", "recv"] {
        f.atom(name);
    }
    let target = f.import("erlang", "apply", 3);
    let run = f.add_function(
        "run",
        3,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 3,
            },
            Instr::CallExt { target, arity: 3 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 3, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// [`caller_apply3_module`]'s **tail** twin — `call_ext_only 3 erlang:apply/3`
/// (the shape the compiler emits for `run(M,F,A) -> erlang:apply(M,F,A)`), so the
/// resolved target reuses the caller's frame.
fn caller_apply3_tail_module() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    // The same caller-supplied atoms [`caller_apply3_module`] interns: the tests
    // seed `x(0)`/`x(1)` with `c`/`recv` terms built from this very table.
    for name in ["erlang", "apply", "c", "recv"] {
        f.atom(name);
    }
    let target = f.import("erlang", "apply", 3);
    let run = f.add_function(
        "run",
        3,
        vec![
            Instr::Label(Label(100)),
            Instr::CallExtOnly { target, arity: 3 },
        ],
    );
    f.add_export("run", 3, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The `erlang:apply/2` caller: `m:g/2` is exactly
/// `call_ext_only 2 erlang:apply/2`, with `x(0)` = the fun and `x(1)` = its
/// argument list (seeded from the export arguments).
fn caller_apply2_module() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let target = f.import("erlang", "apply", 2);
    let g = f.add_function(
        "g",
        2,
        vec![
            Instr::Label(Label(100)),
            Instr::CallExtOnly { target, arity: 2 },
        ],
    );
    f.add_export("g", 2, g);
    let g_atom = f.atom("g");
    (f.finish(), g_atom)
}

/// The caller whose `erlang:apply/3` sits inside a live `try` frame; the handler
/// returns `x(1)` — the **reason** (`try_case` puts the reason in `x(1)`):
///
/// ```text
/// m:run/3:
///     allocate 1
///     try y0 {handler}
///     call_ext 3 erlang:apply/3
///     try_end y0
///     deallocate 1
///     return
///   {handler}:
///     try_case y0
///     move x1 -> x0                 ; the reason the raise landed
///     deallocate 1
///     return
/// ```
fn try_caller_apply3_module() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    // `c`/`recv` are interned so the shape-error tests can build the `M`/`F`
    // argument terms from this module's own table (the raise itself never needs
    // them: it is the `badarg` atom the landing resolves). `boom` is the reason
    // atom a *host BIF error* landing resolves (the applied-target tests below),
    // so it must live in this module's table too.
    for name in ["erlang", "apply", "not_apply", "c", "recv", "boom"] {
        f.atom(name);
    }
    let target = f.import("erlang", "apply", 3);
    let run = f.add_function(
        "run",
        3,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 3,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(103),
            },
            Instr::CallExt { target, arity: 3 },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(103)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
        ],
    );
    f.add_export("run", 3, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The callee `c:recv/0` — a bare `receive` (function index `0`, so a suspension
/// address `Wait { func: 0, .. }` unambiguously names this module):
///
/// ```text
/// c:recv/0:
///     loop_rec wait 1 x0           ; candidate, or fall to the wait
///     remove_message
///     return
///     wait 1                       ; resume at the loop_rec once a message arrives
/// ```
fn callee_recv_module() -> Arc<Module> {
    let mut f = Fixture::new("c");
    let recv = f.add_function(
        "recv",
        0,
        vec![
            Instr::Label(Label(10)),
            Instr::LoopRec {
                fail: Label(11),
                dst: Operand::X(0),
            },
            Instr::RemoveMessage,
            Instr::Return,
            Instr::Label(Label(11)),
            Instr::Wait { target: Label(10) },
        ],
    );
    f.add_export("recv", 0, recv);
    Arc::new(f.finish())
}

fn callees() -> Vec<(String, Arc<Module>)> {
    vec![("c".to_string(), callee_recv_module())]
}

// ---------------------------------------------------------------------------
// (1) a blocking callee behind erlang:apply/3 suspends the CALLER
// ---------------------------------------------------------------------------

#[test]
fn a_receive_inside_the_applied_callee_suspends_the_caller_then_resumes() {
    // `m:run(c, recv, [])` → `erlang:apply(c, recv, [])` → `c:recv()` on an empty
    // mailbox. The seam resolves the applied export **in this machine**, so the
    // callee's `wait` suspends *this* process — a `RunOutcome::Wait`, NOT the folded
    // `error::blocked()` a synchronous host could only produce at the call site.
    let (caller, run) = caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(
            &caller,
            run,
            3,
            &[atom_term(&caller, "c"), atom_term(&caller, "recv"), NIL],
            &mut ctx,
        )
        .expect("a suspend is not an error");

    // The suspension address is the **callee's** `wait` (`c:recv/0` is function
    // index 0; `Label(10)` is its first instruction), proving the block happened
    // inside the applied callee and not at the caller's call site.
    assert_eq!(outcome, RunOutcome::Wait { func: 0, pc: 0 });

    // A message arrives; resume. The entry module is the run's `m` (context `0`);
    // the `(func, pc)` live in the callee's context, which the machine recovers.
    ctx.push(make_small(77));
    let resumed = machine
        .resume_outcome(&caller, 0, 0, &mut ctx)
        .expect("the resume completes");
    // The callee's `return` unwinds back into `m`'s own frame (the non-tail
    // `call_ext` stored the continuation there) and the run reports `x(0)`.
    assert_eq!(resumed, RunOutcome::Returned(make_small(77)));
}

#[test]
fn the_tail_call_form_suspends_and_resumes_the_same_way() {
    // The `call_ext_only 3` shape the compiler actually emits for
    // `run(M,F,A) -> erlang:apply(M,F,A)`: the callee reuses the caller's frame, so
    // the block/resume must report the same outcome as the non-tail form.
    let (caller, run) = caller_apply3_tail_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(
            &caller,
            run,
            3,
            &[atom_term(&caller, "c"), atom_term(&caller, "recv"), NIL],
            &mut ctx,
        )
        .expect("a suspend is not an error");
    assert_eq!(outcome, RunOutcome::Wait { func: 0, pc: 0 });

    ctx.push(make_small(7));
    let resumed = machine
        .resume_outcome(&caller, 0, 0, &mut ctx)
        .expect("the resume completes");
    assert_eq!(resumed, RunOutcome::Returned(make_small(7)));
}

#[test]
fn a_closed_receive_through_apply_returns_without_suspending() {
    // The message is already queued when the run starts: no suspension at all, and
    // the same value the oracle's `receive` yields.
    let (caller, run) = caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    ctx.push(make_small(5));
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(
            &caller,
            run,
            3,
            &[atom_term(&caller, "c"), atom_term(&caller, "recv"), NIL],
            &mut ctx,
        )
        .expect("the queued message satisfies the receive");
    assert_eq!(outcome, RunOutcome::Returned(make_small(5)));
}

// ---------------------------------------------------------------------------
// (2) erlang:apply/2 is NOT special-cased — it reaches the host's own BIF
// ---------------------------------------------------------------------------

#[test]
fn apply2_reaches_the_host_bif_with_the_fun_and_the_argument_list() {
    // `m:g(Fun, Args)` is literally `call_ext_only 2 erlang:apply/2`. The seam must
    // decline it (its `fixed_apply` shape is arity 3 only) so the arm falls through
    // to `call_import` → `call_bif("erlang", "apply", 2, [Fun, Args])`.
    //
    // Oracle: `erlang:apply(fun(A,B) -> A+B end, [1,2])` ⇒ `3` — i.e. `apply/2` is a
    // *fun* applied to an argument list. Under the old arity-2 branch the seam
    // consumed the call (reading `Fun` as the module and `Args` as the function,
    // with `Args = []`), the BIF was never reached, and this test's log stayed empty.
    let (module, g) = caller_apply2_module();
    let log: BifLog = Rc::new(RefCell::new(Vec::new()));
    let mut ctx = TestCtx::new(Vec::new(), recording_apply2(Rc::clone(&log)));

    // The two arguments the caller passes: a real (local) closure and its list.
    let fun = terms::make_fun(&mut ctx, 2, 4_242, 0, &[]).expect("build the fun");
    let args = terms::make_list(&mut ctx, &[make_small(1), make_small(2)]).expect("build the list");

    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&module, g, 2, &[fun, args], &mut ctx)
        .expect("the host BIF answers");

    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(42)),
        "the value is the host's own erlang:apply/2 result"
    );

    let calls = log.borrow();
    assert_eq!(calls.len(), 1, "exactly one BIF dispatch: {calls:?}");
    let (module_name, function_name, arity, args) = &calls[0];
    assert_eq!(
        (module_name.as_str(), function_name.as_str(), *arity),
        ("erlang", "apply", 2)
    );
    assert_eq!(args.len(), 2, "apply/2 takes (Fun, Args)");
    assert!(terms::is_fun(args[0]), "x(0) is the fun, not a module");
    assert_eq!(terms::fun_index(args[0]).unwrap(), 0);
    // x(1) is the *argument list* the caller passed (`[1,2]`), not the fabricated
    // empty `Args` the old arity-2 branch used.
    assert_eq!(terms::cons_head(args[1]).unwrap(), make_small(1));
    let tail = terms::cons_tail(args[1]).unwrap();
    assert_eq!(terms::cons_head(tail).unwrap(), make_small(2));
    assert!(beam_term::is_nil(terms::cons_tail(tail).unwrap()));
}
// ---------------------------------------------------------------------------
// (3) the shape errors are the catchable badarg the oracle produces
// ---------------------------------------------------------------------------

#[test]
fn a_non_atom_module_is_caught_as_badarg() {
    // `try erlang:apply(7, recv, []) catch C:R -> R end` ⇒ `badarg`.
    let (caller, run) = try_caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 3, &[make_small(7), atom_term(&caller, "recv"), NIL], &mut ctx)
        .expect("the badarg must be caught by the live try frame");

    assert_eq!(outcome, RunOutcome::Returned(atom_term(&caller, "badarg")));
}

#[test]
fn a_non_atom_function_is_caught_as_badarg() {
    // `try erlang:apply(c, 7, []) catch C:R -> R end` ⇒ `badarg`.
    let (caller, run) = try_caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 3, &[atom_term(&caller, "c"), make_small(7), NIL], &mut ctx)
        .expect("the badarg must be caught by the live try frame");

    assert_eq!(outcome, RunOutcome::Returned(atom_term(&caller, "badarg")));
}

#[test]
fn a_non_list_args_is_caught_as_badarg() {
    // `try erlang:apply(erlang, not_apply, 7) catch C:R -> R end` ⇒ `badarg`.
    let (caller, run) = try_caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(
            &caller,
            run,
            3,
            &[
                atom_term(&caller, "erlang"),
                atom_term(&caller, "not_apply"),
                make_small(7),
            ],
            &mut ctx,
        )
        .expect("the badarg must be caught by the live try frame");

    assert_eq!(outcome, RunOutcome::Returned(atom_term(&caller, "badarg")));
}

#[test]
fn a_non_proper_args_is_caught_as_badarg() {
    // `try erlang:apply(erlang, not_apply, [1|2]) catch C:R -> R end` ⇒ `badarg`
    // (C's `is_not_nil(tmp)` after the walk — the list tail must be `[]`).
    let (caller, run) = try_caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    let improper = terms::make_cons(&mut ctx, make_small(1), make_small(2)).expect("build [1|2]");
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(
            &caller,
            run,
            3,
            &[
                atom_term(&caller, "erlang"),
                atom_term(&caller, "not_apply"),
                improper,
            ],
            &mut ctx,
        )
        .expect("the badarg must be caught by the live try frame");

    assert_eq!(outcome, RunOutcome::Returned(atom_term(&caller, "badarg")));
}

#[test]
fn an_uncaught_shape_error_propagates_as_the_typed_badarg() {
    // With **no** catch frame the raise cannot land, so it propagates unchanged as
    // the typed `BeamError::BadArg` — never a fabricated atom and never a panic.
    let (caller, run) = caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    let mut machine = Machine::new();

    let error = machine
        .run_export_outcome(&caller, run, 3, &[make_small(7), atom_term(&caller, "recv"), NIL], &mut ctx)
        .expect_err("an uncaught shape error cannot land");

    assert!(matches!(error, BeamError::BadArg), "got {error:?}");
}

// ---------------------------------------------------------------------------
// (4) the nested apply of apply/3 unwrap
// ---------------------------------------------------------------------------

#[test]
fn the_nested_apply_of_apply_unwraps_to_the_same_target() {
    // `erlang:apply(erlang, apply, [c, recv, []])` — C's `apply()` unwrap loop
    // (`beam_common.c:1462-1497`) replaces `(erlang, apply, [M, F, Inner])` with
    // `(M, F, Inner)`, so this resolves to exactly the target of
    // `erlang:apply(c, recv, [])`: the same callee, the same suspension.
    let (caller, run) = caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    let nested = terms::make_list(
        &mut ctx,
        &[atom_term(&caller, "c"), atom_term(&caller, "recv"), NIL],
    )
    .expect("build [c, recv, []]");
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(
            &caller,
            run,
            3,
            &[
                atom_term(&caller, "erlang"),
                atom_term(&caller, "apply"),
                nested,
            ],
            &mut ctx,
        )
        .expect("a suspend is not an error");

    assert_eq!(
        outcome,
        RunOutcome::Wait { func: 0, pc: 0 },
        "the unwrap reaches the same cross-module callee"
    );

    ctx.push(make_small(11));
    let resumed = machine
        .resume_outcome(&caller, 0, 0, &mut ctx)
        .expect("the resume completes");
    assert_eq!(resumed, RunOutcome::Returned(make_small(11)));
}

// ---------------------------------------------------------------------------
// (5) the real-.beam oracle cross-check
// ---------------------------------------------------------------------------

/// The committed fixture, as the host's own module handle.
fn applyrecv_module() -> Arc<Module> {
    Arc::new(load_file(APPLYRECV_BEAM).expect("load applyrecv.beam"))
}

#[test]
fn the_fixtures_run3_is_a_call_ext_of_erlang_apply3() {
    // Pin the oracle *shape*: the fixture must keep lowering `run(M,F,A) ->
    // erlang:apply(M,F,A)` to `call_ext_only 3 {extfunc,erlang,apply,3}`. If a
    // regeneration ever folds the apply into a direct call this fails, rather than
    // the blocking test silently passing for the wrong reason.
    let module = applyrecv_module();
    let run = module
        .export(atom(&module, "run"), 3)
        .expect("the fixture exports run/3");
    // Cross-check: `exports`' own row for `run/3` names the same entry label.
    let run_export = module
        .exports
        .iter()
        .find(|e| module.atom_name(e.function) == Some("run") && e.arity == 3)
        .expect("run/3");
    assert_eq!(run_export.label, run);
    let body = module
        .functions
        .iter()
        .find(|f| module.atom_name(f.name) == Some("run") && f.arity == 3)
        .expect("the run/3 function");
    let call = body
        .code
        .iter()
        .find_map(|instr| match instr {
            Instr::CallExtOnly { target, arity } => Some((*target, *arity)),
            _ => None,
        })
        .expect("call_ext_only");
    assert_eq!(call.1, 3, "the apply is at arity 3");
    let import = &module.imports[call.0 .0 as usize];
    assert_eq!(module.atom_name(import.module), Some("erlang"));
    assert_eq!(module.atom_name(import.function), Some("apply"));
    assert_eq!(import.arity, 3);
}

#[test]
fn the_fixture_blocks_then_resumes_to_the_oracles_value() {
    // Oracle (OTP 29 / erts-17.1):
    //
    //     Self = self(),
    //     P = spawn(fun() -> Self ! {done, applyrecv:run(applyrecv, blocker, [])} end),
    //     P ! {reply, 42},
    //     receive {done, V} -> io:format("~w", [V]) end.   %% ⇒ 42
    //
    // The engine drives the same scenario in one machine: `run/3` applies the
    // fixture's own `blocker/0`, which blocks on the empty mailbox (a `Wait`), then
    // resumes on the injected `{reply, 42}` and answers its payload. (A host would
    // `spawn` the callee and `send` the reply; here the mock mailbox plays the
    // sender, and the *same* `receive` machinery the oracle runs is exercised.)
    let module = applyrecv_module();
    let callees = vec![("applyrecv".to_string(), Arc::clone(&module))];
    let mut ctx = TestCtx::new(callees, rejecting());

    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(
            &module,
            atom(&module, "run"),
            3,
            &[
                atom_term(&module, "applyrecv"),
                atom_term(&module, "blocker"),
                NIL,
            ],
            &mut ctx,
        )
        .expect("a suspend is not an error");

    let RunOutcome::Wait { func, pc } = outcome else {
        panic!("the applied blocker must suspend, got {outcome:?}");
    };
    assert_eq!(func, 1, "`blocker/0` is the fixture's second function");

    // The reply the oracle's `P ! {reply, 42}` delivers.
    let reply = terms::make_tuple(
        &mut ctx,
        &[atom_term(&module, "reply"), make_small(42)],
    )
    .expect("build {reply,42}");
    ctx.push(reply);

    let resumed = machine
        .resume_outcome(&module, func, pc, &mut ctx)
        .expect("the resume completes");
    assert_eq!(
        resumed,
        RunOutcome::Returned(make_small(42)),
        "the engine reproduces the oracle's 42"
    );

    // The `{reply,42}` was consumed by `remove_message`.
    assert!(ctx.messages.is_empty(), "the matched message is unlinked");
}

/// Re-runs the fixture's blocking scenario under the live system `erl` and returns
/// its printed value.
///
/// `None` means no runnable `erl` at all (the caller **skips loudly**). A spawned
/// `erl` that exits non-zero is a **harness** failure, not "no oracle" — it panics
/// with the captured streams so a broken oracle can never masquerade as a skip.
///
/// The fixture directory is **appended** (`-pz`, never `-pa`): it holds stand-in
/// modules (e.g. `tests/fixtures/lists.beam`) that would shadow the real stdlib once
/// prepended.
fn oracle_applyrecv() -> Option<String> {
    let eval = concat!(
        "Self = self(),",
        "P = spawn(fun() -> Self ! {done, applyrecv:run(applyrecv, blocker, [])} end),",
        "P ! {reply, 42},",
        "R = receive {done, V} -> V after 2000 -> timeout end,",
        "io:format(\"~w\", [R]), halt()."
    );
    let output = Command::new("erl")
        .args(["-noshell", "-pz", FIXTURES_DIR, "-eval", eval])
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for `{eval}`\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
fn the_live_oracle_reproduces_the_baked_value() {
    let Some(oracle) = oracle_applyrecv() else {
        eprintln!(
            "SKIP call_ext_apply::the_live_oracle_reproduces_the_baked_value: \
             no `erl` oracle on PATH"
        );
        return;
    };
    // The value the fixture test bakes in, re-derived from the oracle itself.
    assert_eq!(oracle, "42", "the oracle's applyrecv transcript changed");
}

// ---------------------------------------------------------------------------
// `resolve_external_call`'s Ok(None) default keeps the synchronous BIF path
// ---------------------------------------------------------------------------

#[test]
fn a_minimal_context_without_the_seam_folds_the_apply_into_call_bif() {
    // A plain `MinimalContext` keeps `ExecutionContext::resolve_external_call`'s
    // `Ok(None)` default (it owns no module registry), so the applied callee cannot
    // be entered **in this machine**: `funs::apply_target` folds the call into the
    // host's *synchronous* `call_bif` for the triple the seam decoded — `c:recv/0`,
    // NOT the `erlang:apply/3` import. This floor rejects every BIF, so the honest
    // outcome is that typed raise: a synchronous host has no way to let the callee
    // suspend the caller (the gap `TestCtx`'s resolver above closes).
    let (caller, run) = caller_apply3_module();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|module, function, arity, _args| {
        Err(beam_engine::unsupported(&format!(
            "bif {module}:{function}/{arity}"
        )))
    }));
    let mut machine = Machine::new();

    let error = machine
        .run_export_outcome(
            &caller,
            run,
            3,
            &[atom_term(&caller, "c"), atom_term(&caller, "recv"), NIL],
            &mut ctx,
        )
        .expect_err("without a resolver the applied callee reaches the host as a BIF dispatch");
    // The reason names the callee the seam decoded: proof the fold went through
    // `call_bif` with the decoded `(M, F, A)`.
    assert_eq!(reason(error), "unsupported_opcode: bif c:recv/0");
}

// ---------------------------------------------------------------------------
// (6) a host BIF error from an applied target is the catchable raise the oracle
//     produces — C's `$HANDLE_APPLY_ERROR` → `handle_error` → `next_catch`
// ---------------------------------------------------------------------------

/// A BIF floor that raises `error:boom` for the applied `erlang:error/1` (the
/// *host BIF error* an applied target produces) and rejects everything else.
///
/// `boom` is an atom the caller fixtures intern, and the raise is class `error`
/// with a bare-atom reason — the shape `beam_bifs`'s `erlang:error/1` raises and
/// the shape `Machine::raise_bif_error` lands in a live catch frame
/// (`x(1) = boom`, `x(3) = error`).
fn boom_raising() -> BifHook {
    Box::new(|module, function, arity, _args| {
        if module == "erlang" && function == "error" && arity == 1 {
            return Err(BeamError::Raise(Raise::error(ExitReason::atom("boom"))));
        }
        Err(beam_engine::unsupported(&format!(
            "bif {module}:{function}/{arity}"
        )))
    })
}

/// The `Instr::Apply` caller `m:run/0` — `run() -> try apply(erlang, error,
/// [boom]) catch C:R -> R end`:
///
/// ```text
/// m:run/0:
///     allocate 1
///     try y0 {handler}
///     move {atom,erlang} -> x1
///     move {atom,error}  -> x2
///     move {atom,boom}   -> x0      ; the single applied argument
///     apply 1
///     try_end y0
///     deallocate 1
///     return
///   {handler}:
///     try_case y0
///     move x1 -> x0                 ; the reason the raise landed
///     deallocate 1
///     return
/// ```
fn try_apply_module() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let erlang = f.atom("erlang");
    let error = f.atom("error");
    let boom = f.atom("boom");
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(103),
            },
            Instr::Move {
                src: Operand::Atom(erlang),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Atom(error),
                dst: Operand::X(2),
            },
            Instr::Move {
                src: Operand::Atom(boom),
                dst: Operand::X(0),
            },
            Instr::Apply { arity: 1 },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(103)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// [`try_apply_module`] without the `try`: the same `apply(erlang, error,
/// [boom])`, whose raise has **no** live catch frame to land in.
fn bare_apply_module() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let erlang = f.atom("erlang");
    let error = f.atom("error");
    let boom = f.atom("boom");
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Move {
                src: Operand::Atom(erlang),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Atom(error),
                dst: Operand::X(2),
            },
            Instr::Move {
                src: Operand::Atom(boom),
                dst: Operand::X(0),
            },
            Instr::Apply { arity: 1 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The enclosing-frame **tail** twin: `m:outer/0 -> try inner/0 catch C:R -> R
/// end`, where `m:inner/0` is `apply_last 1 0` (the `i_apply_last` shape a tail
/// `run() -> apply(erlang, error, [boom])` compiles to). The live `try` is in the
/// **enclosing** frame, so `inner`'s `apply_last` pops only its own frame and the
/// handler-owning frame survives the raise's landing.
fn enclosing_try_apply_last_module() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let erlang = f.atom("erlang");
    let error = f.atom("error");
    let boom = f.atom("boom");
    // `inner/0` — the tail `apply_last`.
    let inner = f.add_function(
        "inner",
        0,
        vec![
            Instr::Label(Label(200)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Move {
                src: Operand::Atom(erlang),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Atom(error),
                dst: Operand::X(2),
            },
            Instr::Move {
                src: Operand::Atom(boom),
                dst: Operand::X(0),
            },
            Instr::ApplyLast {
                arity: 1,
                deallocate: 0,
            },
        ],
    );
    // `outer/0` — the live `try` in the enclosing frame.
    let outer = f.add_function(
        "outer",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(103),
            },
            Instr::Call {
                arity: 0,
                target: inner,
            },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(103)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
        ],
    );
    f.add_export("outer", 0, outer);
    let outer_atom = f.atom("outer");
    (f.finish(), outer_atom)
}

#[test]
fn a_host_bif_error_from_the_call_ext_apply_seam_is_caught() {
    // Oracle (OTP 29.1.1 / erts-17.1):
    //   `try erlang:apply(erlang, error, [boom]) catch C:R -> {C,R} end`
    //     ⇒ {error,boom}
    // (the `apply(erlang, error, [boom])` sugar compiles to the very same
    //  `call_ext 3 {extfunc,erlang,apply,3}` the fixture below builds). The seam
    // decodes M/F/A and the host **declines** the target as a module
    // (`resolve_external_call` → `Ok(None)`), so `apply_target` folds it into
    // `call_bif`; its `error:boom` raise must land in the live `try`, exactly as
    // C's `$HANDLE_APPLY_ERROR` → `handle_error` → `next_catch` does.
    let (caller, run) = try_caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), boom_raising());
    let boom = terms::make_list(&mut ctx, &[atom_term(&caller, "boom")]).expect("build [boom]");
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(
            &caller,
            run,
            3,
            &[
                atom_term(&caller, "erlang"),
                atom_term(&caller, "error"),
                boom,
            ],
            &mut ctx,
        )
        .expect("the applied floor BIF's error must land in the live try frame");

    // The handler returns `x(1)` = the reason the raise landed.
    assert_eq!(outcome, RunOutcome::Returned(atom_term(&caller, "boom")));
    assert_eq!(
        machine.catch_frame_depth(),
        0,
        "the handler opcode consumed the frame the raise landed in"
    );
}

#[test]
fn a_host_bif_error_from_the_apply_instruction_is_caught() {
    // Oracle: `try apply(erlang, error, [boom]) catch C:R -> R end` ⇒ boom.
    // The `Instr::Apply` arm consumes the same `ApplyTarget::Raised` the
    // `call_ext erlang:apply/3` seam route produces, so this pins the second
    // route independently.
    let (caller, run) = try_apply_module();
    let mut ctx = TestCtx::new(callees(), boom_raising());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the applied floor BIF's error must land in the live try frame");

    assert_eq!(outcome, RunOutcome::Returned(atom_term(&caller, "boom")));
}

#[test]
fn a_host_bif_error_from_a_tail_apply_last_lands_in_the_enclosing_try() {
    // The `Instr::ApplyLast` arm pops its own frame **before** the target is
    // applied; C's `i_apply_last` also runs `$deallocate` before dispatching a
    // resolved callee (`emu/instrs.tab:266-272`). The live `try` here is in the
    // **enclosing** frame, which the pop does not touch, so the handler-owning
    // frame survives the raise's landing and the tail apply reports the oracle's
    // value.
    let (caller, run) = enclosing_try_apply_last_module();
    let mut ctx = TestCtx::new(callees(), boom_raising());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the tail apply's error must land in the enclosing try frame");

    assert_eq!(outcome, RunOutcome::Returned(atom_term(&caller, "boom")));
    assert_eq!(
        machine.frame_depth(),
        1,
        "both the tail apply's own frame and the handler's frame are gone"
    );
}

#[test]
fn a_host_bif_error_from_apply_without_a_catch_frame_propagates_the_typed_raise() {
    // With **no** live `try` the raise cannot land: `raise_bif_error` returns the
    // error unchanged, so the applied BIF's own class/reason propagate — never a
    // fabricated `badarg`, never a panic.
    let (caller, run) = bare_apply_module();
    let mut ctx = TestCtx::new(callees(), boom_raising());
    let mut machine = Machine::new();

    let error = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect_err("an uncaught applied-BIF raise cannot land");

    assert_eq!(reason(error), "boom");
    assert_eq!(machine.catch_frame_depth(), 0);
}

#[test]
fn a_capability_gap_from_an_applied_target_stays_uncatchable() {
    // The `rejecting()` floor answers with the engine's typed `unsupported`
    // diagnostic — a reason atom no module can name. Even inside a live `try` it
    // must stay the uncatchable typed error: the funnel refuses to substitute a
    // *different, wrong* atom, so the handler is never entered.
    let (caller, run) = try_caller_apply3_module();
    let mut ctx = TestCtx::new(callees(), rejecting());
    let boom = terms::make_list(&mut ctx, &[atom_term(&caller, "boom")]).expect("build [boom]");
    let mut machine = Machine::new();

    let err = machine
        .run_export_outcome(
            &caller,
            run,
            3,
            &[
                atom_term(&caller, "erlang"),
                atom_term(&caller, "error"),
                boom,
            ],
            &mut ctx,
        )
        .expect_err("a capability gap is not catchable");

    assert!(
        matches!(err, BeamError::Raise(_)),
        "the gap keeps its typed raise, got {err:?}"
    );
    assert_eq!(
        machine.catch_frame_depth(),
        1,
        "the handler must not have been entered"
    );
}
