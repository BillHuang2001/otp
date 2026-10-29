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

//! The oracle-pinned **`error_info` measurement table**, end to end.
//!
//! [`super::bif_frame`] synthesizes the innermost cooked-stacktrace item of a raise
//! coming out of a BIF — `{Module, Function, ArgList, Location}`, with the
//! `[{error_info, #{module => FormatModule}}]` location extension the closed
//! format-module table selects (`../../erts/emulator/beam/beam_common.c:877-1063`,
//! `:937-1006`) — and [`super::bif_frame::tests`] pins that builder's unit shapes.
//! **This** module pins the measured **rows** through the real instruction routes,
//! not through the builder: each row is a hand-built `Module` whose code wraps one
//! failing call site in `try … catch C:R:S -> S`, so which site synthesizes the item
//! (and which deliberately does not) is observable rather than assumed.
//!
//! The rows below were measured on the system oracle (`erl` 29.1.2 /
//! erts-17.1.1) with a compiled module; each row's comment names its measured item.
//! Two deviations from the oracle's *operands* are deliberate and local:
//!
//! * a fixture's argument can only be what a `move`/`put_list` operand can spell (an
//!   immediate, or one `put_list` cell), so the `binary:at` row passes small
//!   integers where the oracle measured a `<<1:1>>` bitstring. That row pins the
//!   **format-module arm** and the argument positions; the "the `A` slot is a real
//!   argument list, never an arity" rule is additionally pinned by rows whose
//!   arguments *are* heap terms (an atom, and the `apply/3` row's `[1]` list);
//! * a synthetic module has no `Line` chunk, so a **caller** frame's `Location` is
//!   the empty list (or the bare `[{error_info, Map}]` extension, C's
//!   `LINE_INVALID_LOCATION` arm) rather than the oracle's `[{file,_},{line,_}]`.
//!
//! Every row runs on the **interpreter** and on the **native baseline tier** and
//! asserts tier agreement *before* either side is compared with the expected item —
//! a mistake the two tiers share still has to fail somewhere else.

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

use crate::context::Mailbox;
use crate::families::exceptions::bif_frame::tests::{atom_of, expected_item};
use crate::jit::Tier;
use crate::terms::{list_elements, tuple_element};

// ---------------------------------------------------------------------------
// The fixture scaffolding
// ---------------------------------------------------------------------------

/// Every name a row's fixture must intern: the BIF modules/functions the table
/// names, the extension's own names, the `badarg`/`error` atoms a landing raise
/// installs, and the row arguments the tests spell as atoms.
const ROW_ATOMS: &[&str] = &[
    "erlang",
    "hd",
    "send",
    "lists",
    "max",
    "function_clause",
    "binary",
    "at",
    "binary_to_atom",
    "os",
    "getenv",
    "math",
    "log",
    "re",
    "run",
    "atomics",
    "get",
    "counters",
    "records",
    "apply",
    "error",
    "exit",
    "throw",
    "error_info",
    "module",
    "erl_erts_errors",
    "erl_stdlib_errors",
    "erl_kernel_errors",
    "badarg",
    "failed",
    // The raise reasons the rows' hooks install. A reason atom must be *nameable*
    // ([`crate::machine::namespace_atom`]) for the raise to land in `x(1)` of the
    // handler at all — an unnameable reason propagates as a bare `Err` — so each
    // row's fixture interns its own reason.
    "foo",
    "stop",
    "ball",
    "oops",
    "undef",
    "a",
    "b",
    "x",
    "notaref",
    "hi",
];

/// One register value a row's fixture `move`s into `x(i)` — the two shapes the
/// measured rows need (an immediate and an atom of the fixture's own table).
#[derive(Clone, Copy)]
enum Reg {
    Int(i64),
    Atom(&'static str),
    /// The empty list — `Operand::Nil`, the one compound term a `move` can spell
    /// without a heap cell (`lists:max([])`'s argument).
    Nil,
}

impl Reg {
    /// The `move` source operand, with an atom resolved against the fixture's table.
    fn operand(self, module: &Module) -> Operand {
        match self {
            Reg::Int(value) => Operand::Integer(value),
            Reg::Atom(name) => Operand::Atom(
                module
                    .atom_table
                    .lookup(name)
                    .unwrap_or_else(|| panic!("the fixture interns {name}")),
            ),
            Reg::Nil => Operand::Nil,
        }
    }

