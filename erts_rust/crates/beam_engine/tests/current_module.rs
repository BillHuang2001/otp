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

//! **Module identity** across a cross-module `call_ext`: [`Machine::current_module`]
//! and the module-context-aware position accessors.
//!
//! # Why module identity matters
//!
//! The position accessors ([`Machine::current_function`], [`Machine::current_location`],
//! [`Machine::current_stacktrace`]) are **module-relative**: a `(func, pc)` is an index
//! into a *specific* module's tables, so naming it in the wrong module invents an
//! `M`/`F`/`A`. When a host resolves a `call_ext*` through
//! [`ExecutionContext::resolve_external_call`] the callee runs **in the same machine**
//! (see `tests/external_call.rs`), so the machine's current address belongs to the
//! *callee's* module, not the entry module the caller passed.
//!
//! # The chain under test
//!
//! `m:run/0` allocates its own frame, `call_ext`s `c:recv/0`, and the callee suspends
//! on an empty mailbox. At the suspension:
//!
//! * [`Machine::current_module`] is the callee's `Arc<Module>` (the very handle the
//!   host resolved the call with);
//! * [`Machine::current_function`]/[`Machine::current_location`] name the **callee**;
//! * [`Machine::initial_call`] still names the **entry** module (`m:run/0` — the
//!   `$initial_call`);
//! * [`Machine::current_stacktrace`] resolves **each** frame in its own module:
//!   `[{c,recv,0,[]},{m,run,0,[]}]` — the outer frame is the caller's, *not* a
//!   fabricated `{c,noop,0,[]}` (which is what resolving a caller-module `(func, pc)`
//!   against the callee module would produce).
//!
//! The synthetic modules are built by hand (each with its own atom table), reusing the
//! scaffolding shapes of `tests/external_call.rs`; the resolver passes plain strings,
//! so no shared [`AtomTable`](beam_loader::Module::atom_table) is needed.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use beam_common::Result;
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext, RunOutcome};
use beam_loader::{Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand};
use beam_term::{eq, make_atom, make_small, Atom, Eterm, NIL};

