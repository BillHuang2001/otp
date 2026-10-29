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

//! The **verbatim echo** of an `erlang:raise/3` supplied trace, end to end.
//!
//! The oracle returns the third argument of `erlang:raise/3` unchanged from a
//! handler's `build_stacktrace`: C keeps the caller's list in `c_p->ftrace` and
//! `build_stacktrace`'s `is_raised_exc` arm returns it with **no** decoding, name
//! resolution, frame counting or re-encoding
//! (`../../erts/emulator/beam/beam_common.c:1219-1223`; `raise_3`'s no-copy path
//! is literally `c_p->ftrace = stacktrace`,
//! `../../erts/emulator/beam/bif.c:1641-1643`). Three consequences are measured
//! here, each against a row taken from the system oracle (`erl` 29.1.2 /
//! erts-17.1.1, compiled module):
//!
//! ```text
//! 1> try erlang:raise(error, boom, []) catch _:_:S -> S end.
//! []
//! 1> try erlang:raise(error, boom, [{m,f,[1,2],[]}]) catch _:_:S -> S end.
//! [{m,f,[1,2],[]}]
//! 1> try erlang:raise(error, boom,
//!                     [{some_unknown_module,f,1,[{file,"z.erl"},{line,3}]}])
//!     catch _:_:S -> S end.
//! [{some_unknown_module,f,1,[{file,"z.erl"},{line,3}]}]
//! ```
//!
//! * the **empty** trace stays empty — its term `[]` is word-identical to "no trace
//!   supplied", so only the engine's one-shot marker can tell them apart
//!   ([`an_empty_supplied_trace_cooks_the_empty_list_where_an_absent_one_derives_frames`]);
//! * a frame whose `A` slot is an **argument list** is echoed, not refused — that
//!   shape is what `erlang:error/2,3` itself stores there
//!   ([`an_argument_list_frame_is_echoed_verbatim`]);
//! * a frame naming a **module this runtime never loaded** is echoed, because
//!   nothing resolves its names ([`an_unknown_module_frame_is_echoed_verbatim`]).
//!
//! [`a_caught_argument_list_trace_round_trips_through_a_re_raise`] closes the loop
//! the oracle closes: the trace an `erlang:error/2` raise cooked comes back
//! **unchanged** when it is handed to `erlang:raise/3` again, argument list and all.
//! [`the_folded_rethrow_keeps_the_echo`] pins the compiler-folded shape
//! (`sys_core_fold` turns `catch C:R:S -> erlang:raise(C,R,S)` into `raw_raise`),
//! where C stores the caught exception's trace **object** back into `c_p->ftrace`
//! and so keeps the echo — including for the empty trace, which must not be
//! re-derived into the previous exception's frames.
//!
//! Every row is driven through the machine's `ExecutionContext::call_bif` seam
//! ([`MinimalContext::with_bif_hook`]) rather than through `beam_bifs`, exactly as
//! the sibling suites do: what is under test is the **engine's** treatment of
//! [`beam_common::Raise::supplied_trace`], not a BIF implementation's. Each row
//! runs on the **interpreter** and on the **native baseline tier**, and tier
//! agreement is asserted *before* either side is compared with the expected term
//! (the discipline of [`super::error_info::cooked_on_both_tiers`]).

use super::*;
// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity. (An explicit
// import shadows the glob-imported `terms` above.)
use crate::terms::test_support as terms;

use beam_common::{LiveReason, Raise};
use beam_loader::{Atom, Export, Import, ImportRef};
use beam_term::{is_nil, Eterm, NIL};

use crate::families::exceptions::bif_frame::tests::atom_of;
use super::error_info::cooked_on_both_tiers;
use crate::terms::{list_elements, tuple_element};

// ---------------------------------------------------------------------------
// The fixture: a module whose `run/0` wraps host calls in `try … catch`
// ---------------------------------------------------------------------------

/// Every name the fixtures intern: the functions, the host's targets, the
/// class/reason pair, the frame names of the measured rows and the location keys.
const ATOMS: &[&str] = &[
    "run",
    "inner",
    "host",
    "target",
    "erlang",
    "raise",
    "error",
    "boom",
    "m",
    "f",
    "a",
    "b",
    "file",
    "line",
    "some_unknown_module",
];

/// A module with its own atom table and one import per [`Probe::import`] call.
struct Probe {
    module: Module,
}