    /// The term the item's argument list must hold, built the same way.
    fn term(self, module: &Module) -> Eterm {
        match self {
            Reg::Int(value) => make_small(value),
            Reg::Atom(name) => atom_of(module, name),
            Reg::Nil => NIL,
        }
    }
}

/// The terms a row's `Reg`s materialize, for the expected item.
fn terms_of(module: &Module, registers: &[Reg]) -> Vec<Eterm> {
    registers.iter().map(|register| register.term(module)).collect()
}

/// A `MinimalContext` whose BIF answer is the typed `badarg` for **everything** —
/// the shape a host returns for a wrongly-typed argument, which is how every
/// measured row fails. No BIF implementation is consulted, so a row pins the
/// *engine's* item, never a mock's.
fn badarg_ctx() -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(|_, _, _, _| Err(BeamError::BadArg)))
}

/// A hook that raises `error` for exactly the row's `(module, function, arity)` and
/// refuses everything else with `badarg` — the honesty allowlist: a stray call the
/// fixture is not supposed to make fails loudly instead of answering the row's
/// raise and making a wrong test pass.
fn raising_ctx(
    target: (&'static str, &'static str, u32),
    reason: &'static str,
    class: ExceptionClass,
) -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(move |module, function, arity, _args| {
        if (module, function, arity) == target {
            return Err(BeamError::Raise(Raise::new(class, ExitReason::atom(reason))));
        }
        Err(BeamError::BadArg)
    }))
}

/// Run `run/0` of `module` on `Tier::Interpreter` **and** `Tier::BaselineJit` and
/// hand back the cooked stacktrace — asserting tier agreement **first**.
pub(super) fn cooked_on_both_tiers(module: &Module, run_atom: Atom, ctx: &mut MinimalContext) -> Eterm {
    let mut interpreter = Machine::new();
    let cooked = interpreter
        .run_export(module, run_atom, 0, &[], ctx)
        .expect("the handler returns the cooked trace");
    let mut jit = Machine::new();
    let cooked_jit = jit
        .run_with_tier(module, run_atom, 0, &[], ctx, Tier::BaselineJit)
        .expect("the handler returns the cooked trace on the native tier too");
    // A mistake both tiers share still has to fail somewhere else, so tier agreement
    // is asserted before the expected item is compared with either side.
    assert!(
        cmp(cooked, cooked_jit, &module.atom_table).is_eq(),
        "the interpreter and the baseline tier must cook the same trace"
    );
    cooked
}

/// Assert the innermost cooked item of `cooked` is exactly the oracle's
/// `{bif_module, bif_function, args, [{error_info, #{module => format_module}}]}`
/// (or an empty location when the BIF module is outside the closed table).
///
/// The comparison is **field by field** as well as whole-term, because the whole
/// item would also agree if both sides were wrong in the same way: the third field
/// must be the argument **list term**, never an arity, and the fourth the location
/// extension (`[]` vs `[{error_info, #{module => X}}]`).
fn assert_head_item(
    module: &Module,
    ctx: &mut MinimalContext,
    cooked: Eterm,
    bif_module: &str,
    bif_function: &str,
    args: &[Eterm],
    format_module: Option<&str>,
) {
    let items = list_elements(cooked).expect("the cooked stacktrace is a proper list");
    assert!(
        !items.is_empty(),
        "the cooked stacktrace must hold at least the BIF's own item"
    );
    let item = items[0];
    assert_eq!(
        tuple_element(item, 0).unwrap(),
        atom_of(module, bif_module),
        "the item's module field must name the BIF's module"
    );
    assert_eq!(
        tuple_element(item, 1).unwrap(),
        atom_of(module, bif_function),
        "the item's function field must name the BIF's function"
    );
    let expected_args = terms::make_list(ctx, args).expect("the arena never refuses a reservation");
    assert!(
        cmp(tuple_element(item, 2).unwrap(), expected_args, &module.atom_table).is_eq(),
        "the item's A slot must be the argument LIST, never an arity"
    );
    let location = tuple_element(item, 3).unwrap();
    match format_module {
        None => assert_eq!(
            location, NIL,
            "a BIF module outside the table gets no `error_info` extension"
        ),
        Some(format) => {
            let map =
                terms::make_flatmap(ctx, &[(atom_of(module, "module"), atom_of(module, format))])
                    .expect("the arena never refuses a reservation");
            let pair = terms::make_tuple(ctx, &[atom_of(module, "error_info"), map])
                .expect("the arena never refuses a reservation");
            let expected =
                terms::make_list(ctx, &[pair]).expect("the arena never refuses a reservation");
            assert!(
                cmp(location, expected, &module.atom_table).is_eq(),
                "the location must be `[{{error_info,#{{module => {format}}}}}]`"
            );
        }
    }
    // The same shape, built independently by the unit suite's oracle-item builder,
    // so the two suites cannot drift apart.
    let expected = expected_item(ctx, module, bif_module, bif_function, args, format_module);
    assert!(
        cmp(item, expected, &module.atom_table).is_eq(),
        "the item must agree with `bif_frame::tests::expected_item` term for term"
    );
}

