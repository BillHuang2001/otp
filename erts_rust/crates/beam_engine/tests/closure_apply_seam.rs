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

//! In-machine closure-apply seam (`ExecutionContext::resolve_local_fun`) tests: a
//! `call_fun`/`call_fun2` whose closure the host resolves is executed **in the
//! calling machine**, so a `receive` inside the closure suspends the *calling*
//! process ([`RunOutcome::Wait`]) and a raise inside it lands in the caller's live
//! `try`/`catch`.
//!
//! # What the seam changes
//!
//! Without the seam a `call_fun` is a **value-returning** `apply_fun` call: the host
//! runs the closure elsewhere (or emulates it) and hands back a term. A closure whose
//! body would block cannot suspend its caller, and a raise inside it cannot land in
//! the caller's live `try`/`catch` — the `Err` surfaces at the call site instead.
//! With the seam the host answers `Some((callee_module, lambda_entry_label))` and the
//! engine enters the lambda body **in this machine**: it copies the closure's free
//! variables into `x(arity..)` (C's `reg[i + arity] = funp->env[i]` — the call
//! arguments first, the captures after), stores the caller's return address in the
//! live frame and reports `Step::Enter`, exactly as C's `call_fun`
//! (`../../erts/emulator/beam/beam_common.c`) dispatches to the fun body's code in the
//! same process.
//!
//! # The three defects these tests pin
//!
//! 1. **The capture/argument ordering.** A closure's captures must land **after** the
//!    call arguments, byte-identical to what the compiled code expects: the oracle's
//!    lambda for `f(N) -> call1(fun(A) -> receive X -> {N,A,X} end end)` compiles to
//!    `'-f/1-fun-0-'/2`, whose body is `allocate 2,2; {move,{x,0},{y,0}};
//!    {move,{x,1},{y,1}}; …; {put_tuple2,{x,0},{list,[{y,1},{y,0},{x,0}]}}` — i.e.
//!    **x(0) is the call argument and x(1) the captured `N`**. Every closure below
//!    captures a free variable whose value differs from the argument's, so a swapped
//!    ordering shows up as a wrong tuple, never as a pass.
//! 2. **The block.** A `receive` inside the seam-reached closure must suspend the
//!    *calling* process and resume through `Machine::resume_outcome`.
//! 3. **The closure-delivered raise.** A raise inside the applied closure must land in
//!    the caller's live `try`/`catch` — on the in-machine route **and** on the
//!    value-returning `apply_fun` fallback — while a genuine capability gap
//!    (`beam_engine::unsupported`) stays the **uncatchable** typed error.
//!
//! # The oracle
//!
//! Every expected value below is derived from this source, compiled with the system
//! `erlc` (OTP 29.1.1 / erts-17.1) and run under `erl`:
//!
//! ```erlang
//! -module(sap3).
//! -export([call1/1, call11/1, f/1, f11/1, raise_in_fun/2]).
//!
//! call1(F)  -> F(10).
//! call11(F) -> F(11).
//!
//! f(N)   -> call1(fun(A) -> receive X -> {N, A, X} end end).
//! f11(N) -> call11(fun(A) -> receive X -> {N, A, X} end end).
//!
//! raise_in_fun(_N, F) -> try F(10) catch C:R -> {caught, C, R} end.
//! ```
//!
//! Observed (the values baked into the assertions):
//!
//! ```text
//! sap3:f(7)   with 99 in the mailbox                     -> {7,10,99}
//! sap3:f11(7) with 99 in the mailbox                     -> {7,11,99}
//! sap3:raise_in_fun(7, fun(A) -> erlang:error(boom) end) -> {caught,error,boom}
//! try apply(1, f, []) catch C:R -> {C,R} end             -> {error,badarg}
//! ```
//!
//! (The `apply/3` `badarg` is the shape the `apply_fun` **fallback** models: a
//! closure-application failure whose reason is the routable `badarg` atom.)
//!
//! # Hand-built modules
//!
//! The seam is handed the closure **term** and the call arguments, so no shared
//! [`AtomTable`](beam_loader::Module::atom_table) is needed: each module carries its
//! own table and the host answers with the callee it has. Every fixture seeds the same
//! names in the same order, so a landed atom term compares across modules (the
//! `cross_module_catch.rs` convention).