impl Probe {
    fn new() -> Probe {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
        module.atom_table.seed(ATOMS.iter().copied());
        Probe { module }
    }

    /// Declare a `<module>:<function>/<arity>` import and hand back the ref that
    /// calls it.
    fn import(&mut self, module: &str, function: &str, arity: u32) -> ImportRef {
        let module = self.module.atom_table.intern(module);
        let function = self.module.atom_table.intern(function);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef(self.module.imports.len() as u32 - 1)
    }

    /// Append a function; returns its entry label (the first label in `code`).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.module.atom_table.intern(name);
        let module = self.module.name;
        let mut labels = BTreeMap::new();
        for (index, instr) in code.iter().enumerate() {
            if let Instr::Label(label) = instr {
                labels.insert(*label, index);
            }
        }
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

    /// Append a function and export it; returns its name atom.
    fn export(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Atom {
        let name_atom = self.module.atom_table.intern(name);
        let entry = self.add_function(name, arity, code);
        self.module.exports.push(Export {
            function: name_atom,
            arity,
            label: entry,
        });
        name_atom
    }
}

/// The handler shape the compiler emits for `try … catch C:R:S -> S end`: land the
/// exception, move its raw trace word into `x(0)` and cook it.
fn handler(frame: Operand) -> Vec<Instr> {
    vec![
        Instr::TryCase { frame },
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::BuildStacktrace,
        Instr::Deallocate(1),
        Instr::Return,
    ]
}

/// `run/0` = `try host:<function>() catch C:R:S -> S end` — one frame, one call, and
/// the handler returns the cooked trace.
fn caught_call(function: &str) -> (Module, Atom) {
    let mut probe = Probe::new();
    let import = probe.import("host", function, 0);
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(20),
        },
        Instr::CallExt {
            target: import,
            arity: 0,
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(20)),
    ];
    code.extend(handler(Operand::Y(0)));
    let run = probe.export("run", 0, code);
    (probe.module, run)
}

// ---------------------------------------------------------------------------
// The raise the host's `call_bif` seam returns
// ---------------------------------------------------------------------------

/// `error:boom` carrying `supplied` as the `erlang:raise/3` third argument.
fn raise_supplying(supplied: Eterm) -> BeamError {
    BeamError::Raise(
        Raise::error(ExitReason::atom("boom")).with_supplied_trace(LiveReason::new(supplied)),
    )
}

/// `error:boom` carrying the argument list `args` in the innermost item's `A` slot —
/// the shape `erlang:error/2` produces (`Raise::saved_args`).
fn raise_with_args(args: Eterm) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("boom")).with_saved_args(LiveReason::new(args)))
}

/// A context whose every host call raises `error:boom` supplying `supplied`.
fn supplying_ctx(supplied: Eterm) -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(move |_, _, _, _| Err(raise_supplying(supplied))))
}

/// A context whose every host call raises `error:boom` with **no** trace supplied.
fn bare_ctx() -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(|_, _, _, _| {
        Err(BeamError::Raise(Raise::error(ExitReason::atom("boom"))))
    }))
}

// ---------------------------------------------------------------------------
// The supplied trace terms, built on a context's pointer-stable heap
// ---------------------------------------------------------------------------

/// `[{m,f,[1,2],[]}]` — a supplied frame whose `A` slot is an **argument list**
/// (never an arity), exactly what the oracle echoes for
/// `erlang:raise(error, boom, [{m,f,[1,2],[]}])`.
fn argument_list_frame(ctx: &mut MinimalContext, module: &Module) -> Eterm {
    let args = terms::make_list(ctx, &[make_small(1), make_small(2)]).expect("the argument list");
    let item = terms::make_tuple(
        ctx,
        &[atom_of(module, "m"), atom_of(module, "f"), args, NIL],
    )
    .expect("the frame tuple");
    terms::make_list(ctx, &[item]).expect("the trace list")
}