/// Assert the cooked trace holds exactly **one** item and that it is the
/// **caller's** own frame: the module `m`, the function `run`, `a_slot` in the `A`
/// slot and (with no `Line` chunk in a synthetic module) an empty location.
///
/// This is the shape C's `non_bif_stacktrace:` arm renders for
/// `erlang:error/1,2,3`, `erlang:exit/1` and `erlang:throw/1`
/// (`../../erts/emulator/beam/beam_common.c:1005-1032`): no BIF item may be
/// prepended to those raises.
fn assert_caller_frame(module: &Module, cooked: Eterm, a_slot: Eterm) {
    let items = list_elements(cooked).expect("the cooked stacktrace is a proper list");
    assert_eq!(
        items.len(),
        1,
        "no BIF head item may be prepended to the raising BIFs"
    );
    let item = items[0];
    assert_eq!(
        tuple_element(item, 0).unwrap(),
        atom_of(module, "m"),
        "the item's module is the caller's module"
    );
    assert_eq!(
        tuple_element(item, 1).unwrap(),
        atom_of(module, "run"),
        "the item's function is the caller's own function"
    );
    assert_eq!(
        tuple_element(item, 2).unwrap(),
        a_slot,
        "the caller frame's A slot"
    );
    assert_eq!(
        tuple_element(item, 3).unwrap(),
        NIL,
        "a synthetic caller frame has no source location and no extension"
    );
}

/// `run/0` = `try <bif_module>:<bif_function>(<registers…>) catch C:R:S -> S end`,
/// compiled by hand (`Label` / `Allocate` / `Move` / `Try` / `CallExt` / `TryEnd` /
/// `Return` plus the compiler's `TryCase` → `move x(2) -> x(0)` →
/// `build_stacktrace` handler), so the route under test is unambiguous.
///
/// `import_arity` is the **import table's** declared arity and `registers` are the
/// values moved into `x(0..n)` — the instruction's own arity is `registers.len()`,
/// which is what makes an import/instruction arity split observable.
fn call_ext_traced(
    bif_module: &str,
    bif_function: &str,
    import_arity: u32,
    registers: &[Reg],
) -> (Module, Atom) {
    let mut fixture = Fixture::new(ROW_ATOMS);
    let module_atom = fixture.module.atom_table.intern(bif_module);
    let function_atom = fixture.module.atom_table.intern(bif_function);
    let run_atom = fixture.module.atom_table.intern("run");
    fixture.module.imports.push(Import {
        module: module_atom,
        function: function_atom,
        arity: import_arity,
    });
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
    ];
    for (index, register) in registers.iter().enumerate() {
        code.push(Instr::Move {
            src: register.operand(&fixture.module),
            dst: Operand::X(index as u32),
        });
    }
    code.extend([
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(20),
        },
        Instr::CallExt {
            arity: registers.len() as u32,
            target: ImportRef(0),
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
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::BuildStacktrace,
        Instr::Deallocate(1),
        Instr::Return,
    ]);
    let entry = fixture.add_function("run", 0, code);
    fixture.module.exports.push(Export {
        function: run_atom,
        arity: 0,
        label: entry,
    });
    (fixture.finish(), run_atom)
}