use std::collections::BTreeMap;
use std::sync::Arc;

use beam_common::{BeamError, ExceptionClass, ExitReason, Raise, Result};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext, RunOutcome};
use beam_loader::{
    Export, FunDescriptor, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand,
};
use beam_term::{cmp, make_atom, make_small, signed_value, Atom, Eterm};

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

/// The names every fixture seeds, in this order (see the module docs).
const SEED_ATOMS: [&str; 7] = [
    "error",
    "exit",
    "throw",
    "EXIT",
    "function_clause",
    "boom",
    "badarg",
];

/// A builder for one synthetic module with its own atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        for atom in SEED_ATOMS {
            module.atom_table.intern(atom);
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

    /// The index the **next** appended function will have.
    fn next_function_index(&self) -> u32 {
        self.module.functions.len() as u32
    }

    /// Register a `FunT` descriptor; its position is the fun-table index.
    fn add_fun(&mut self, name: &str, arity: u32, num_free: u32, uniq: u32) -> u32 {
        let function = self.atom(name);
        let index = self.module.funs.len() as u32;
        self.module.funs.push(FunDescriptor {
            function,
            arity,
            label: Label(index + 1),
            index,
            num_free,
            uniq,
        });
        index
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

// ---------------------------------------------------------------------------
// The callers (synthetic mirrors of the oracle's `call1/1` + `f/1`)
// ---------------------------------------------------------------------------

/// The `FunT` identity every caller's closure carries (the compiled `make_fun3`'s
/// `uniq`/`index` are opaque to the seam — the host answers from its own registry).
const LAMBDA_UNIQ: u32 = 0xCAFE;

/// `m:run/0` — the oracle's `f/1` with its `call1/1` inlined:
///
/// ```text
/// run/0:
///     allocate 0 2
///     move 7 -> x0                    ; the capture source (the oracle's `N`)
///     make_fun3 <funT> x1 x0          ; F = fun(A) -> … end  (num_free = 1)
///     move 10 -> x0                   ; the call argument (the oracle's `A`)
///     call_fun 1                      ; F(10)
///     deallocate 0
///     return
/// ```
fn caller_building_a_closure() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let index = f.add_fun("lambda", 2, 1, LAMBDA_UNIQ);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 2,
            },
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(0),
            },
            Instr::MakeFun3 {
                index,
                dst: Operand::X(1),
                env: vec![Operand::X(0)],
            },
            Instr::Move {
                src: Operand::Integer(10),
                dst: Operand::X(0),
            },
            Instr::CallFun { arity: 1 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The `call_fun2` twin of [`caller_building_a_closure`]: the same closure, but held
/// in a **register operand** (`x(2)`) rather than `x(arity)`, and called with the
/// oracle's `call11/1` argument (`F(11)`) so the expected tuple is `{7,11,99}`.
fn caller_building_a_closure_via_call_fun2() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let index = f.add_fun("lambda", 2, 1, LAMBDA_UNIQ);
    let unsafe_atom = f.atom("unsafe");
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 3,
            },
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(0),
            },
            Instr::MakeFun3 {
                index,
                dst: Operand::X(2),
                env: vec![Operand::X(0)],
            },
            Instr::Move {
                src: Operand::Integer(11),
                dst: Operand::X(0),
            },
            Instr::CallFun2 {
                tag: Operand::Atom(unsafe_atom),
                arity: 1,
                func: Operand::X(2),
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// `m:run/0` with a live `try` around the closure call — the oracle's
/// `raise_in_fun/2`:
///
/// ```erlang
/// run() -> try (fun(A) -> … end)(10) catch C:R -> R end.
/// ```
///
/// The handler returns the **reason** (`x(1)` after `try_case`), so the test asserts
/// the landed exception's reason term.
fn caller_guarding_a_closure() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let index = f.add_fun("lambda", 2, 1, LAMBDA_UNIQ);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 2,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(103),
            },
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(0),
            },
            Instr::MakeFun3 {
                index,
                dst: Operand::X(1),
                env: vec![Operand::X(0)],
            },
            Instr::Move {
                src: Operand::Integer(10),
                dst: Operand::X(0),
            },
            Instr::CallFun { arity: 1 },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            // The handler: `try_case` unpacks x(0) = class (from x(3)), x(1) = reason.
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

