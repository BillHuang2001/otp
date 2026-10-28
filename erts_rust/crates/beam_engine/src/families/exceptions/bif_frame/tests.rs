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

//! The synthesized **BIF head item** ([`super`]): the closed format-module table,
//! the rule-1 gate, the item's shape (real argument list + `{error_info, Map}`
//! location extension), every decline, and the end-to-end cooked trace on **both**
//! tiers.

use super::*;

use std::collections::BTreeMap;

use beam_common::BeamError;
use beam_loader::{Atom, Export, Function, HeapNeed, Import, ImportRef, Instr, Label, NO_FAIL};
use beam_term::{cmp, make_small};

use crate::context::MinimalContext;
use crate::jit::Tier;
use crate::machine::CatchFrame;
use crate::terms::test_support as terms;
use crate::terms::{list_elements, tuple_element};

/// The names a fixture interns: every BIF module/function a test asserts on, the
/// extension's own names, and the two atoms a landing raise needs (`badarg`,
/// `error`).
const TABLE_ATOMS: &[&str] = &[
    "erlang",
    "hd",
    "binary",
    "at",
    "os",
    "getenv",
    "records",
    "get",
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
    "run",
];

/// A module whose own atom table holds [`TABLE_ATOMS`] (and `m`).
fn fixture() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    for name in TABLE_ATOMS {
        module.atom_table.intern(name);
    }
    module
}

/// The atom `name` **as an immediate**, via the fixture's table.
///
/// `pub(crate)` for the sibling route suites (`tests/error_info.rs`), which build
/// the same expected item on a module of their own.
pub(crate) fn atom_of(module: &Module, name: &str) -> Eterm {
    make_atom(
        module
            .atom_table
            .lookup(name)
            .unwrap_or_else(|| panic!("the fixture interns {name}")),
    )
}

/// A machine holding one live catch frame at `(func=0, pc=0)` owned by frame 0.
fn machine_with_a_catch_frame() -> Machine {
    let mut machine = Machine::new();
    machine.push_frame(0).expect("the bottom frame");
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    machine
}

/// The oracle's own item for `module:function(args…)`, built independently of the
/// production builder from already-materialized terms (the test arena never
/// collects, so the operand/expected split needs no rooting).
///
/// `pub(crate)` so `tests/error_info.rs` asserts the very same shape the unit
/// suite does — one definition, so the two cannot drift.
pub(crate) fn expected_item(
    ctx: &mut MinimalContext,
    module: &Module,
    bif_module: &str,
    bif_function: &str,
    args: &[Eterm],
    format_module: Option<&str>,
) -> Eterm {
    let location = match format_module {
        None => NIL,
        Some(format) => {
            let map = terms::make_flatmap(
                ctx,
                &[(atom_of(module, "module"), atom_of(module, format))],
            )
            .unwrap();
            let pair = terms::make_tuple(ctx, &[atom_of(module, "error_info"), map]).unwrap();
            terms::make_list(ctx, &[pair]).unwrap()
        }
    };
    let arg_list = terms::make_list(ctx, args).unwrap();
    terms::make_tuple(
        ctx,
        &[
            atom_of(module, bif_module),
            atom_of(module, bif_function),
            arg_list,
            location,
        ],
    )
    .unwrap()
}

/// Run [`bif_head_item`] for `bif_module:bif_function` over `Operand::X(0..n)` with
/// the registers set to `values`.
fn head_item(
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    module: &Module,
    bif_module: &str,
    bif_function: &str,
    values: &[i64],
) -> Option<Eterm> {
    for (i, value) in values.iter().enumerate() {
        machine.set_x(i as u32, make_small(*value)).unwrap();
    }
    let args: Vec<Operand> = (0..values.len()).map(|i| Operand::X(i as u32)).collect();
    bif_head_item(machine, ctx, module, bif_module, bif_function, &args)
        .expect("the arena never refuses a reservation")
}

// ---------------------------------------------------------------------------
// the closed format-module table
// ---------------------------------------------------------------------------

#[test]
fn format_module_names_the_three_error_modules() {
    // `../../erts/emulator/beam/beam_common.c:937-1006`, arm by arm.
    for name in ["atomics", "counters", "erlang", "erts_internal", "persistent_term"] {
        assert_eq!(format_module(name), Some("erl_erts_errors"), "{name}");
    }
    for name in ["code", "os"] {
        assert_eq!(format_module(name), Some("erl_kernel_errors"), "{name}");
    }
    for name in ["binary", "ets", "lists", "maps", "math", "re", "unicode"] {
        assert_eq!(format_module(name), Some("erl_stdlib_errors"), "{name}");
    }
}

