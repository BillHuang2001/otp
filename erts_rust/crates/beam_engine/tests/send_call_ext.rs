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
//! The `call_ext*`-encoded `erlang:send/2` / `erlang:'!'/2` seam.
//!
//! # Why the seam exists
//!
//! The compiled `!` opcode (`Instr::Send`) delivers through
//! [`ExecutionContext::send_message_with_off_heap`], handing the *running*
//! machine's off-heap (refc) payload handles
//! ([`Machine::refc_binaries`](beam_engine::Machine::refc_binaries)) to the host so
//! the receiver-side copy can resolve an engine-built binary by its **data
//! pointer** (a *shared* copy) instead of flattening it onto the receiver's heap.
//!
//! An explicit `erlang:send(Pid, Msg)` — which the installed `erlc` compiles to
//! `{call_ext*,2,{extfunc,erlang,send,2}}` (verified with `beam_disasm`) — used to
//! fold into the synchronous `call_bif` route instead, which carries **no** handle
//! slice: a message the running machine had just built with
//! `bs_init_writable`/`bs_create_bin` was therefore flattened rather than shared.
//! `Machine::resolve_call_ext_send` reproduces the `send` opcode's delivery for the
//! `call_ext*` form, gated on a destination both routes resolve identically.
//!
//! # What this suite pins
//!
//! 1. **The handles are forwarded, and `x(0)` is the message.** `call_ext` (the
//!    non-tail form) calls the seam with exactly `machine.refc_binaries()` (same
//!    data pointers, in build order) and leaves the BIF's value — the message
//!    itself, as C's `BIF_SEND` returns — in `x(0)`.
//! 2. **The tail forms honour their frame discipline.** `call_ext_only` reuses the
//!    frame's existing continuation, `call_ext_last` pops its `deallocate` frame
//!    before following the successor frame's continuation; both forward the handles.
//! 3. **The `'!'/2` alias routes through the same seam** (it is the same BIF).
//! 4. **The destination gate falls through.** A destination that is not an internal
//!    pid or a reference — an atom, a port, a `{Name,Node}` pair — and any non-send
//!    import (`erlang:send/3`) stay on the ordinary `call_bif` route: the seam is
//!    **not** called, so no existing observable moves.
//! 5. **A reference destination routes through the seam** (both routes resolve it
//!    through the same alias lookup).
//! 6. **`TermNotFound` is the silent drop** C's `erl_send` and the `erlang:send/2`
//!    BIF route both perform (the message is returned), and a seam failure is the
//!    arm's ordinary raise.
//!
//! The modules are synthetic: the smallest bytecode shapes that reproduce each
//! `call_ext*` form's frame discipline. The off-heap binary is built by the engine
//! itself (`bs_init_writable`, capacity 1024 > `ERL_ONHEAP_BINARY_LIMIT`), so the
//! forwarded handles are genuinely machine-built ones and no test process state is
//! fabricated.

use std::collections::BTreeMap;

use beam_common::{BeamError, Result};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext, RunOutcome};
use beam_loader::{Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module};
use beam_term::{
    make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data, make_small,
    Atom, Eterm,
};

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
        // The class/reason atoms a landing may resolve, so a raise has something to
        // name (the same seed the sibling `call_ext_apply` suite uses).
        for atom in ["error", "exit", "throw", "EXIT", "badarg", "foo", "node"] {
            module.atom_table.intern(atom);
        }
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append an import and return its [`ImportRef`].
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

/// `module:name`'s atom as the loader's own [`Atom`] (a table **index**).
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

/// The name of the export to run.
fn export_atom(module: &Module, name: &str) -> Atom {
    atom(module, name)
}

// ---------------------------------------------------------------------------
// The recording host
// ---------------------------------------------------------------------------