/// `[{some_unknown_module,f,1,[{file,"z.erl"},{line,3}]}]` — the oracle's row for a
/// supplied frame naming a module this runtime never loaded.
fn unknown_module_frame(ctx: &mut MinimalContext, module: &Module) -> Eterm {
    let codes: Vec<Eterm> = "z.erl".chars().map(|c| make_small(c as i64)).collect();
    let name = terms::make_list(ctx, &codes).expect("the file name");
    let file = terms::make_tuple(ctx, &[atom_of(module, "file"), name]).expect("the {file,_} pair");
    let line = terms::make_tuple(ctx, &[atom_of(module, "line"), make_small(3)])
        .expect("the {line,_} pair");
    let location = terms::make_list(ctx, &[file, line]).expect("the location list");
    let item = terms::make_tuple(
        ctx,
        &[
            atom_of(module, "some_unknown_module"),
            atom_of(module, "f"),
            make_small(1),
            location,
        ],
    )
    .expect("the frame tuple");
    terms::make_list(ctx, &[item]).expect("the trace list")
}

// ===========================================================================
// 1. An empty supplied trace is NOT the same as no supplied trace
// ===========================================================================

/// `erlang:raise(error, boom, [])` cooks `[]`, while a raise that supplies **no**
/// trace at all cooks the `raise_snapshot`-derived frames.
///
/// Measured on the oracle: the first row is `[]` (C's `is_raised_exc` arm returns the
/// stored `NIL`); the second has no supplied trace to return, so `build_stacktrace`
/// resolves the saved chain. The engine's discriminator is the one-shot
/// `pending_supplied_term_verbatim` marker
/// ([`Machine::raise_supplied_term_verbatim`](crate::Machine::raise_supplied_term_verbatim)),
/// because the two raises arrive carrying the **same** trace word.
#[test]
fn an_empty_supplied_trace_cooks_the_empty_list_where_an_absent_one_derives_frames() {
    let (module, run) = caught_call("target");

    // The raise that supplies `[]` …
    let mut supplying = supplying_ctx(NIL);
    let empty = cooked_on_both_tiers(&module, run, &mut supplying);

    // … and the raise that supplies nothing.
    let mut absent = bare_ctx();
    let derived = cooked_on_both_tiers(&module, run, &mut absent);

    assert!(
        is_nil(empty),
        "`raise(error, boom, [])` must cook the empty trace, got {empty:?}"
    );
    assert!(
        !is_nil(derived),
        "a raise that supplied no trace must still cook the raise_snapshot frames"
    );
    assert!(
        !cmp(empty, derived, &module.atom_table).is_eq(),
        "an explicitly empty supplied trace and no supplied trace must be distinguishable"
    );
}

// ===========================================================================
// 2. An argument-list frame is echoed verbatim
// ===========================================================================

/// A supplied frame whose `A` slot is an **argument list** is echoed unchanged — the
/// shape `erlang:error/2,3` itself writes there, and the one the symbolic decoder
/// cannot express (its frames carry an arity).
#[test]
fn an_argument_list_frame_is_echoed_verbatim() {
    let (module, run) = caught_call("target");
    let mut build = MinimalContext::new();
    let supplied = argument_list_frame(&mut build, &module);
    let mut ctx = supplying_ctx(supplied);

    let cooked = cooked_on_both_tiers(&module, run, &mut ctx);
    let mut expected_ctx = MinimalContext::new();
    let expected = argument_list_frame(&mut expected_ctx, &module);
    assert!(
        cmp(cooked, expected, &module.atom_table).is_eq(),
        "the cooked trace must be [{{m,f,[1,2],[]}}] verbatim"
    );
    // The frame's `A` slot is the argument LIST, not an arity.
    let item = list_elements(cooked).expect("a proper list")[0];
    let args = tuple_element(item, 2).expect("a 4-tuple");
    assert!(
        cmp(args, expected_args(&mut expected_ctx, &module), &module.atom_table).is_eq(),
        "the A slot must stay the argument list [1,2]"
    );
}

/// The `[1,2]` argument list, for comparing an echoed frame's `A` slot.
fn expected_args(ctx: &mut MinimalContext, _module: &Module) -> Eterm {
    terms::make_list(ctx, &[make_small(1), make_small(2)]).expect("the argument list")
}

// ===========================================================================
// 3. An unknown-module frame is echoed verbatim
// ===========================================================================