#[test]
fn format_module_has_no_answer_outside_the_table() {
    // C's `default:` arm: the item's Location is the empty list. Oracle-measured for
    // `records:get/2` (`{records,get,[1,2],[]}`), and the same for the rest of the
    // `bif.tab` modules outside the table (`erts_debug`, `erl_ddll`, `prim_file`, …).
    for name in ["records", "erts_debug", "erl_ddll", "prim_file", "string", "io"] {
        assert_eq!(format_module(name), None, "{name}");
    }
}

#[test]
fn the_rule_1_gate_excludes_only_the_raising_bifs() {
    // C's `non_bif_stacktrace` (:911-922): exactly `erlang:error/1,2,3`,
    // `erlang:exit/1` and `erlang:throw/1`.
    for arity in 1..=3 {
        assert!(is_non_bif_raise("erlang", "error", arity), "erlang:error/{arity}");
    }
    assert!(is_non_bif_raise("erlang", "exit", 1));
    assert!(is_non_bif_raise("erlang", "throw", 1));
    // A different arity, a different `erlang` BIF, and the same names in another
    // module all DO get the head item.
    assert!(!is_non_bif_raise("erlang", "error", 4));
    assert!(!is_non_bif_raise("erlang", "error", 0));
    assert!(!is_non_bif_raise("erlang", "exit", 2));
    assert!(!is_non_bif_raise("erlang", "throw", 2));
    assert!(!is_non_bif_raise("erlang", "hd", 1));
    assert!(!is_non_bif_raise("erlang", "map_get", 2));
    assert!(!is_non_bif_raise("records", "get", 2));
}

// ---------------------------------------------------------------------------
// the item builder
// ---------------------------------------------------------------------------

#[test]
fn the_item_names_the_bif_with_its_real_arguments_and_the_extension() {
    // Oracle: `catch hd(1)` ->
    // `{erlang,hd,[1],[{error_info,#{module => erl_erts_errors}}]}`.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    let item = head_item(&mut machine, &mut ctx, &module, "erlang", "hd", &[1])
        .expect("a live catch frame and a nameable BIF synthesize the item");
    let expected = expected_item(&mut ctx, &module, "erlang", "hd", &[make_small(1)], Some("erl_erts_errors"));
    assert!(
        cmp(item, expected, &module.atom_table).is_eq(),
        "the item must be {{erlang,hd,[1],[{{error_info,#{{module => erl_erts_errors}}}}]}}"
    );
}

#[test]
fn the_extension_names_the_stdlib_module_for_binary() {
    // Oracle: `catch binary:at(<<1:1>>,0)` ->
    // `{binary,at,[<<1:1>>,0],[{error_info,#{module => erl_stdlib_errors}}]}`.
    // (Reached through `call_ext` in compiled code — see the module docs — but the
    // table arm and the item shape are the same builder.)
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    let item = head_item(&mut machine, &mut ctx, &module, "binary", "at", &[1, 0]).unwrap();
    let expected = expected_item(
        &mut ctx,
        &module,
        "binary",
        "at",
        &[make_small(1), make_small(0)],
        Some("erl_stdlib_errors"),
    );
    assert!(cmp(item, expected, &module.atom_table).is_eq());
}

#[test]
fn the_extension_names_the_kernel_module_for_os() {
    // Oracle: `catch os:getenv(1)` -> `{os,getenv,[1],[{error_info,#{module => erl_kernel_errors}}]}`.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    let item = head_item(&mut machine, &mut ctx, &module, "os", "getenv", &[1]).unwrap();
    let expected = expected_item(
        &mut ctx,
        &module,
        "os",
        "getenv",
        &[make_small(1)],
        Some("erl_kernel_errors"),
    );
    assert!(cmp(item, expected, &module.atom_table).is_eq());
}

#[test]
fn a_module_outside_the_table_gets_no_extension() {
    // Oracle: `catch records:get(1,2)` -> `{records,get,[1,2],[]}` — the real
    // arguments, and an EMPTY location.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    let item = head_item(&mut machine, &mut ctx, &module, "records", "get", &[1, 2]).unwrap();
    let expected = expected_item(
        &mut ctx,
        &module,
        "records",
        "get",
        &[make_small(1), make_small(2)],
        None,
    );
    assert!(cmp(item, expected, &module.atom_table).is_eq());
    assert_eq!(
        tuple_element(item, 3).unwrap(),
        NIL,
        "no format module means the empty-list Location"
    );
}