/// What one `send_message_with_off_heap` call observed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct SendCall {
    /// The destination term the arm read from `x(0)`.
    to: Eterm,
    /// The message term the arm read from `x(1)`.
    message: Eterm,
    /// The **forwarded source off-heap table**: the payload data pointers of the
    /// handle slice, in order (the identity a receiver-side copy resolves by).
    data_ptrs: Vec<usize>,
}

/// What the stub seam answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeamReply {
    /// A completed delivery.
    Ok,
    /// The host's typed `badarg` (what the real seam answers for a destination it
    /// rejects).
    BadArg,
    /// The process table's typed "no such process" (what the real seam answers for
    /// a pid no process exists for).
    TermNotFound,
}

/// A host that records every seam call and every synchronous BIF call, so a test
/// can tell **which** route an arm took.
struct TestCtx {
    core: MinimalContext,
    /// Every `send_message_with_off_heap` call, in order.
    sends: Vec<SendCall>,
    /// `(module, function, arity)` of every `call_bif` the arm routed synchronously.
    bifs: Vec<(String, String, u32)>,
    /// The seam's answer.
    reply: SeamReply,
}

impl TestCtx {
    fn new() -> TestCtx {
        TestCtx {
            core: MinimalContext::new(),
            sends: Vec::new(),
            bifs: Vec::new(),
            reply: SeamReply::Ok,
        }
    }

    /// Build a term on this context's arena and hand back the raw words, so a test
    /// can place a hand-built reference thing.
    fn arena_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.core.alloc_words(n)
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
        self.bifs
            .push((module.to_string(), function.to_string(), arity));
        // The floor answers the two send shapes so the *fall-through* route has a
        // defined value (the message, exactly what the real BIF returns).
        if module == "erlang" && (function == "send" || function == "!") {
            return Ok(args[1]);
        }
        Err(beam_engine::unsupported(&format!(
            "bif {module}:{function}/{arity}"
        )))
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

    fn send_message_with_off_heap(
        &mut self,
        to: Eterm,
        message: Eterm,
        src_binaries: &[beam_alloc::RefcBinary],
    ) -> Result<()> {
        self.sends.push(SendCall {
            to,
            message,
            data_ptrs: src_binaries
                .iter()
                .map(|binary| binary.data_ptr() as usize)
                .collect(),
        });
        match self.reply {
            SeamReply::Ok => Ok(()),
            SeamReply::BadArg => Err(BeamError::BadArg),
            SeamReply::TermNotFound => Err(BeamError::TermNotFound),
        }
    }
}

/// The payload data pointers of the machine's in-flight handles, in build order.
fn machine_handles(machine: &Machine) -> Vec<usize> {
    machine
        .refc_binaries()
        .iter()
        .map(|binary| binary.data_ptr() as usize)
        .collect()
}

// ---------------------------------------------------------------------------
// The synthetic modules
// ---------------------------------------------------------------------------

/// Builds an off-heap (refc) binary **in the engine** and returns the term it left
/// in `x(0)`: `wdr/1` = `bs_init_writable` with the capacity in `x(0)`.
///
/// `bs_init_writable` allocates a ref-counted payload of `capacity` bytes and
/// reports a zero-length window over it, so the machine records one off-heap
/// handle for it — the genuinely machine-built payload the forwarded slice must
/// carry.
fn build_off_heap_binary(machine: &mut Machine, ctx: &mut TestCtx, capacity: i64) -> Eterm {
    let mut fixture = Fixture::new("wdr");
    let code = vec![
        Instr::Label(Label(1)),
        Instr::BsInitWritable,
        Instr::Return,
    ];
    let entry = fixture.add_function("wdr", 1, code);
    fixture.add_export("wdr", 1, entry);
    let module = fixture.finish();

    match machine
        .run_export_outcome(
            &module,
            export_atom(&module, "wdr"),
            1,
            &[make_small(capacity)],
            ctx,
        )
        .expect("bs_init_writable must build the writable accumulator")
    {
        RunOutcome::Returned(term) => term,
        other => panic!("bs_init_writable suspended: {other:?}"),
    }
}