/// `run/0` = `try apply(a, b, [1]) catch C:R:S -> S end` — the `erlang:apply/3`
/// route, whose callee identity rides `x(0)`/`x(1)` and whose argument list rides
/// `x(2)` (C's `fixed_apply` register shape). The list is the one heap term a
/// hand-built fixture can spell (`put_list`).
fn apply_traced() -> (Module, Atom) {
    let mut fixture = Fixture::new(ROW_ATOMS);
    let erlang = fixture.module.atom_table.intern("erlang");
    let apply = fixture.module.atom_table.intern("apply");
    let a = fixture.module.atom_table.intern("a");
    let b = fixture.module.atom_table.intern("b");
    let run_atom = fixture.module.atom_table.intern("run");
    fixture.module.imports.push(Import {
        module: erlang,
        function: apply,
        arity: 3,
    });
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Atom(a),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Atom(b),
            dst: Operand::X(1),
        },
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Nil,
            dst: Operand::X(2),
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(20),
        },
        Instr::CallExt {
            arity: 3,
            target: ImportRef(0),
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
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::BuildStacktrace,
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let entry = fixture.add_function("run", 0, code);
    fixture.module.exports.push(Export {
        function: run_atom,
        arity: 0,
        label: entry,
    });
    (fixture.finish(), run_atom)
}

/// `run/0` = `try 1 ! hi catch C:R:S -> S end` — the `Instr::Send` route, whose
/// destination and message ride `x(0)`/`x(1)` (`emu/bif_instrs.tab:436-441`).
fn send_traced() -> (Module, Atom) {
    let mut fixture = Fixture::new(ROW_ATOMS);
    let hi = fixture.module.atom_table.intern("hi");
    let run_atom = fixture.module.atom_table.intern("run");
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Atom(hi),
            dst: Operand::X(1),
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(20),
        },
        Instr::Send,
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(20)),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::BuildStacktrace,
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let entry = fixture.add_function("run", 0, code);
    fixture.module.exports.push(Export {
        function: run_atom,
        arity: 0,
        label: entry,
    });
    (fixture.finish(), run_atom)
}

/// A mailbox that refuses **every** operation with the engine's typed `badarg` — the
/// shape a host reports for a send it cannot perform. Only `send_message` is reached
/// by the `send` route; the other eight exist so the seam is complete and a stray
/// receive fails loudly instead of silently answering "empty".
struct RefuseSend;

impl Mailbox for RefuseSend {
    fn send_message(&mut self, _to: Eterm, _message: Eterm) -> Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Err(BeamError::BadArg)
    }
    fn receive_skip(&mut self) -> Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_remove(&mut self) -> Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_reset(&mut self) -> Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_use(&mut self, _reference: Eterm) -> Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_clear(&mut self, _reference: Eterm) -> Result<()> {
        Err(BeamError::BadArg)
    }
}

// ---------------------------------------------------------------------------
// The measured table: the closed format-module arms
// ---------------------------------------------------------------------------

#[test]
fn binary_at_reports_the_stdlib_format_module() {
    // Oracle: `catch binary:at(<<1:1>>,0)` ->
    // `{binary,at,[<<1:1>>,0],[{error_info,#{module => erl_stdlib_errors}}]}`.
    // (`<<1:1>>` is not a `move`-spellable operand; the arm and the argument
    // positions are what this row pins — see the module docs.)
    let (module, run_atom) =
        call_ext_traced("binary", "at", 2, &[Reg::Int(1), Reg::Int(0)]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "binary",
        "at",
        &terms_of(&module, &[Reg::Int(1), Reg::Int(0)]),
        Some("erl_stdlib_errors"),
    );
}

#[test]
fn os_getenv_reports_the_kernel_format_module() {
    // Oracle: `catch os:getenv(1)` ->
    // `{os,getenv,[1],[{error_info,#{module => erl_kernel_errors}}]}`.
    let (module, run_atom) = call_ext_traced("os", "getenv", 1, &[Reg::Int(1)]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "os",
        "getenv",
        &terms_of(&module, &[Reg::Int(1)]),
        Some("erl_kernel_errors"),
    );
}