/// `m:run/1` — the compiled `call1/1` verbatim: the closure arrives in `x(0)`, is
/// moved to `x(1)` (where `call_fun 1` reads it), the argument goes to `x(0)`, and the
/// call is a plain (non-tail) `call_fun`.
fn caller_taking_a_closure() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let run = f.add_function(
        "run",
        1,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 1,
            },
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Integer(10),
                dst: Operand::X(0),
            },
            Instr::CallFun { arity: 1 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 1, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

// ---------------------------------------------------------------------------
// The callee `c` — the module the closure seam resolves a closure to
// ---------------------------------------------------------------------------

/// The callee module plus the two lambda entry labels the seam answers with.
struct Callee {
    module: Arc<Module>,
    /// `lambda_recv/2`'s entry label — the blocking lambda body.
    recv_lambda: Label,
    /// `lambda_raise/2`'s entry label — the raising lambda body.
    raise_lambda: Label,
    /// The function index of `lambda_recv` (what a `RunOutcome::Wait` reports).
    recv_func: u32,
}

/// `c` — the lambda bodies, each the compiled shape of the oracle's
/// `'-f/1-fun-0-'/2` / a raising `fun(A) -> erlang:error(boom) end`.
///
/// The seam places the call argument at `x(0)` and the captured `N` at `x(1)`; the
/// blocking lambda's body then does exactly what the oracle's compiled lambda does:
///
/// ```text
/// lambda_recv/2:
///     allocate 2 2
///     move x0 -> y0           ; A  (the call argument)
///     move x1 -> y1           ; N  (the captured free variable)
///   {21}:
///     loop_rec {22} x0        ; X  (the received message)
///     remove_message
///     put_tuple2 x0 {y1, y0, x0}   ; {N, A, X}
///     deallocate 2
///     return
///   {22}:
///     wait {21}
/// ```
fn callee_module() -> Callee {
    let mut f = Fixture::new("c");

    // Index 0: a trivial function, so the lambda bodies are not function 0.
    let noop = f.add_function("noop", 0, vec![Instr::Label(Label(1)), Instr::Return]);
    f.add_export("noop", 0, noop);

    // Index 1: the blocking lambda.
    let recv_func = f.next_function_index();
    let recv_lambda = f.add_function(
        "lambda_recv",
        2,
        vec![
            Instr::Label(Label(20)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(2),
                live: 2,
            },
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::Y(1),
            },
            Instr::Label(Label(21)),
            Instr::LoopRec {
                fail: Label(22),
                dst: Operand::X(0),
            },
            Instr::RemoveMessage,
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: vec![Operand::Y(1), Operand::Y(0), Operand::X(0)],
            },
            Instr::Deallocate(2),
            Instr::Return,
            Instr::Label(Label(22)),
            Instr::Wait { target: Label(21) },
        ],
    );

    // Index 2: the raising lambda (`erlang:error(boom)`).
    let error = f.import("erlang", "error", 1);
    let boom = f.atom("boom");
    let raise_lambda = f.add_function(
        "lambda_raise",
        2,
        vec![
            Instr::Label(Label(30)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 2,
            },
            Instr::Move {
                src: Operand::Atom(boom),
                dst: Operand::X(0),
            },
            Instr::CallExtLast {
                target: error,
                arity: 1,
                deallocate: 0,
            },
        ],
    );

    Callee {
        module: Arc::new(f.finish()),
        recv_lambda,
        raise_lambda,
        recv_func,
    }
}

// ---------------------------------------------------------------------------
// The host: the closure seam + a faithful mailbox + a pluggable BIF floor
// ---------------------------------------------------------------------------

type BifFloor = Box<dyn FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm>>;

/// What the closure seam does for a `call_fun`/`call_fun2`.
enum Seam {
    /// Answer with `(callee, lambda_label)` — the in-machine route.
    Resolve,
    /// Decline (`Ok(None)`) — the value-returning `apply_fun` fallback.
    Decline,
    /// Report a genuine capability gap (`Err(error::unsupported(..))`) — the shape
    /// that must stay **uncatchable**.
    Gap,
}