/// The module under test: one function per `call_ext*` form, all importing the
/// send BIFs.
struct SendFixture {
    module: Module,
}

impl SendFixture {
    /// `run/2` — `call_ext` (non-tail): the callee's value is followed by the
    /// function's own `return`, whose continuation was stored in the live frame.
    fn call_ext_module() -> SendFixture {
        let mut fixture = Fixture::new("m");
        let target = fixture.import("erlang", "send", 2);
        let code = vec![
            Instr::Label(Label(1)),
            Instr::CallExt { arity: 2, target },
            Instr::Return,
        ];
        let entry = fixture.add_function("run", 2, code);
        fixture.add_export("run", 2, entry);
        SendFixture {
            module: fixture.finish(),
        }
    }

    /// `run/2` — `call_ext_only` (tail): nothing is stored; the frame's **existing**
    /// continuation is followed.
    fn call_ext_only_module() -> SendFixture {
        let mut fixture = Fixture::new("m");
        let target = fixture.import("erlang", "send", 2);
        let code = vec![
            Instr::Label(Label(1)),
            Instr::CallExtOnly { arity: 2, target },
        ];
        let entry = fixture.add_function("run", 2, code);
        fixture.add_export("run", 2, entry);
        SendFixture {
            module: fixture.finish(),
        }
    }

    /// `run/2` — `call_ext_last` (tail, `deallocate 0`): an `allocate 0` frame is
    /// popped on the success path before the successor frame's continuation is
    /// followed.
    fn call_ext_last_module() -> SendFixture {
        let mut fixture = Fixture::new("m");
        let target = fixture.import("erlang", "send", 2);
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 2,
            },
            Instr::CallExtLast {
                arity: 2,
                target,
                deallocate: 0,
            },
        ];
        let entry = fixture.add_function("run", 2, code);
        fixture.add_export("run", 2, entry);
        SendFixture {
            module: fixture.finish(),
        }
    }

    /// `run/2` — `call_ext` to `erlang:'!'/2` (the same BIF under its operator
    /// name).
    fn bang_module() -> SendFixture {
        let mut fixture = Fixture::new("m");
        let target = fixture.import("erlang", "!", 2);
        let code = vec![
            Instr::Label(Label(1)),
            Instr::CallExt { arity: 2, target },
            Instr::Return,
        ];
        let entry = fixture.add_function("run", 2, code);
        fixture.add_export("run", 2, entry);
        SendFixture {
            module: fixture.finish(),
        }
    }

    /// `run/3` — `call_ext_only` to `erlang:send/3`, which is **not** special-cased
    /// (the option list belongs to the BIF, not to the delivery): the arm must stay
    /// on the ordinary `call_bif` route.
    fn send3_module() -> SendFixture {
        let mut fixture = Fixture::new("m");
        let target = fixture.import("erlang", "send", 3);
        let code = vec![
            Instr::Label(Label(1)),
            Instr::CallExtOnly { arity: 3, target },
        ];
        let entry = fixture.add_function("run", 3, code);
        fixture.add_export("run", 3, entry);
        SendFixture {
            module: fixture.finish(),
        }
    }

    fn run(
        &self,
        machine: &mut Machine,
        arity: u32,
        args: &[Eterm],
        ctx: &mut TestCtx,
    ) -> Result<RunOutcome> {
        machine.run_export_outcome(
            &self.module,
            export_atom(&self.module, "run"),
            arity,
            args,
            ctx,
        )
    }
}

/// An internal pid term (`make_internal_pid`).
fn a_pid() -> Eterm {
    make_internal_pid(make_pid_data(7, 1234))
}

// ---------------------------------------------------------------------------
// (1) the non-tail form, and the forwarded handles
// ---------------------------------------------------------------------------