// ---------------------------------------------------------------------------
// Minimal synthetic-module scaffolding (see tests/external_call.rs)
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
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Register an import and return its reference.
    fn import(&mut self, module: &str, function: &str, arity: u32) -> ImportRef {
        let module = self.atom(module);
        let function = self.atom(function);
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

/// The caller module `m`: `run/0` allocates its own frame, `call_ext`s the named
/// callee export at arity 0, then `deallocate`/`return`s.
fn caller_module(callee_function: &str) -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let target = f.import("c", callee_function, 0);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::CallExt { target, arity: 0 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The callee module `c`: `noop/0` (function index `0`), `recv/0` (index `1`, a bare
/// `receive` that suspends on an empty mailbox) and `recv_twice/0` (index `2`, two
/// sequential receives, so a resume with a single message re-suspends in the same
/// callee).
fn callee_module() -> Arc<Module> {
    let mut f = Fixture::new("c");

    let noop = f.add_function("noop", 0, vec![Instr::Label(Label(1)), Instr::Return]);
    f.add_export("noop", 0, noop);

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

    let recv_twice = f.add_function(
        "recv_twice",
        0,
        vec![
            Instr::Label(Label(40)),
            Instr::LoopRec {
                fail: Label(41),
                dst: Operand::X(0),
            },
            Instr::RemoveMessage,
            Instr::Label(Label(42)),
            Instr::LoopRec {
                fail: Label(43),
                dst: Operand::X(1),
            },
            Instr::RemoveMessage,
            Instr::Return,
            Instr::Label(Label(41)),
            Instr::Wait { target: Label(40) },
            Instr::Label(Label(43)),
            Instr::Wait { target: Label(42) },
        ],
    );
    f.add_export("recv_twice", 0, recv_twice);

    Arc::new(f.finish())
}

// ---------------------------------------------------------------------------
// A custom ExecutionContext implementing the cross-module seam
// ---------------------------------------------------------------------------

type BifFloor = Box<dyn FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm>>;

/// A hand-written context that serves the cross-module seam plus a tiny mailbox.
///
/// * `callees` — the modules the seam resolves, keyed by name. A `call_ext*` to a
///   name in this list returns `Some((Arc<Module>, export_label))`; anything else
///   returns `Ok(None)` (the synchronous path).
/// * `messages`/`scan` — a faithful mailbox: `receive_next` peeks at the scan
///   position, `receive_skip` advances it, `receive_remove` unlinks the current
///   message and `receive_reset` repositions at the first.
/// * `bif` — a pluggable BIF floor (the synchronous `call_ext*` target when the
///   seam declines).
/// * `resolved` — every `(module, function, arity)` the seam **answered `Some`**
///   for. It is the observable "the callee really ran in this machine" signal: a
///   seam that silently declined would leave it empty, and the tests that depend
///   on a live callee module assert it is exactly the call they made.
struct SeamCtx {
    callees: Vec<(String, Arc<Module>)>,
    messages: Vec<Eterm>,
    scan: usize,
    bif: BifFloor,
    /// The `(module, function, arity)` triples the seam resolved, in call order.
    resolved: Vec<(String, String, u32)>,
    /// The term arena the position accessors build `{M,F,A}`/stacktrace terms in —
    /// the same pointer-stable chunked heap `MinimalContext` serves, delegated so a
    /// seam context can hand the term builders words (the trait default is a typed
    /// `unsupported`).
    arena: MinimalContext,
}

impl SeamCtx {
    fn new(callees: Vec<(String, Arc<Module>)>, bif: BifFloor) -> SeamCtx {
        SeamCtx {
            callees,
            messages: Vec::new(),
            scan: 0,
            bif,
            resolved: Vec::new(),
            arena: MinimalContext::new(),
        }
    }
}

impl ExecutionContext for SeamCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        (self.bif)(module, function, arity, args)
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
        self.arena.alloc_words(n)
    }
    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        // Find the answer first (the `&self.callees` borrow cannot coexist with a
        // `self.resolved` push), then record it.
        let mut answer = None;
        for (name, callee) in &self.callees {
            if name != module {
                continue;
            }
            let Some(symbol) = callee.atom_table.lookup(function) else {
                continue;
            };
            if let Some(label) = callee.export(symbol, arity) {
                answer = Some((Arc::clone(callee), label));
                break;
            }
        }
        match answer {
            Some(resolved) => {
                self.resolved
                    .push((module.to_string(), function.to_string(), arity));
                Ok(Some(resolved))
            }
            None => Ok(None),
        }
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

/// A BIF floor that rejects everything (the synchronous `call_ext*` path is never
/// taken when the seam resolves the call).
fn rejecting() -> BifFloor {
    Box::new(|module, function, _arity, _args| {
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a name interned in `module`'s atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// Asserts the first three elements of `got` are the `{Module, Function, Arity}`
/// tuple `{module_name, function, arity}`, resolved through `owner`'s atom table.
fn assert_mfa_fields(owner: &Module, got: Eterm, module_name: &str, function: &str, arity: u32) {
    let expected = [
        make_atom(atom(owner, module_name)),
        make_atom(atom(owner, function)),
        make_small(arity as i64),
    ];
    for (index, want) in expected.iter().enumerate() {
        let element = terms::tuple_element(got, index).expect("an MFA element");
        assert!(
            eq(element, *want, &owner.atom_table),
            "MFA element {index}: got {element:?}, want {want:?}"
        );
    }
}

/// Asserts `got` is the `{M,F,A}` tuple `{module_name, function, arity}`.
fn assert_mfa(owner: &Module, got: Eterm, module_name: &str, function: &str, arity: u32) {
    assert_eq!(
        terms::tuple_arity(got).expect("an MFA is a tuple"),
        3,
        "an MFA is a 3-tuple"
    );
    assert_mfa_fields(owner, got, module_name, function, arity);
}

/// Asserts `got` is the `{M,F,A,Location}` stacktrace item for
/// `{module_name, function, arity}` with an empty (`[]`) location — the shape the
/// synthetic modules produce (they carry no `Line` chunk).
fn assert_stacktrace_item(owner: &Module, got: Eterm, module_name: &str, function: &str, arity: u32) {
    assert_eq!(
        terms::tuple_arity(got).expect("a stacktrace item is a tuple"),
        4,
        "a stacktrace item is a {{M,F,A,Location}} 4-tuple"
    );
    assert_mfa_fields(owner, got, module_name, function, arity);
    let location = terms::tuple_element(got, 3).expect("the Location element");
    assert!(
        eq(location, NIL, &owner.atom_table),
        "a synthetic module has no Line chunk, so Location is [], got {location:?}"
    );
}

/// Suspend `m:run/0` inside `c:recv/0` on an empty mailbox, returning the callee
/// handle, the entry module + its `run` atom, and the paused machine + context.
fn suspended_in_callee(
    callee_function: &str,
) -> (Arc<Module>, Module, Atom, Machine, SeamCtx) {
    let callee = callee_module();
    let (caller, run) = caller_module(callee_function);
    let mut ctx = SeamCtx::new(vec![("c".to_string(), Arc::clone(&callee))], rejecting());
    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("a suspend is not an error");
    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "the callee's receive must suspend on an empty mailbox, got {outcome:?}"
    );
    // The suspension must be a *cross-module* one: the seam has to have answered
    // `Some` for the exact call `m:run/0` makes. Otherwise the `Wait` above would
    // be indistinguishable from a run that never left the entry module.
    assert_eq!(
        ctx.resolved,
        vec![("c".to_string(), callee_function.to_string(), 0)],
        "the seam must have resolved the call, not declined it"
    );
    (callee, caller, run, machine, ctx)
}

// ---------------------------------------------------------------------------
// (a) the machine names the callee module while suspended inside it
// ---------------------------------------------------------------------------

#[test]
fn a_suspended_cross_module_run_names_the_callee_module() {
    let (callee, _caller, _run, machine, _ctx) = suspended_in_callee("recv");
    // The suspension is inside the callee (`c:recv/0` is function index 1; its
    // `wait` resumes at `Label(10)`, its first instruction).
    assert_eq!(machine.current_code().map(|(f, _)| f), Some(1));
    let current = machine
        .current_module()
        .expect("the machine is executing inside the callee");
    assert!(
        Arc::ptr_eq(&current, &callee),
        "current_module must be the very Arc the host resolved the call with"
    );
}

#[test]
fn current_function_names_the_callee_not_the_caller() {
    let (callee, caller, _run, machine, mut ctx) = suspended_in_callee("recv");
    let value = machine
        .current_function(&caller, &mut ctx)
        .expect("current_function")
        .expect("a suspended machine has a current function");
    // Resolved in the CALLEE module `c`, not the entry module `m`.
    assert_mfa(&callee, value, "c", "recv", 0);
}

#[test]
fn initial_call_stays_the_entry_module() {
    let (_callee, caller, _run, machine, mut ctx) = suspended_in_callee("recv");
    let value = machine
        .initial_call(&caller, &mut ctx)
        .expect("initial_call")
        .expect("a machine that has run has an entry call");
    // `$initial_call` is the run's ENTRY call, which belongs to `m` regardless of
    // where the machine currently is.
    assert_mfa(&caller, value, "m", "run", 0);
}

#[test]
fn current_location_names_the_callee() {
    let (callee, caller, _run, machine, mut ctx) = suspended_in_callee("recv");
    let value = machine
        .current_location(&caller, &mut ctx)
        .expect("current_location")
        .expect("a suspended machine has a current location");
    assert_stacktrace_item(&callee, value, "c", "recv", 0);
}

#[test]
fn current_stacktrace_resolves_each_frame_in_its_own_module() {
    let (callee, caller, _run, machine, mut ctx) = suspended_in_callee("recv");
    let stacktrace = machine
        .current_stacktrace(&caller, &mut ctx)
        .expect("current_stacktrace");
    let frames = terms::list_elements(stacktrace).expect("a stacktrace is a proper list");
    assert_eq!(
        frames.len(),
        2,
        "the suspended callee frame plus its caller frame, got {frames:?}"
    );
    // Innermost = the callee (its own module context).
    assert_stacktrace_item(&callee, frames[0], "c", "recv", 0);
    // Outer = the CALLER, resolved in `m`: `(func 0, pc 3)` is `m:run/0`'s resume
    // point (`{m,run,0}`), NOT a fabricated `{c,noop,0}` (resolving it against the
    // callee would name the callee's `noop/0`).
    assert_stacktrace_item(&caller, frames[1], "m", "run", 0);
}

// ---------------------------------------------------------------------------
// (b) the module identity survives a suspend/resume cycle
// ---------------------------------------------------------------------------

#[test]
fn the_callee_module_survives_a_resume() {
    // `m:run/0 -> c:recv_twice/0`; the callee needs TWO messages, so a resume with a
    // single message re-suspends in the SAME callee — proving `resume_outcome` does
    // not reset the module-context registry, and the position accessors still name
    // the callee after the resume.
    let callee = callee_module();
    let (caller, run) = caller_module("recv_twice");
    let mut ctx = SeamCtx::new(vec![("c".to_string(), Arc::clone(&callee))], rejecting());
    let mut machine = Machine::new();

    let first = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("a suspend is not an error");
    assert_eq!(first, RunOutcome::Wait { func: 2, pc: 0 });

    ctx.messages.push(make_small(1));
    let second = machine
        .resume_outcome(&caller, 2, 0, &mut ctx)
        .expect("the resume suspends again, which is not an error");
    assert_eq!(second, RunOutcome::Wait { func: 2, pc: 3 });

    let current = machine
        .current_module()
        .expect("the machine is still inside the callee");
    assert!(Arc::ptr_eq(&current, &callee));
    let value = machine
        .current_function(&caller, &mut ctx)
        .expect("current_function")
        .expect("a suspended machine has a current function");
    assert_mfa(&callee, value, "c", "recv_twice", 0);
}

// ---------------------------------------------------------------------------
// (c) a run that completes in the entry module reports no callee
// ---------------------------------------------------------------------------

#[test]
fn a_run_that_stays_in_the_entry_module_reports_none() {
    // A synchronous `call_ext*` (no seam) runs the callee elsewhere, so the machine
    // never enters a module context: `current_module()` is `None` (the passed entry
    // module is the right one).
    let (caller, run) = caller_module("recv");
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|module, function, arity, _args| {
        if module == "c" && function == "recv" && arity == 0 {
            return Ok(make_small(5));
        }
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    }));
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the synchronous call succeeds");
    assert_eq!(outcome, RunOutcome::Returned(make_small(5)));
    assert!(
        machine.current_module().is_none(),
        "a run that never entered a cross-module callee has no callee module"
    );
}

// ---------------------------------------------------------------------------
// (d) a callee the seam DECLINES falls through to the synchronous BIF floor
// ---------------------------------------------------------------------------

#[test]
fn a_declined_callee_falls_through_to_the_synchronous_bif_floor() {
    // The seam answers by NAME, looked up in the *callee's own* atom table; a
    // function the callee module does not even intern must make it answer
    // `Ok(None)` -- "I cannot run this callee in your machine" -- so the
    // `call_ext*` takes the synchronous `call_bif` path. This pins the decline
    // branch independently: a resolver that declined *everything* would still
    // satisfy the accessor tests above only through their `rejecting()` floor,
    // and a resolver that resolved by name alone (ignoring the callee's atom
    // table) would enter a module context that cannot name the target.
    let callee = callee_module();
    let (caller, run) = caller_module("missing");
    assert!(
        callee.atom_table.lookup("missing").is_none(),
        "the fixture must not intern `missing`, or the seam would resolve it"
    );

    let seen = Rc::new(RefCell::new(Vec::<(String, String, u32)>::new()));
    let recorder = Rc::clone(&seen);
    let bif: BifFloor = Box::new(move |module, function, arity, args| {
        recorder
            .borrow_mut()
            .push((module.to_string(), function.to_string(), arity));
        assert!(
            args.is_empty(),
            "the declined `missing/0` call carries no arguments, got {args:?}"
        );
        Ok(make_small(42))
    });
    let mut ctx = SeamCtx::new(vec![("c".to_string(), Arc::clone(&callee))], bif);
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the synchronous call succeeds");
    assert_eq!(outcome, RunOutcome::Returned(make_small(42)));
    assert_eq!(
        seen.borrow().as_slice(),
        [("c".to_string(), "missing".to_string(), 0)],
        "the declined callee must reach the synchronous BIF floor exactly once"
    );
    assert!(
        ctx.resolved.is_empty(),
        "the seam must not claim to have resolved a name the callee does not intern"
    );
    assert!(
        machine.current_module().is_none(),
        "a synchronous call runs elsewhere: the machine enters no module context"
    );
}