#[test]
fn math_log_reports_the_stdlib_format_module() {
    // Oracle: `catch math:log(a)` ->
    // `{math,log,[a],[{error_info,#{module => erl_stdlib_errors}}]}`. An ATOM
    // argument, so the item's argument list is a real term of the row.
    let (module, run_atom) = call_ext_traced("math", "log", 1, &[Reg::Atom("a")]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "math",
        "log",
        &terms_of(&module, &[Reg::Atom("a")]),
        Some("erl_stdlib_errors"),
    );
}

#[test]
fn re_run_reports_the_stdlib_format_module() {
    // Oracle: `catch re:run(1,2)` ->
    // `{re,run,[1,2],[{error_info,#{module => erl_stdlib_errors}}]}`.
    let (module, run_atom) = call_ext_traced("re", "run", 2, &[Reg::Int(1), Reg::Int(2)]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "re",
        "run",
        &terms_of(&module, &[Reg::Int(1), Reg::Int(2)]),
        Some("erl_stdlib_errors"),
    );
}

#[test]
fn erlang_binary_to_atom_reports_the_erts_format_module() {
    // Oracle: `catch erlang:binary_to_atom(x)` ->
    // `{erlang,binary_to_atom,[x],[{error_info,#{module => erl_erts_errors}}]}` — an
    // `erlang` BIF that is **not** one of the three raising BIFs, so it does get the
    // head item.
    let (module, run_atom) = call_ext_traced("erlang", "binary_to_atom", 1, &[Reg::Atom("x")]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "erlang",
        "binary_to_atom",
        &terms_of(&module, &[Reg::Atom("x")]),
        Some("erl_erts_errors"),
    );
}

#[test]
fn atomics_get_reports_the_erts_format_module() {
    // Oracle: `catch atomics:get(notaref,1)` ->
    // `{atomics,get,[notaref,1],[{error_info,#{module => erl_erts_errors}}]}`.
    let (module, run_atom) =
        call_ext_traced("atomics", "get", 2, &[Reg::Atom("notaref"), Reg::Int(1)]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "atomics",
        "get",
        &terms_of(&module, &[Reg::Atom("notaref"), Reg::Int(1)]),
        Some("erl_erts_errors"),
    );
}

#[test]
fn counters_get_reports_the_erts_format_module() {
    // Oracle: `catch counters:get(x,1)` ->
    // `{counters,get,[x,1],[{error_info,#{module => erl_erts_errors}}]}`.
    let (module, run_atom) = call_ext_traced("counters", "get", 2, &[Reg::Atom("x"), Reg::Int(1)]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "counters",
        "get",
        &terms_of(&module, &[Reg::Atom("x"), Reg::Int(1)]),
        Some("erl_erts_errors"),
    );
}

#[test]
fn records_get_gets_no_extension() {
    // Oracle: `catch records:get(1,2)` -> `{records,get,[1,2],[]}` — a module
    // outside the closed table gets NO `error_info`, so its location is `[]`.
    let (module, run_atom) = call_ext_traced("records", "get", 2, &[Reg::Int(1), Reg::Int(2)]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "records",
        "get",
        &terms_of(&module, &[Reg::Int(1), Reg::Int(2)]),
        None,
    );
    assert!(
        is_nil(tuple_element(list_elements(cooked).unwrap()[0], 3).unwrap()),
        "the location of a row outside the table is the empty list"
    );
}

// ---------------------------------------------------------------------------
// The measured table: a `function_clause` reason vs every other reason
// ---------------------------------------------------------------------------

#[test]
fn lists_max_cooks_a_function_clause_item_without_an_extension() {
    // Measured on the oracle with a compiled module:
    //   `lmax() -> lists:max([]).` caught ->
    //   `{error, function_clause, {lists,max,[[]],[{file,"lists.erl"},{line,638}]}}`.
    //
    // The extension is dropped because of the **reason**, not the module: C's only
    // producer of a bare `function_clause` is `func_info`, which names its own frame with
    // a source location and attaches no info. `lists` IS in the closed table, so a
    // non-`function_clause` raise out of the same module keeps its extension (the
    // `lists:seq/3` row below).
    //
    // No engine-side door can name a source location, so the honest shape is the real
    // argument list with an EMPTY location.
    let (module, run_atom) = call_ext_traced("lists", "max", 1, &[Reg::Nil]);
    let mut ctx = raising_ctx(
        ("lists", "max", 1),
        "function_clause",
        ExceptionClass::Error,
    );
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "lists",
        "max",
        &terms_of(&module, &[Reg::Nil]),
        None,
    );
}