#[test]
fn call_ext_erlang_send_forwards_the_machines_off_heap_handles() {
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    // The message IS the machine-built off-heap binary: the handle the receiver
    // copy needs to resolve it by data pointer must be forwarded.
    let message = build_off_heap_binary(&mut machine, &mut ctx, 1024);
    let handles = machine_handles(&machine);
    assert_eq!(handles.len(), 1, "one machine-built off-heap payload");

    let to = a_pid();
    let fixture = SendFixture::call_ext_module();
    let outcome = fixture
        .run(&mut machine, 2, &[to, message], &mut ctx)
        .expect("the send seam completes with no off-heap source error");

    // (a) the seam was called, with exactly the machine's handles ...
    assert_eq!(ctx.sends.len(), 1, "one seam call");
    assert_eq!(
        ctx.sends[0],
        SendCall {
            to,
            message,
            data_ptrs: handles.clone(),
        },
        "the seam receives (`x(0)`, `x(1)`, `machine.refc_binaries()`) verbatim"
    );
    // ... and the BIF route was never reached.
    assert!(ctx.bifs.is_empty(), "no synchronous `call_bif` on the seam route");
    // The BIF's value is the message itself (`Pid ! Msg` is `Msg`), in `x(0)`.
    assert_eq!(outcome, RunOutcome::Returned(message));
    assert_eq!(machine.x(0), Some(message));
}

#[test]
fn a_machine_with_no_off_heap_binary_forwards_an_empty_slice() {
    // The negative baseline: with no machine-built payload the forwarded slice is
    // empty (the documented degradation is a receiver-copy concern, not this arm's),
    // and the delivery still happens.
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    let to = a_pid();
    let message = make_small(42);
    let fixture = SendFixture::call_ext_module();

    let outcome = fixture
        .run(&mut machine, 2, &[to, message], &mut ctx)
        .expect("a plain on-heap message delivers");

    assert_eq!(outcome, RunOutcome::Returned(message));
    assert_eq!(ctx.sends.len(), 1);
    assert!(ctx.sends[0].data_ptrs.is_empty());
}

// ---------------------------------------------------------------------------
// (2) the tail forms
// ---------------------------------------------------------------------------

#[test]
fn call_ext_only_erlang_send_forwards_the_handles_and_returns_the_message() {
    // A tail call stores nothing: the seam runs and the frame's *existing*
    // continuation (the bottom `Halt` frame) yields `x(0)`.
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    let message = build_off_heap_binary(&mut machine, &mut ctx, 1024);
    let handles = machine_handles(&machine);
    let to = a_pid();
    let fixture = SendFixture::call_ext_only_module();

    let outcome = fixture
        .run(&mut machine, 2, &[to, message], &mut ctx)
        .expect("the tail send completes");

    assert_eq!(ctx.sends.len(), 1);
    assert_eq!(ctx.sends[0].data_ptrs, handles);
    assert_eq!(ctx.sends[0].to, to);
    assert!(ctx.bifs.is_empty());
    assert_eq!(outcome, RunOutcome::Returned(message));
    assert_eq!(machine.frame_depth(), 1, "a tail call stores no frame");
}

#[test]
fn call_ext_last_erlang_send_forwards_the_handles_and_pops_its_frame() {
    // The `deallocate 0` frame is popped exactly once, on the success path, before
    // the successor frame's continuation is followed (the `call_ext_last`
    // discipline the synchronous BIF success path applies).
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    let message = build_off_heap_binary(&mut machine, &mut ctx, 1024);
    let handles = machine_handles(&machine);
    assert!(!handles.is_empty());
    let to = a_pid();
    let fixture = SendFixture::call_ext_last_module();

    let outcome = fixture
        .run(&mut machine, 2, &[to, message], &mut ctx)
        .expect("the tail send completes");

    assert_eq!(ctx.sends.len(), 1);
    assert_eq!(ctx.sends[0].data_ptrs, handles);
    assert_eq!(ctx.sends[0].to, to);
    assert!(ctx.bifs.is_empty());
    assert_eq!(outcome, RunOutcome::Returned(message));
    assert_eq!(
        machine.frame_depth(),
        1,
        "the allocate frame is popped, leaving the bottom frame"
    );
}