/// A hand-written context that serves the closure seam plus a faithful mailbox.
///
/// * `inner` — a [`MinimalContext`]: it owns the reserved-heap arena every term
///   built during the run (`make_fun3`'s closure, the `{N,A,X}` tuple) lives on, and
///   the optional **apply hook** the fallback route consults.
/// * `callee`/`lambda` — the module + lambda entry label the seam answers with.
/// * `seam` — resolve / decline / report a capability gap.
/// * `messages`/`scan` — a faithful mailbox: `receive_next` peeks at the scan
///   position, `receive_skip` advances it, `receive_remove` unlinks the current
///   message and repositions at the first.
struct SeamCtx {
    inner: MinimalContext,
    callee: Arc<Module>,
    lambda: Label,
    seam: Seam,
    messages: Vec<Eterm>,
    scan: usize,
}

impl SeamCtx {
    fn new(callee: Arc<Module>, lambda: Label, bif: BifFloor) -> SeamCtx {
        let mut inner = MinimalContext::new();
        inner.set_bif_hook(bif);
        SeamCtx {
            inner,
            callee,
            lambda,
            seam: Seam::Resolve,
            messages: Vec::new(),
            scan: 0,
        }
    }

    /// A context over an already-configured [`MinimalContext`] (its apply hook, BIF
    /// floor and heap arena are the caller's).
    fn with_inner(callee: Arc<Module>, lambda: Label, inner: MinimalContext) -> SeamCtx {
        SeamCtx {
            inner,
            callee,
            lambda,
            seam: Seam::Resolve,
            messages: Vec::new(),
            scan: 0,
        }
    }

    /// The seam declines every closure (the `Ok(None)` trait default).
    fn declining(mut self) -> SeamCtx {
        self.seam = Seam::Decline;
        self
    }