#[test]
fn lists_seq_keeps_the_stdlib_extension() {
    // The row that settles the rule. Measured on the oracle (OTP 29.1.2):
    //   `try lists:seq(1,2,0) catch C:R:S -> hd(S) end` ->
    //   `{lists,seq,[1,2,0],[{file,"lists.erl"},{line,541},
    //                          {error_info,#{module => erl_stdlib_errors}}]}`
    //
    // `lists:seq/3` is a `beam_bifs` stdlib **fast path** — there is no `bif.tab` row —
    // but `lists.erl` raises through `error_with_info/2`, i.e. an Erlang-level
    // `erlang:error(Reason, Args, [{error_info, #{module => erl_stdlib_errors}}])`. The
    // extension is therefore the source body's own, not a BIF classification, and a fast
    // path keeps it exactly like a declared BIF. Only the source position is a residual.
    let (module, run_atom) = call_ext_traced(
        "lists",
        "seq",
        3,
        &[Reg::Int(1), Reg::Int(2), Reg::Int(0)],
    );
    let mut ctx = raising_ctx(("lists", "seq", 3), "badarg", ExceptionClass::Error);
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "lists",
        "seq",
        &terms_of(&module, &[Reg::Int(1), Reg::Int(2), Reg::Int(0)]),
        Some("erl_stdlib_errors"),
    );
}

#[test]
fn a_declared_bif_keeps_its_extension() {
    // The declared-BIF row, side by side with the fast-path one above: the same
    // `call_ext` route and the same `badarg` reason for `erlang:hd/1` (`ubif`, a real
    // `bif.tab` row). The item keeps the closed table's
    // `[{error_info, #{module => erl_erts_errors}}]` extension — on both tiers, with tier
    // agreement asserted first (`cooked_on_both_tiers`).
    let (module, run_atom) = call_ext_traced("erlang", "hd", 1, &[Reg::Int(1)]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "erlang",
        "hd",
        &terms_of(&module, &[Reg::Int(1)]),
        Some("erl_erts_errors"),
    );
}

// ---------------------------------------------------------------------------
// The measured table: the two non-`call_ext` routes
// ---------------------------------------------------------------------------

#[test]
fn a_send_failure_cooks_the_send_bif_item() {
    // Oracle: `catch (1 ! hi)` ->
    // `{erlang,send,[1,hi],[{error_info,#{module => erl_erts_errors}}]}` — the
    // canonical `send/2` name, never the `'!'/2` alias, with the destination and
    // message from `x(0)`/`x(1)`.
    let (module, run_atom) = send_traced();
    let mut ctx = badarg_ctx();
    ctx.set_mailbox(Box::new(RefuseSend));
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "erlang",
        "send",
        &terms_of(&module, &[Reg::Int(1), Reg::Atom("hi")]),
        Some("erl_erts_errors"),
    );
}

#[test]
fn apply_to_a_missing_function_keeps_the_callee_item_with_an_empty_location() {
    // Oracle: `catch apply(a,b,[1])` -> `{a,b,[1],[]}`: the `undef` head item names
    // the unresolvable callee with its argument list, and its `Location` stays NIL —
    // never an `error_info` extension (a callee that does not exist has no format
    // module, and `a` is outside the table anyway).
    let (module, run_atom) = apply_traced();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|module, function, arity, _args| {
        if (module, function, arity) == ("a", "b", 1) {
            return Err(undef_error());
        }
        Err(BeamError::BadArg)
    }));
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    let items = list_elements(cooked).expect("the cooked stacktrace is a proper list");
    let item = items[0];
    assert_eq!(tuple_element(item, 0).unwrap(), atom_of(&module, "a"));
    assert_eq!(tuple_element(item, 1).unwrap(), atom_of(&module, "b"));
    let expected_args = terms::make_list(&mut ctx, &[make_small(1)]).expect("the arena serves the list");
    assert!(
        cmp(
            tuple_element(item, 2).unwrap(),
            expected_args,
            &module.atom_table
        )
        .is_eq(),
        "the callee item carries the applied argument list"
    );
    assert_eq!(
        tuple_element(item, 3).unwrap(),
        NIL,
        "an `undef` head item keeps `Location == NIL`"
    );
}