// ---------------------------------------------------------------------------
// (3) the `'!'/2` alias
// ---------------------------------------------------------------------------

#[test]
fn the_bang_alias_routes_through_the_same_seam() {
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    let message = build_off_heap_binary(&mut machine, &mut ctx, 1024);
    let handles = machine_handles(&machine);
    let to = a_pid();
    let fixture = SendFixture::bang_module();

    let outcome = fixture
        .run(&mut machine, 2, &[to, message], &mut ctx)
        .expect("`erlang:!'/2` delivers through the seam");

    assert_eq!(ctx.sends.len(), 1);
    assert_eq!(ctx.sends[0].data_ptrs, handles);
    assert!(ctx.bifs.is_empty());
    assert_eq!(outcome, RunOutcome::Returned(message));
}

// ---------------------------------------------------------------------------
// (4) the destination gate: fall-through for everything the routes do not share
// ---------------------------------------------------------------------------

/// Asserts the arm took the **synchronous `call_bif`** route: the seam was never
/// called, the BIF was, and the BIF's value (the message) came back.
fn assert_fell_through(
    ctx: &TestCtx,
    outcome: &RunOutcome,
    expected: &[(String, String, u32)],
    message: Eterm,
) {
    assert!(
        ctx.sends.is_empty(),
        "the seam must not be reached for this destination"
    );
    assert_eq!(ctx.bifs, expected, "the ordinary BIF route answers");
    assert_eq!(*outcome, RunOutcome::Returned(message));
}

#[test]
fn an_atom_destination_falls_through_to_the_bif_route() {
    // The BIF route does a registered-name lookup (and can deliver); the seam is
    // `badarg`. The observables must not move, so the arm falls through.
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    let fixture = SendFixture::call_ext_module();
    let message = make_small(42);

    let outcome = fixture
        .run(
            &mut machine,
            2,
            &[atom_term(&fixture.module, "foo"), message],
            &mut ctx,
        )
        .expect("the fall-through route answers with the message");

    assert_fell_through(
        &ctx,
        &outcome,
        &[("erlang".to_string(), "send".to_string(), 2)],
        message,
    );
}

#[test]
fn a_port_destination_falls_through_to_the_bif_route() {
    // The BIF route reports its own typed capability gap for a port; the seam is
    // `badarg`. Different observables ⇒ fall through.
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    let fixture = SendFixture::call_ext_module();
    let message = make_small(42);
    let port = make_internal_port(make_port_data(3));

    let outcome = fixture
        .run(&mut machine, 2, &[port, message], &mut ctx)
        .expect("the fall-through route answers with the message");

    assert_fell_through(
        &ctx,
        &outcome,
        &[("erlang".to_string(), "send".to_string(), 2)],
        message,
    );
}

#[test]
fn a_name_node_tuple_destination_falls_through_to_the_bif_route() {
    // `{Name, Node}` is the distributed-send shape: the BIF route reports its typed
    // capability gap, the seam is `badarg`. Different observables ⇒ fall through.
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    let fixture = SendFixture::call_ext_module();
    let message = make_small(42);
    let name = atom_term(&fixture.module, "foo");
    let node = atom_term(&fixture.module, "node");
    let pair = terms::make_tuple(&mut ctx, &[name, node]).expect("a 2-tuple");

    let outcome = fixture
        .run(&mut machine, 2, &[pair, message], &mut ctx)
        .expect("the fall-through route answers with the message");

    assert_fell_through(
        &ctx,
        &outcome,
        &[("erlang".to_string(), "send".to_string(), 2)],
        message,
    );
}