/// A supplied frame naming a module the runtime never loaded is echoed as the term it
/// is: the echo resolves **no** name, so the `proc_lib`/`gen_server` re-raise shape
/// — which names modules the re-raising module never mentions — can never turn the
/// trace into a typed error.
#[test]
fn an_unknown_module_frame_is_echoed_verbatim() {
    let (module, run) = caught_call("target");
    let mut build = MinimalContext::new();
    let supplied = unknown_module_frame(&mut build, &module);
    let mut ctx = supplying_ctx(supplied);

    let cooked = cooked_on_both_tiers(&module, run, &mut ctx);
    let mut expected_ctx = MinimalContext::new();
    let expected = unknown_module_frame(&mut expected_ctx, &module);
    assert!(
        cmp(cooked, expected, &module.atom_table).is_eq(),
        "the cooked trace must be the supplied unknown-module frame verbatim"
    );
}

// ===========================================================================
// 4. The argument-list trace survives an `erlang:raise/3` re-raise
// ===========================================================================

/// `run/0` = `begin {C,R,S} = try erlang:error(boom, [a,b]) catch C:R:S -> {C,R,S} end,
///            try erlang:raise(C,R,S) catch C:R:S2 -> S2 end end` — the measured
/// program, as the **non-folded** `raise/3` shape (the compiler only folds a
/// verbatim `catch C:R:S -> erlang:raise(C,R,S)` handler body, which
/// [`folded_rethrow`] covers).
fn round_trip_module() -> (Module, Atom) {
    let mut probe = Probe::new();
    let error_2 = probe.import("erlang", "error", 2);
    let raise_3 = probe.import("erlang", "raise", 3);
    let error = probe.module.atom_table.intern("error");
    let boom = probe.module.atom_table.intern("boom");
    let a = probe.module.atom_table.lookup("a").expect("seeded");
    let b = probe.module.atom_table.lookup("b").expect("seeded");
    // A two-slot frame: `Y(0)` holds the first `try`'s handler address, `Y(1)` the
    // second's, exactly as the compiler nests two `try`s.
    let body = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 0,
        },
        // `erlang:error(boom, [a, b])`: the reason in `x(0)`, the argument list in
        // `x(1)` (consed tail-first).
        Instr::Move {
            src: Operand::Atom(boom),
            dst: Operand::X(0),
        },
        Instr::PutList {
            head: Operand::Atom(b),
            tail: Operand::Nil,
            dst: Operand::X(1),
        },
        Instr::PutList {
            head: Operand::Atom(a),
            tail: Operand::X(1),
            dst: Operand::X(1),
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(20),
        },
        Instr::CallExt {
            target: error_2,
            arity: 2,
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
        // The first handler cooks `S`, then re-raises with
        // `erlang:raise(error, boom, S)`.
        Instr::Label(Label(20)),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::BuildStacktrace,
        // `S` — the cooked trace — is in `x(0)`; `raise/3` wants it in `x(2)`.
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(2),
        },
        Instr::Move {
            src: Operand::Atom(error),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Atom(boom),
            dst: Operand::X(1),
        },
        Instr::Try {
            frame: Operand::Y(1),
            fail: Label(30),
        },
        Instr::CallExt {
            target: raise_3,
            arity: 3,
        },
        Instr::TryEnd {
            frame: Operand::Y(1),
        },
        Instr::Deallocate(2),
        Instr::Return,
        Instr::Label(Label(30)),
        Instr::TryCase {
            frame: Operand::Y(1),
        },
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::BuildStacktrace,
        Instr::Deallocate(2),
        Instr::Return,
    ];
    let run = probe.export("run", 0, body);
    (probe.module, run)
}