#[test]
fn a_zero_arity_bif_cooks_an_empty_argument_list() {
    // C's `make_arglist(c_p, reg, 0)` is `NIL`: the `A` slot is `[]`, never an arity.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    let item = head_item(&mut machine, &mut ctx, &module, "erlang", "hd", &[]).unwrap();
    assert_eq!(
        tuple_element(item, 2).unwrap(),
        NIL,
        "a 0-arity BIF's ArgList is the empty list"
    );
}

// ---------------------------------------------------------------------------
// the declines
// ---------------------------------------------------------------------------

#[test]
fn the_item_is_declined_without_a_live_catch_frame() {
    // No live catch frame: the raise cannot land, the cooked trace is never
    // reached, and the uncaught path derives its frames from the raise snapshot.
    let module = fixture();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.set_x(0, make_small(1)).unwrap();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        bif_head_item(&mut machine, &mut ctx, &module, "erlang", "hd", &[Operand::X(0)]).unwrap(),
        None
    );
}

#[test]
fn the_item_is_declined_for_the_raising_bifs() {
    // `erlang:error/1,2,3`, `erlang:exit/1` and `erlang:throw/1`: the caller's own
    // frame is the innermost one (`error/2,3`'s args and `error_info` ride it), so no
    // BIF item may be prepended.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    for (function, values) in [
        ("error", vec![1]),
        ("error", vec![1, 2]),
        ("error", vec![1, 2, 3]),
        ("exit", vec![1]),
        ("throw", vec![1]),
    ] {
        assert_eq!(
            head_item(&mut machine, &mut ctx, &module, "erlang", function, &values),
            None,
            "erlang:{function}/{} must not get a head item",
            values.len()
        );
    }
}

#[test]
fn the_item_is_declined_when_a_name_cannot_be_resolved() {
    // A module whose snapshot holds neither the BIF's names nor the extension's, and
    // a context with no live namespace: nothing is fabricated (an atom index is only
    // valid against the table it was read with).
    let module = Module::default();
    let mut machine = machine_with_a_catch_frame();
    machine.set_x(0, make_small(1)).unwrap();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        bif_head_item(&mut machine, &mut ctx, &module, "erlang", "hd", &[Operand::X(0)]).unwrap(),
        None
    );
}

#[test]
fn the_item_is_declined_when_an_argument_operand_is_unreadable() {
    // `args` comes from the instruction, so an out-of-range register is a malformed
    // instruction — not a runtime value: the raise stays unmodified (the BIF's own
    // error is never replaced by a marshalling one).
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        bif_head_item(
            &mut machine,
            &mut ctx,
            &module,
            "erlang",
            "hd",
            &[Operand::X(crate::machine::MAX_REGISTER as u32)],
        )
        .unwrap(),
        None
    );
}

// ---------------------------------------------------------------------------
// end to end: the cooked trace, on both tiers
// ---------------------------------------------------------------------------

/// A tiny module builder with an owned atom table (the same shape the sibling
/// exception test modules use, kept local so this module stands alone).
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new() -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
        for name in TABLE_ATOMS {
            module.atom_table.intern(name);
        }
        Fixture { module }
    }

    /// Append a function whose code starts with `entry`; returns its label.
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

    fn finish(self) -> Module {
        self.module
    }
}

/// `run/0` that calls `bif_module:bif_function` with `n` small-int arguments inside a
/// `try … catch` and returns the cooked `catch` stacktrace.
///
/// `fail` is the BIF instruction's own failure label: [`NO_FAIL`] makes a BIF error a
/// **raise**, a real label makes it a guard branch (the same instruction, two
/// semantics — the gate must arm nothing on the second).
fn traced_bif_call(bif_module: &str, bif_function: &str, n: u8, fail: Label) -> (Module, Atom) {
    let mut fixture = Fixture::new();
    let bif_module = fixture.module.atom_table.intern(bif_module);
    let bif_function = fixture.module.atom_table.intern(bif_function);
    let run_atom = fixture.module.atom_table.intern("run");
    fixture.module.imports.push(Import {
        module: bif_module,
        function: bif_function,
        arity: u32::from(n),
    });
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
    ];
    for (i, value) in (1..=n).map(i64::from).enumerate() {
        code.push(Instr::Move {
            src: Operand::Integer(value),
            dst: Operand::X(i as u32),
        });
    }
    let args: Vec<Operand> = (0..u32::from(n)).map(Operand::X).collect();
    code.extend([
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(20),
        },
        Instr::Bif {
            arity: n,
            bif: ImportRef(0),
            fail,
            args,
            dst: Operand::X(0),
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ]);
    if fail == NO_FAIL {
        // The raise arm: the handler returns the cooked trace.
        code.extend([
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
    } else {
        // The guard-failure arm: the fail label just answers `failed`.
        code.extend([
            Instr::Label(Label(20)),
            Instr::Move {
                src: Operand::Atom(fixture.module.atom_table.intern("failed")),
                dst: Operand::X(0),
            },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
        ]);
    }
    let entry = fixture.add_function("run", 0, code);
    fixture.module.exports.push(Export {
        function: run_atom,
        arity: 0,
        label: entry,
    });
    (fixture.finish(), run_atom)
}

/// A context whose only BIF answer is `badarg` for everything (the shape a host
/// returns for a wrongly-typed argument).
fn badarg_ctx() -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(|_, _, _, _| Err(BeamError::BadArg)))
}