#[test]
fn a_non_send_import_is_on_the_ordinary_route() {
    // `erlang:send/3` takes an option list that belongs to the BIF: it is not the
    // compiled-send shape and must keep its existing route even for a pid
    // destination.
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    let fixture = SendFixture::send3_module();
    let message = make_small(42);
    let to = a_pid();

    let outcome = fixture
        .run(&mut machine, 3, &[to, message, make_small(0)], &mut ctx)
        .expect("the fall-through route answers with the message");

    assert_fell_through(
        &ctx,
        &outcome,
        &[("erlang".to_string(), "send".to_string(), 3)],
        message,
    );
}

// ---------------------------------------------------------------------------
// (5) a reference destination routes through the seam
// ---------------------------------------------------------------------------

#[test]
fn a_reference_destination_routes_through_the_seam() {
    // Both routes resolve a reference through the same alias lookup (the reference
    // is passed to the host verbatim), so the gate admits it.
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    let message = build_off_heap_binary(&mut machine, &mut ctx, 1024);
    let handles = machine_handles(&machine);

    let words = ctx.arena_words(beam_term::INTERNAL_REF_WORDS).expect("arena");    // SAFETY: `alloc_words` handed out `INTERNAL_REF_WORDS` (4) exclusively-owned,
    // properly aligned words on this context's pointer-stable arena, which outlives
    // the run; `write_internal_ref` writes exactly those words and returns the
    // tagged root pointing into them.
    let reference = unsafe { beam_term::write_internal_ref(words, 42) }
        .expect("an in-range reference id");

    let fixture = SendFixture::call_ext_module();
    let outcome = fixture
        .run(&mut machine, 2, &[reference, message], &mut ctx)
        .expect("an alias destination delivers through the seam");

    assert_eq!(ctx.sends.len(), 1);
    assert_eq!(ctx.sends[0].to, reference);
    assert_eq!(ctx.sends[0].data_ptrs, handles);
    assert!(ctx.bifs.is_empty());
    assert_eq!(outcome, RunOutcome::Returned(message));
}

// ---------------------------------------------------------------------------
// (6) the two seam outcomes: the silent drop and the raise
// ---------------------------------------------------------------------------

#[test]
fn a_send_to_a_pid_with_no_process_is_the_silent_drop_the_bif_route_performs() {
    // C's `erl_send` drops the message and reports success for a pid no process
    // exists for, and the `erlang:send/2` BIF route maps the table's typed
    // `TermNotFound` to exactly that. The seam route must agree — a send to a dead
    // pid is not an exception.
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    ctx.reply = SeamReply::TermNotFound;
    let to = a_pid();
    let message = make_small(42);
    let fixture = SendFixture::call_ext_module();

    let outcome = fixture
        .run(&mut machine, 2, &[to, message], &mut ctx)
        .expect("a dropped send is a success");

    assert_eq!(ctx.sends.len(), 1, "the seam still ran");
    assert_eq!(outcome, RunOutcome::Returned(message));
}

#[test]
fn a_seam_failure_is_the_arms_ordinary_raise() {
    // A seam failure is reported as the arm's raise, i.e. exactly what the
    // synchronous `call_bif` failure produces: with no live catch frame the typed
    // error propagates unchanged (never a fabricated value).
    let mut machine = Machine::new();
    let mut ctx = TestCtx::new();
    ctx.reply = SeamReply::BadArg;
    let fixture = SendFixture::call_ext_module();

    let error = fixture
        .run(&mut machine, 2, &[a_pid(), make_small(42)], &mut ctx)
        .expect_err("an uncaught seam failure cannot land");

    assert!(
        matches!(error, BeamError::BadArg),
        "the seam's typed error propagates, got {error:?}"
    );
    assert_eq!(ctx.sends.len(), 1, "the seam was the route taken");
    assert_eq!(machine.catch_frame_depth(), 0);
}