/// The end-to-end oracle row: an `erlang:error(Reason, [A,B])` raise cooks a frame
/// whose `A` slot is `[A,B]`, and handing **that** trace to `erlang:raise(C,R,S)`
/// yields the very same term back.
///
/// Before the verbatim echo landed, the re-raise's `build_stacktrace` decoded the
/// supplied frames and refused an argument-list `A` slot with the typed
/// `unsupported_bif`, which re-masked the `proc_lib`/`gen_server` re-raise path.
/// This pins the measured behaviour instead: nothing is decoded, so the trace
/// round-trips.
#[test]
fn a_caught_argument_list_trace_round_trips_through_a_re_raise() {
    let (module, run) = round_trip_module();
    let mut ctx = MinimalContext::new();
    let args = terms::make_list(&mut ctx, &[atom_of(&module, "a"), atom_of(&module, "b")])
        .expect("the argument list");
    ctx.set_bif_hook(Box::new(move |_, function, _, call_args| match function {
        // `erlang:error/2`: the argument list replaces the caller frame's `A` slot
        // (C's `EXC_ERROR_2`).
        "error" => Err(raise_with_args(args)),
        // `erlang:raise/3`: the third argument *is* the trace to echo, and it is
        // exactly the term the first handler cooked.
        "raise" => Err(raise_supplying(call_args[2])),
        _ => Err(BeamError::BadArg),
    }));

    let after = cooked_on_both_tiers(&module, run, &mut ctx);

    // The trace round-tripped: the raising frame still names `run/0` and its `A` slot
    // still carries `erlang:error/2`'s argument list.
    let items = list_elements(after).expect("a proper list");
    assert_eq!(items.len(), 1, "the raise happened in one function");
    let head = items[0];
    assert_eq!(
        tuple_element(head, 0).expect("a 4-tuple"),
        atom_of(&module, "m"),
        "the echoed frame names the raising module"
    );
    assert_eq!(
        tuple_element(head, 1).expect("a 4-tuple"),
        atom_of(&module, "run"),
        "the echoed frame names the raising function"
    );
    let mut expected_ctx = MinimalContext::new();
    let expected = terms::make_list(
        &mut expected_ctx,
        &[atom_of(&module, "a"), atom_of(&module, "b")],
    )
    .expect("the argument list");
    assert!(
        cmp(
            tuple_element(head, 2).expect("a 4-tuple"),
            expected,
            &module.atom_table
        )
        .is_eq(),
        "the echoed A slot must be erlang:error/2's argument list [a,b]"
    );
}

// ===========================================================================
// 5. The compiler-folded rethrow (`raw_raise`) keeps the echo
// ===========================================================================

/// `run/0` = `try inner() catch C:R:S -> S end` with
/// `inner/0` = `try host:target() catch C:R:S -> erlang:raise(C,R,S) end` — the shape
/// `sys_core_fold` turns into `raw_raise` (`../../lib/compiler/src/sys_core_fold.erl`,
/// confirmed by disassembling a compiled module).
fn folded_rethrow() -> (Module, Atom) {
    let mut probe = Probe::new();
    let import = probe.import("host", "target", 0);
    let inner = vec![
        Instr::Label(Label(10)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(20),
        },
        Instr::CallExt {
            target: import,
            arity: 0,
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(20)),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        // C's `raw_raise`: `x(0)` holds the class atom `try_case` copied from `x(3)`,
        // `x(1)` the value and `x(2)` the trace object.
        Instr::RawRaise,
    ];
    probe.add_function("inner", 0, inner);
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(40),
        },
        Instr::Call {
            target: Label(10),
            arity: 0,
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(40)),
    ];
    code.extend(handler(Operand::Y(0)));
    let run = probe.export("run", 0, code);
    (probe.module, run)
}

/// The folded rethrow must keep the echo: C stores the caught exception's trace
/// **object** back into `c_p->ftrace` (`../../erts/emulator/beam/beam_common.c:1164`),
/// so `is_raised_exc` still holds at the outer handler and `build_stacktrace` echoes
/// the trace instead of rebuilding one.
///
/// The empty trace is the discriminating row: re-deriving would render the frames of
/// the exception the re-raise left in the machine's snapshot (a supplied raise
/// deliberately rebuilds none), which is neither `[]` nor the supplied list.
#[test]
fn the_folded_rethrow_keeps_the_echo() {
    let (module, run) = folded_rethrow();

    // The empty supplied trace.
    let mut empty_ctx = supplying_ctx(NIL);
    let empty = cooked_on_both_tiers(&module, run, &mut empty_ctx);
    assert!(
        is_nil(empty),
        "a folded rethrow must keep the empty supplied trace, got {empty:?}"
    );

    // … and the argument-list frame, which the rethrow must not replace with the
    // snapshot's frames either.
    let mut build = MinimalContext::new();
    let supplied = argument_list_frame(&mut build, &module);
    let mut args_ctx = supplying_ctx(supplied);
    let echoed = cooked_on_both_tiers(&module, run, &mut args_ctx);
    let mut expected_ctx = MinimalContext::new();
    let expected = argument_list_frame(&mut expected_ctx, &module);
    assert!(
        cmp(echoed, expected, &module.atom_table).is_eq(),
        "a folded rethrow must echo the supplied argument-list frame"
    );
}