/// `beam_rt`'s `error::undef()` shape: class `error`, the bare reason atom `undef`.
fn undef_error() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("undef")))
}

// ---------------------------------------------------------------------------
// The negatives: the rule-1 gate and the caller frame
// ---------------------------------------------------------------------------

#[test]
fn erlang_error_cooks_only_the_caller_frame() {
    // `erlang:error/1` is one of the three raising BIFs (C's `non_bif_stacktrace`
    // arm), so nothing may be prepended: the innermost item is the caller's own
    // frame, with the **arity** in its `A` slot (`run/0`, so `0`) and no extension.
    let (module, run_atom) = call_ext_traced("erlang", "error", 1, &[Reg::Int(1)]);
    let mut ctx = raising_ctx(("erlang", "error", 1), "foo", ExceptionClass::Error);
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_caller_frame(&module, cooked, make_small(0));
}

#[test]
fn erlang_exit_cooks_only_the_caller_frame() {
    let (module, run_atom) = call_ext_traced("erlang", "exit", 1, &[Reg::Int(1)]);
    let mut ctx = raising_ctx(("erlang", "exit", 1), "stop", ExceptionClass::Exit);
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_caller_frame(&module, cooked, make_small(0));
}

#[test]
fn erlang_throw_cooks_only_the_caller_frame() {
    let (module, run_atom) = call_ext_traced("erlang", "throw", 1, &[Reg::Int(1)]);
    let mut ctx = raising_ctx(("erlang", "throw", 1), "ball", ExceptionClass::Throw);
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_caller_frame(&module, cooked, make_small(0));
}

#[test]
fn erlang_error_2_arms_the_caller_frames_argument_list_and_extension() {
    // C's `non_bif_stacktrace` arm gives the **caller's** frame the exception's own
    // argument list (`is_list(args) || is_nil(args)` selects it in the `A` slot,
    // `utils.c:3449-3457`) and, for `EXC_ERROR_3`, the `error_info` term packed as
    // `{error_info, Map}` after the `{file,_}`/`{line,_}` pairs (`:1024-1026`). It is
    // exactly the rule-1 gate (no head item) that lets those latches arm — with a BIF
    // head item the extension would belong to the BIF's own item instead.
    //
    // The two terms are built in a **separate** arena the hook can capture (a hook
    // cannot borrow the run's context), and that arena is kept alive for the whole
    // test — the documented two-hosts rule for term comparisons.
    let (module, run_atom) = call_ext_traced("erlang", "error", 2, &[Reg::Int(1), Reg::Int(2)]);
    let mut arena = MinimalContext::new();
    let saved_args = terms::make_list(&mut arena, &[make_small(7)]).expect("the arena serves the list");
    let map = terms::make_flatmap(
        &mut arena,
        &[(
            atom_of(&module, "module"),
            atom_of(&module, "erl_erts_errors"),
        )],
    )
    .expect("the arena serves the map");
    let mut ctx = MinimalContext::with_bif_hook(Box::new(move |module, function, arity, _args| {
        if (module, function, arity) == ("erlang", "error", 2) {
            return Err(BeamError::Raise(
                Raise::error(ExitReason::atom("oops"))
                    .with_saved_args(LiveReason::new(saved_args))
                    .with_error_info(LiveReason::new(map)),
            ));
        }
        Err(BeamError::BadArg)
    }));
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    let items = list_elements(cooked).expect("the cooked stacktrace is a proper list");
    assert_eq!(items.len(), 1, "no BIF head item for erlang:error/2");
    let item = items[0];
    assert_eq!(tuple_element(item, 0).unwrap(), atom_of(&module, "m"));
    assert_eq!(
        tuple_element(item, 1).unwrap(),
        atom_of(&module, "run"),
        "the item is the caller's own frame"
    );
    let expected_args = terms::make_list(&mut ctx, &[make_small(7)]).expect("the arena serves the list");
    assert!(
        cmp(
            tuple_element(item, 2).unwrap(),
            expected_args,
            &module.atom_table
        )
        .is_eq(),
        "the caller frame's A slot is the exception's own argument list, not the arity"
    );
    // A synthetic module has no `Line` chunk, so the location is the bare extension
    // (C's `LINE_INVALID_LOCATION` arm prepends nothing to it).
    let location = list_elements(tuple_element(item, 3).unwrap()).expect("the location is a list");
    assert_eq!(
        location.len(),
        1,
        "no `{{file,_}}`/`{{line,_}}` pair in a synthetic module"
    );
    let extension = location[0];
    assert_eq!(
        tuple_element(extension, 0).unwrap(),
        atom_of(&module, "error_info")
    );
    let entries = crate::terms::map_entries(tuple_element(extension, 1).unwrap())
        .expect("the extension's value is a map");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        (entries[0].0, entries[0].1),
        (
            atom_of(&module, "module"),
            atom_of(&module, "erl_erts_errors")
        ),
        "the extension names the raising BIF's format module"
    );
}