    /// The seam reports a capability gap for every closure.
    fn reporting_a_gap(mut self) -> SeamCtx {
        self.seam = Seam::Gap;
        self
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
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn heap_available(&self) -> usize {
        self.inner.heap_available()
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        // The arena `make_fun3` builds the closure on and `put_tuple2` builds the
        // result on: a reserved, pointer-stable region (the `MinimalContext` heap).
        self.inner.alloc_words(n)
    }

    fn apply(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        self.inner.apply(module, function, arity, args)
    }

    fn apply_fun(&mut self, fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        self.inner.apply_fun(fun, args)
    }

    fn resolve_local_fun(
        &mut self,
        _fun: Eterm,
        _args: &[Eterm],
    ) -> Result<Option<(Arc<Module>, Label)>> {
        // The engine reads the closure's free variables from the term itself, so the
        // seam only has to name the lambda body.
        match self.seam {
            Seam::Resolve => Ok(Some((Arc::clone(&self.callee), self.lambda))),
            Seam::Decline => Ok(None),
            Seam::Gap => Err(beam_engine::unsupported(
                "resolve_local_fun: no module for the closure",
            )),
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

/// A BIF floor that rejects everything (the synchronous `call_bif` path is never
/// taken when the seam resolves the call).
fn rejecting() -> BifFloor {
    Box::new(|module, function, _arity, _args| {
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    })
}

/// A BIF floor that models `erlang:error/1` by raising with the **bare atom** `boom`
/// (class `error`) — exactly what `beam_bifs` produces for an atom reason.
fn raising_error_bif() -> BifFloor {
    Box::new(|module, function, arity, args| {
        if module == "erlang" && function == "error" && arity == 1 && args.len() == 1 {
            return Err(BeamError::Raise(Raise::new(
                ExceptionClass::Error,
                ExitReason::atom("boom"),
            )));
        }
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    })
}

/// The caller-module atom term for `name` — the caller's atom table is the one a
/// landed reason is compared against.
fn caller_atom(module: &Module, name: &str) -> Eterm {
    make_atom(module.atom_table.lookup(name).expect("the caller names the atom"))
}

// ---------------------------------------------------------------------------
// (1) a blocking closure, WITH a capture
// ---------------------------------------------------------------------------

#[test]
fn a_blocking_closure_suspends_the_calling_process_and_reads_its_capture() {
    // Oracle: `sap3:f(7)` with `99` in the mailbox -> `{7,10,99}`, where the lambda is
    // `fun(A) -> receive X -> {N, A, X} end end` with the captured `N = 7`. The seam
    // must place the call argument at x(0) and the capture at x(1) — a swapped
    // ordering yields `{10,7,99}`, so the assertion is discriminating.
    let callee = callee_module();
    let (caller, run) = caller_building_a_closure();
    let mut ctx = SeamCtx::new(
        Arc::clone(&callee.module),
        callee.recv_lambda,
        rejecting(),
    );
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("a suspend is not an error");
    let RunOutcome::Wait { func, pc } = outcome else {
        panic!("the closure body must suspend the calling process, got {outcome:?}");
    };
    assert_eq!(
        func, callee.recv_func,
        "the suspension is in the callee module's lambda body, not at the call site"
    );
    // `pc` 4 is the lambda's `loop_rec` (the `wait`'s resume target): code index 0 is
    // `Label(20)`, then `allocate`/`move`/`move`, then `Label(21)`.
    assert_eq!(pc, 4, "the process parked at the closure body's resume label");
    assert!(
        machine.current_module().is_some(),
        "a resume must stay inside the registered callee context"
    );

    // A message arrives; resume the *same* machine (the registers and the closure
    // frame survive).
    ctx.messages.push(make_small(99));
    let resumed = machine
        .resume_outcome(&caller, func, pc, &mut ctx)
        .expect("the resume completes");
    let expected = terms::make_tuple(&mut ctx, &[make_small(7), make_small(10), make_small(99)])
        .expect("the oracle tuple builds on the arena");
    match resumed {
        RunOutcome::Returned(value) => assert!(
            cmp(value, expected, &caller.atom_table).is_eq(),
            "the closure body must return the oracle's {{7,10,99}} (capture at x(arity))"
        ),
        other => panic!("expected the resumed closure to return, got {other:?}"),
    }
}

#[test]
fn a_blocking_closure_via_call_fun2_reads_its_capture_from_a_register_operand() {
    // The `call_fun2` twin: the closure is named by a **register operand** (`x(2)`)
    // and the call argument is 11 (the oracle's `call11/1`), so the expected value is
    // `{7,11,99}` — the same capture placement, reached through the other instruction.
    let callee = callee_module();
    let (caller, run) = caller_building_a_closure_via_call_fun2();
    let mut ctx = SeamCtx::new(
        Arc::clone(&callee.module),
        callee.recv_lambda,
        rejecting(),
    );
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("a suspend is not an error");
    let RunOutcome::Wait { func, pc } = outcome else {
        panic!("the `call_fun2` closure must suspend the caller, got {outcome:?}");
    };
    assert_eq!(func, callee.recv_func);

    ctx.messages.push(make_small(99));
    let resumed = machine
        .resume_outcome(&caller, func, pc, &mut ctx)
        .expect("the resume completes");
    let expected = terms::make_tuple(&mut ctx, &[make_small(7), make_small(11), make_small(99)])
        .expect("the oracle tuple builds on the arena");
    match resumed {
        RunOutcome::Returned(value) => assert!(
            cmp(value, expected, &caller.atom_table).is_eq(),
            "the `call_fun2` closure body must return the oracle's {{7,11,99}}"
        ),
        other => panic!("expected the resumed closure to return, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (2) the third defect: a raise inside the applied closure lands in the caller
// ---------------------------------------------------------------------------

#[test]
fn a_raise_inside_the_seam_reached_closure_lands_in_the_callers_catch() {
    // Oracle: `sap3:raise_in_fun(7, fun(A) -> erlang:error(boom) end)` ->
    // `{caught,error,boom}`. Here the caller's handler returns the *reason* term, so
    // the expectation is the bare atom `boom` in class `error`.
    let callee = callee_module();
    let (caller, run) = caller_guarding_a_closure();
    let mut ctx = SeamCtx::new(
        Arc::clone(&callee.module),
        callee.raise_lambda,
        raising_error_bif(),
    );
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the raise inside the closure must be caught by the caller's try");

    assert_eq!(
        outcome,
        RunOutcome::Returned(caller_atom(&caller, "boom")),
        "the closure-delivered raise must land in the caller's catch frame"
    );
    assert_eq!(
        machine.catch_frame_depth(),
        0,
        "the handler consumed its catch frame"
    );
    assert_eq!(
        machine.pending_class(),
        Some(ExceptionClass::Error),
        "the landed class must be `error`"
    );
}

#[test]
fn a_routable_apply_fun_failure_lands_in_the_callers_catch() {
    // The `apply_fun` **fallback** route: the host declines the seam but its
    // `apply_fun` raises a routable `BeamError::BadArg`. The oracle's equivalent is the
    // closure-application machinery's own catchable `badarg`:
    //   `try apply(1, f, []) catch C:R -> {C,R} end`  ->  `{error,badarg}`.
    let callee = callee_module();
    let (caller, run) = caller_guarding_a_closure();
    let inner = MinimalContext::with_apply_hook(Box::new(|_fun, _args| Err(BeamError::BadArg)));
    let mut ctx = SeamCtx::with_inner(Arc::clone(&callee.module), callee.raise_lambda, inner)
        .declining();
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the fallback's routable badarg must be caught");

    assert_eq!(
        outcome,
        RunOutcome::Returned(caller_atom(&caller, "badarg")),
        "a routable `apply_fun` failure must land in the caller's catch frame"
    );
    assert_eq!(machine.catch_frame_depth(), 0);
    assert_eq!(machine.pending_class(), Some(ExceptionClass::Error));
}

#[test]
fn a_capability_gap_from_the_seam_stays_uncatchable() {
    // A genuine coverage limit (`error::unsupported`, whose reason atom the running
    // module cannot name) must NOT be swallowed by the catch-aware funnel: it stays the
    // uncatchable typed error even though a live `try` frame exists.
    let callee = callee_module();
    let (caller, run) = caller_guarding_a_closure();
    let mut ctx = SeamCtx::new(
        Arc::clone(&callee.module),
        callee.raise_lambda,
        rejecting(),
    )
    .reporting_a_gap();
    let mut machine = Machine::new();

    let err = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
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

#[test]
fn a_capability_gap_from_the_apply_fun_fallback_stays_uncatchable() {
    // The same contract on the fallback route.
    let callee = callee_module();
    let (caller, run) = caller_guarding_a_closure();
    let inner = MinimalContext::with_apply_hook(Box::new(|_fun, _args| {
        Err(beam_engine::unsupported("apply_fun: no closure registry"))
    }));
    let mut ctx = SeamCtx::with_inner(Arc::clone(&callee.module), callee.raise_lambda, inner)
        .declining();
    let mut machine = Machine::new();

    let err = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect_err("a capability gap is not catchable");

    assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
    assert_eq!(machine.catch_frame_depth(), 1);
}

// ---------------------------------------------------------------------------
// (3) a declining host is byte-identical: `call_fun` still uses `apply_fun`
// ---------------------------------------------------------------------------

#[test]
fn a_minimal_context_without_the_resolver_keeps_the_value_returning_fallback() {
    // A plain `MinimalContext` (the trait's `Ok(None)` default) with an apply hook:
    // `call_fun` stores the hook's value in `x(0)` and the run continues normally. The
    // closure term is opaque to this host.
    let (caller, run) = caller_taking_a_closure();
    let mut ctx = MinimalContext::with_apply_hook(Box::new(|_fun, args| {
        Ok(make_small(signed_value(args[0]) + 1))
    }));
    let mut machine = Machine::new();

    let fun = terms::make_fun(&mut ctx, 1, LAMBDA_UNIQ as u64, 0, &[])
        .expect("materialize the closure on the arena");
    let outcome = machine
        .run_export_outcome(&caller, run, 1, &[fun], &mut ctx)
        .expect("the fallback applies the closure");

    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(11)),
        "the apply hook's value (10 + 1) must land in x(0)"
    );
    assert!(
        machine.current_module().is_none(),
        "the value-returning fallback never leaves the caller's module context"
    );
}

#[test]
fn a_seam_that_declines_is_byte_identical_to_a_host_without_it() {
    // The same decline reached through the seam host: a `Seam::Decline` answer is the
    // trait's `Ok(None)`, so the engine takes the unchanged value-returning route.
    let callee = callee_module();
    let (caller, run) = caller_building_a_closure();
    let inner = MinimalContext::with_apply_hook(Box::new(|_fun, args| {
        Ok(make_small(signed_value(args[0]) + 1))
    }));
    let mut ctx = SeamCtx::with_inner(Arc::clone(&callee.module), callee.recv_lambda, inner)
        .declining();
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the declined closure falls back to apply_fun");

    assert_eq!(outcome, RunOutcome::Returned(make_small(11)));
    assert!(
        machine.current_module().is_none(),
        "a declined closure must not enter the callee module context"
    );
}