#[test]
fn a_bif_failure_cooks_the_bif_head_item_on_both_tiers() {
    let (module, run_atom) = traced_bif_call("erlang", "hd", 1, NO_FAIL);
    let mut ctx = badarg_ctx();
    let mut interpreter = Machine::new();
    let cooked = interpreter
        .run_export(&module, run_atom, 0, &[], &mut ctx)
        .expect("the handler returns the cooked trace");
    let mut jit = Machine::new();
    let cooked_jit = jit
        .run_with_tier(&module, run_atom, 0, &[], &mut ctx, Tier::BaselineJit)
        .expect("the handler returns the cooked trace on the native tier too");

    // Tier agreement (before either side is compared with the oracle): a mistake
    // both tiers share still has to fail somewhere else.
    assert!(
        cmp(cooked, cooked_jit, &module.atom_table).is_eq(),
        "the interpreter and the baseline tier must cook the same trace"
    );

    let items = list_elements(cooked).unwrap();
    assert_eq!(items.len(), 2, "the BIF head item plus the raising frame");
    let expected = expected_item(
        &mut ctx,
        &module,
        "erlang",
        "hd",
        &[make_small(1)],
        Some("erl_erts_errors"),
    );
    assert!(
        cmp(items[0], expected, &module.atom_table).is_eq(),
        "the innermost item must be the BIF's {{erlang,hd,[1],[{{error_info,·}}]}}"
    );
    assert_eq!(
        tuple_element(items[1], 1).unwrap(),
        atom_of(&module, "run"),
        "then the raising frame"
    );
}

#[test]
fn a_bif_outside_the_table_cooks_an_item_without_the_extension() {
    // Oracle: `catch records:get(1,2)` -> `{records,get,[1,2],[]}`.
    let (module, run_atom) = traced_bif_call("records", "get", 2, NO_FAIL);
    let mut ctx = badarg_ctx();
    let mut machine = Machine::new();
    let cooked = machine
        .run_export(&module, run_atom, 0, &[], &mut ctx)
        .expect("the handler returns the cooked trace");
    let items = list_elements(cooked).unwrap();
    let expected = expected_item(
        &mut ctx,
        &module,
        "records",
        "get",
        &[make_small(1), make_small(2)],
        None,
    );
    assert!(cmp(items[0], expected, &module.atom_table).is_eq());
}

#[test]
fn the_raising_bifs_cook_only_the_caller_frame() {
    // `erlang:error/1` raised through the raise arm: the oracle's innermost item is
    // the caller's own frame, so nothing may be prepended (the frame count is 1).
    let (module, run_atom) = traced_bif_call("erlang", "error", 1, NO_FAIL);
    let mut ctx = badarg_ctx();
    let mut machine = Machine::new();
    let cooked = machine
        .run_export(&module, run_atom, 0, &[], &mut ctx)
        .expect("the handler returns the cooked trace");
    let items = list_elements(cooked).unwrap();
    assert_eq!(items.len(), 1, "no BIF head item for erlang:error/1");
    assert_eq!(
        tuple_element(items[0], 1).unwrap(),
        atom_of(&module, "run"),
        "the caller's own function is the innermost frame"
    );
}

#[test]
fn a_guard_failure_arms_no_head_item() {
    // The same `bif` instruction with a real fail label is a guard branch: it takes
    // the label and never raises, so nothing may be armed.
    let (module, run_atom) = traced_bif_call("erlang", "hd", 1, Label(20));
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