#[test]
fn the_imports_declared_arity_names_the_argument_list() {
    // C's `make_arglist(c_p, reg, bif_mfa->arity)` uses the **import's** declared
    // arity (`beam_common.c:1062`), never the call site's operand — the split the
    // real `call_ext 3 erlang:apply/3` encoding relies on. This row makes the two
    // arities differ so the rule is observable: the import declares `hd/1` while the
    // instruction marshals two operands, and the item names exactly ONE argument.
    let (module, run_atom) =
        call_ext_traced("erlang", "hd", 1, &[Reg::Int(1), Reg::Int(2)]);
    let mut ctx = badarg_ctx();
    let cooked = cooked_on_both_tiers(&module, run_atom, &mut ctx);
    assert_head_item(
        &module,
        &mut ctx,
        cooked,
        "erlang",
        "hd",
        &terms_of(&module, &[Reg::Int(1)]),
        Some("erl_erts_errors"),
    );
    assert_eq!(
        list_elements(tuple_element(list_elements(cooked).unwrap()[0], 2).unwrap())
            .expect("the argument list is a proper list")
            .len(),
        1,
        "the argument list follows the import's declared arity, not the instruction's"
    );
}

#[test]
fn a_guard_failure_arms_no_head_item() {
    // The same `bif` instruction with a real fail label is a **guard branch**: it
    // takes its fail label and never raises, so no item may be armed and the cooked
    // path is never reached. (The `call_ext` route carries no fail label, so this is
    // the `bif`/`gc_bif` shape.)
    let mut fixture = Fixture::new(ROW_ATOMS);
    let erlang = fixture.module.atom_table.intern("erlang");
    let hd = fixture.module.atom_table.intern("hd");
    let failed = fixture.module.atom_table.intern("failed");
    let run_atom = fixture.module.atom_table.intern("run");
    fixture.module.imports.push(Import {
        module: erlang,
        function: hd,
        arity: 1,
    });
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(20),
        },
        Instr::Bif {
            arity: 1,
            bif: ImportRef(0),
            fail: Label(20),
            args: vec![Operand::X(0)],
            dst: Operand::X(0),
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(20)),
        Instr::Move {
            src: Operand::Atom(failed),
            dst: Operand::X(0),
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let entry = fixture.add_function("run", 0, code);
    fixture.module.exports.push(Export {
        function: run_atom,
        arity: 0,
        label: entry,
    });
    let module = fixture.finish();
    let mut ctx = badarg_ctx();
    let mut machine = Machine::new();
    let value = machine
        .run_export(&module, run_atom, 0, &[], &mut ctx)
        .expect("the guard branch answers");
    assert_eq!(
        value,
        atom_of(&module, "failed"),
        "a guard failure takes its fail label"
    );
    assert!(
        !machine.pending_callee_head,
        "no head item may be armed by a guard failure"
    );
}
