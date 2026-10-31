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

use beam_common::{BeamError, ExitReason, Raise};
use beam_loader::{Atom, Export, Function, HeapNeed, Import, ImportRef, Instr, Label, NO_FAIL};
use beam_term::{cmp, make_small, AtomTable};

use crate::context::{MinimalContext, RootSet};
use crate::families::exceptions::stacktrace::symbolic_stacktrace;
use crate::families::exceptions::tests::oracle::render_error_info_extension;
use crate::jit::Tier;
use crate::machine::CatchFrame;
use crate::step::Step;
use crate::terms::alloc_tests::MovingContext;
use crate::terms::{
    cons_head, cons_tail, list_elements, make_cons_rooted, make_list_rooted, make_map_rooted,
    make_tuple_rooted, tuple_element,
};
use crate::terms::test_support as terms;

/// The names a fixture interns: every BIF module/function a test asserts on, the
/// extension's own names, and the two atoms a landing raise needs (`badarg`,
/// `error`).
const TABLE_ATOMS: &[&str] = &[
    "erlang",
    "hd",
    "lists",
    "max",
    "seq",
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
    "ets",
    "lookup",
    "cause",
    "id",
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
/// the registers set to `values`, for a `badarg` failure (the shape a host returns
/// for a wrongly-typed argument — and one whose reason is not `function_clause`, so
/// the extension's own rules are what a row observes).
fn head_item(
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    module: &Module,
    bif_module: &str,
    bif_function: &str,
    values: &[i64],
) -> Option<Eterm> {
    head_item_for(machine, ctx, module, bif_module, bif_function, values, &BeamError::BadArg)
}

/// [`head_item`] with an explicit failure — the knob the shape-rule rows turn.
fn head_item_for(
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    module: &Module,
    bif_module: &str,
    bif_function: &str,
    values: &[i64],
    error: &BeamError,
) -> Option<Eterm> {
    for (i, value) in values.iter().enumerate() {
        machine.set_x(i as u32, make_small(*value)).unwrap();
    }
    let args: Vec<Operand> = (0..values.len()).map(|i| Operand::X(i as u32)).collect();
    bif_head_item(machine, ctx, module, bif_module, bif_function, &args, error)
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
// a structured argument survives a collecting allocation (regression)
// ---------------------------------------------------------------------------

#[test]
fn a_boxed_argument_in_the_head_item_is_self_contained() {
    // Regression: `carve` used to store the post-reservation argument terms verbatim
    // in the item's `ArgList`, so the item's argument-list cells named the argument's
    // *own* heap block rather than a copy that lives inside the item's reservation.
    // `MovingContext` is a conservative collector (it traces and rebases any word that
    // points into a live block), so it keeps such a verbatim pointer alive — which is
    // exactly what makes the two paths distinguishable: the verbatim path keeps the
    // `ArgList` element *identical* to the relocated original argument, while the
    // deep-copy path makes it a distinct, self-contained object that moves **with**
    // the item.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MovingContext::new();

    // A boxed argument — `{1, 2}` — parked in x(0). `MovingContext` collects on every
    // rooted allocation, so each builder below runs a real collection first.
    let arg = make_tuple_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 2, 0, |_, _| {
        Ok(vec![make_small(1), make_small(2)])
    })
    .expect("the moving arena serves the first rooted allocation");
    machine.set_x(0, arg).expect("x(0) is a live register");

    // Building the item reserves once — collecting the argument block into the other
    // semi-space — and then deep-copies the re-read argument into that reservation.
    let item = bif_head_item(
        &mut machine,
        &mut ctx,
        &module,
        "erlang",
        "hd",
        &[Operand::X(0)],
        &BeamError::BadArg,
    )
    .expect("the arena serves the item reservation")
    .expect("a live catch frame and a nameable BIF synthesize the item");

    // Park the item in a root slot; the original argument stays rooted in x(0) so we
    // can compare the `ArgList` element's identity against the relocated original.
    machine.set_x(1, item).expect("x(1) is a live register");

    // Force a SECOND collecting allocation after the item is built: both the rooted
    // original argument and the rooted item block are relocated.
    let _ = make_tuple_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 1, 0, |_, _| {
        Ok(vec![make_small(99)])
    })
    .expect("the moving arena serves the second rooted allocation");

    assert!(ctx.moved_any, "the collector must have relocated something");
    assert!(ctx.collections >= 3, "the second collection must really have run");

    let item = machine.x(1).expect("the item is rooted");
    let original = machine.x(0).expect("the original argument is rooted");
    let arg_list = tuple_element(item, 2).expect("the item's ArgList");
    let args = list_elements(arg_list).expect("a proper argument list");
    assert_eq!(args.len(), 1, "hd/1 names exactly one argument");

    // The `ArgList` names the item's own deep copy, never the original argument's
    // block: the copy is self-contained inside the item's reservation, so it moves
    // with the item instead of riding a pointer the next collection relocates.
    assert_ne!(
        args[0], original,
        "the ArgList must name a self-contained copy, not the original argument block"
    );

    // The relocated copy still reads as `{1, 2}`.
    assert_eq!(
        tuple_element(args[0], 0).expect("field 0"),
        make_small(1),
        "the relocated argument's first field"
    );
    assert_eq!(
        tuple_element(args[0], 1).expect("field 1"),
        make_small(2),
        "the relocated argument's second field"
    );
}

#[test]
fn a_proper_list_argument_in_the_head_item_is_self_contained() {
    // Same regression family as `a_boxed_argument_in_the_head_item_is_self_contained`,
    // but the argument is a *proper list*: its cons spine must be deep-copied into the
    // item's reservation and re-pointed cell by cell, so the `ArgList` element reads
    // `[1, 2, 3]` after every collection instead of naming the original argument's
    // spine (whose `CDR` links the collector rebases independently).
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MovingContext::new();

    let arg = make_list_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 3, 0, |_, _| {
        Ok(vec![make_small(1), make_small(2), make_small(3)])
    })
    .expect("the moving arena serves the first rooted allocation");
    machine.set_x(0, arg).expect("x(0) is a live register");

    let item = bif_head_item(
        &mut machine,
        &mut ctx,
        &module,
        "erlang",
        "hd",
        &[Operand::X(0)],
        &BeamError::BadArg,
    )
    .expect("the arena serves the item reservation")
    .expect("a live catch frame and a nameable BIF synthesize the item");
    machine.set_x(1, item).expect("x(1) is a live register");

    let _ = make_tuple_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 1, 0, |_, _| {
        Ok(vec![make_small(99)])
    })
    .expect("the moving arena serves the second rooted allocation");

    assert!(ctx.moved_any, "the collector must have relocated something");
    assert!(ctx.collections >= 3, "the second collection must really have run");

    let item = machine.x(1).expect("the item is rooted");
    let original = machine.x(0).expect("the original argument is rooted");
    let arg_list = tuple_element(item, 2).expect("the item's ArgList");
    let args = list_elements(arg_list).expect("a proper argument list");
    assert_eq!(args.len(), 1, "hd/1 names exactly one argument");

    assert_ne!(
        args[0], original,
        "the ArgList must name a self-contained copy, not the original argument spine"
    );
    assert_eq!(
        list_elements(args[0]).expect("the copied argument is a proper list"),
        vec![make_small(1), make_small(2), make_small(3)],
        "the relocated argument's elements"
    );
}

#[test]
fn an_improper_list_argument_in_the_head_item_is_self_contained() {
    // An improper list (`[1 | 2]`) exercises the list arm's non-list `CDR` branch:
    // the head is deep-copied as an immediate, the tail is shared verbatim, and the
    // one cons cell still moves with the item.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MovingContext::new();

    let arg = make_cons_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 0, |_, _| {
        Ok((make_small(1), make_small(2)))
    })
    .expect("the moving arena serves the first rooted allocation");
    machine.set_x(0, arg).expect("x(0) is a live register");

    let item = bif_head_item(
        &mut machine,
        &mut ctx,
        &module,
        "erlang",
        "hd",
        &[Operand::X(0)],
        &BeamError::BadArg,
    )
    .expect("the arena serves the item reservation")
    .expect("a live catch frame and a nameable BIF synthesize the item");
    machine.set_x(1, item).expect("x(1) is a live register");

    let _ = make_tuple_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 1, 0, |_, _| {
        Ok(vec![make_small(99)])
    })
    .expect("the moving arena serves the second rooted allocation");

    assert!(ctx.moved_any, "the collector must have relocated something");
    assert!(ctx.collections >= 3, "the second collection must really have run");

    let item = machine.x(1).expect("the item is rooted");
    let original = machine.x(0).expect("the original argument is rooted");
    let arg_list = tuple_element(item, 2).expect("the item's ArgList");
    let args = list_elements(arg_list).expect("a proper argument list");
    assert_eq!(args.len(), 1, "hd/1 names exactly one argument");

    assert_ne!(
        args[0], original,
        "the ArgList must name a self-contained copy, not the original argument spine"
    );
    assert_eq!(
        cons_head(args[0]).expect("the copied argument's CAR"),
        make_small(1),
        "the relocated argument's head"
    );
    assert_eq!(
        cons_tail(args[0]).expect("the copied argument's CDR"),
        make_small(2),
        "the relocated argument's improper tail"
    );
}

#[test]
fn a_list_nested_in_a_tuple_and_map_argument_is_self_contained() {
    // A list reached *through* a boxed parent — here `{[1, 2], #{error => [3, 4]}}` —
    // must be re-pointed from the parent's copied child slot, not just at the top
    // level. The boxed copy arm walks every child slot and deep-copies each child, so
    // both the tuple element and the map value must read as lists after collection.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MovingContext::new();

    let list_a = make_list_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 2, 0, |_, _| {
        Ok(vec![make_small(1), make_small(2)])
    })
    .expect("list A");
    machine.set_x(10, list_a).expect("x(10) roots list A");

    let list_b = make_list_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 2, 0, |_, _| {
        Ok(vec![make_small(3), make_small(4)])
    })
    .expect("list B");
    machine.set_x(11, list_b).expect("x(11) roots list B");

    let map = make_map_rooted(
        &mut machine,
        &module,
        &mut ctx,
        RootSet::ALL_LIVE,
        1,
        0,
        |machine, _slack, _atoms| {
            Ok(vec![(
                atom_of(&module, "error"),
                machine.x(11).expect("list B is rooted"),
            )])
        },
    )
    .expect("the map");
    machine.set_x(12, map).expect("x(12) roots the map");

    let arg = make_tuple_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 2, 0, |machine, _| {
        Ok(vec![
            machine.x(10).expect("list A is rooted"),
            machine.x(12).expect("the map is rooted"),
        ])
    })
    .expect("the tuple argument");
    machine.set_x(0, arg).expect("x(0) is a live register");

    let item = bif_head_item(
        &mut machine,
        &mut ctx,
        &module,
        "erlang",
        "hd",
        &[Operand::X(0)],
        &BeamError::BadArg,
    )
    .expect("the arena serves the item reservation")
    .expect("a live catch frame and a nameable BIF synthesize the item");
    machine.set_x(1, item).expect("x(1) is a live register");

    let _ = make_tuple_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 1, 0, |_, _| {
        Ok(vec![make_small(99)])
    })
    .expect("the moving arena serves the second rooted allocation");

    assert!(ctx.moved_any, "the collector must have relocated something");
    assert!(ctx.collections >= 4, "several collections must really have run");

    let item = machine.x(1).expect("the item is rooted");
    let original = machine.x(0).expect("the original argument is rooted");
    let arg_list = tuple_element(item, 2).expect("the item's ArgList");
    let args = list_elements(arg_list).expect("a proper argument list");
    assert_eq!(args.len(), 1, "hd/1 names exactly one argument");

    assert_ne!(
        args[0], original,
        "the ArgList must name a self-contained copy, not the original argument block"
    );

    let copied_list = tuple_element(args[0], 0).expect("the tuple's list element");
    assert_eq!(
        list_elements(copied_list).expect("the copied tuple element is a proper list"),
        vec![make_small(1), make_small(2)],
        "the relocated nested list's elements"
    );

    let copied_map = tuple_element(args[0], 1).expect("the tuple's map element");
    let value = beam_term::map::map_get(
        copied_map,
        atom_of(&module, "error"),
        &module.atom_table,
    )
    .expect("map_get resolves the copied map")
    .expect("the key is present");
    assert_eq!(
        list_elements(value).expect("the map value is a proper list"),
        vec![make_small(3), make_small(4)],
        "the relocated map value's elements"
    );
}

// ---------------------------------------------------------------------------
// the landed head item survives the cooked trace's reservation (regression)
// ---------------------------------------------------------------------------

#[test]
fn a_landed_bif_head_item_survives_the_cooked_trace_reservation() {
    // Regression: part (a) made `carve`'s `ArgList` self-contained, but the finished
    // head item is still landed raw as the raise's `x(2)` trace word and later
    // re-embedded by `symbolic_stacktrace` with only that register as a root. Drive
    // the full caught raise path — `carve` → `raise_bif_error_with_head` (the landing)
    // → `symbolic_stacktrace` (the cooked-trace reservation + re-embedding) — under
    // `MovingContext`, which collects on **every** rooted allocation, and then force
    // one more collection with the cooked trace parked in `x(0)`. The head item (and
    // its relocated `ArgList`) must still read back intact.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MovingContext::new();

    // A boxed argument `{1, 2}` in x(0), rooted across every collection below.
    let arg = make_tuple_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 2, 0, |_, _| {
        Ok(vec![make_small(1), make_small(2)])
    })
    .expect("the moving arena serves the argument reservation");
    machine.set_x(0, arg).expect("x(0) is a live register");

    // Building the item reserves once (collecting the argument block) and deep-copies
    // the re-read argument into that reservation.
    let head = bif_head_item(
        &mut machine,
        &mut ctx,
        &module,
        "erlang",
        "hd",
        &[Operand::X(0)],
        &BeamError::BadArg,
    )
    .expect("the arena serves the item reservation")
    .expect("a live catch frame and a nameable BIF synthesize the item");

    // Land the raise through the real funnel: this consumes the one-shot head latch
    // and publishes the head item as the handler's raw trace in x(2).
    let step = machine
        .raise_bif_error_with_head(&module, BeamError::BadArg, Some(head), &mut ctx)
        .expect("the raise lands in the live catch frame");
    assert!(
        matches!(step, Step::Goto { .. }),
        "the landing transfers to the handler"
    );

    // The handler cooks the trace: `symbolic_stacktrace` reserves once (collecting the
    // x(2) root and relocating the head item's whole graph) and conses the re-read
    // head word in front of the (empty) snapshot list.
    let cooked = symbolic_stacktrace(&mut machine, &module, &mut ctx, &Operand::X(2), true)
        .expect("the cooked trace reservation succeeds");

    // Park the cooked trace in a root slot and force a third collection, so the
    // `[Item]` list and everything it names must be relocated one more time.
    machine.set_x(0, cooked).expect("x(0) is a live register");
    let _ = make_tuple_rooted(&mut machine, &mut ctx, RootSet::ALL_LIVE, 1, 0, |_, _| {
        Ok(vec![make_small(99)])
    })
    .expect("the moving arena serves the follow-up reservation");

    assert!(ctx.moved_any, "the collector must have relocated something");
    assert!(ctx.collections >= 3, "the follow-up collection must really have run");

    let cooked = machine.x(0).expect("the cooked trace is rooted in x(0)");
    let items = list_elements(cooked).expect("the cooked trace is a proper list");
    assert_eq!(items.len(), 1, "the head item plus an empty snapshot list");

    let item = items[0];
    assert_eq!(
        tuple_element(item, 0).expect("the item's module"),
        atom_of(&module, "erlang"),
        "the relocated item names the failing BIF's module"
    );
    assert_eq!(
        tuple_element(item, 1).expect("the item's function"),
        atom_of(&module, "hd"),
        "the relocated item names the failing BIF"
    );
    let arg_list = tuple_element(item, 2).expect("the item's ArgList");
    let args = list_elements(arg_list).expect("a proper argument list");
    assert_eq!(args.len(), 1, "hd/1 names exactly one argument");
    assert_eq!(
        tuple_element(args[0], 0).expect("field 0"),
        make_small(1),
        "the relocated argument's first field"
    );
    assert_eq!(
        tuple_element(args[0], 1).expect("field 1"),
        make_small(2),
        "the relocated argument's second field"
    );
}


// ---------------------------------------------------------------------------
// the shape rule: only a DECLARED BIF gets the `error_info` extension
// ---------------------------------------------------------------------------

/// `function_clause` — C's `func_info`-only reason (no declared BIF raises it).
fn function_clause_error() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("function_clause")))
}

#[test]
fn a_function_clause_raise_carries_no_extension_even_for_a_tabled_module() {
    // The bare `function_clause` reason alone drops the extension for a module the
    // closed table DOES name, because C's only producer of that reason is `func_info`,
    // which names its OWN frame with a source location and never attaches info. This is
    // the arm that makes `lists:max([])` cook `{lists,max,[[]],[]}`.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    let item = head_item_for(
        &mut machine,
        &mut ctx,
        &module,
        "lists",
        "max",
        &[1],
        &function_clause_error(),
    )
    .expect("a live catch frame and a nameable module synthesize the item");
    let expected = expected_item(
        &mut ctx,
        &module,
        "lists",
        "max",
        &[make_small(1)],
        None,
    );
    assert!(cmp(item, expected, &module.atom_table).is_eq());
    assert_eq!(
        tuple_element(item, 3).unwrap(),
        NIL,
        "a `function_clause` item carries no `error_info` extension"
    );
}

#[test]
fn a_non_function_clause_raise_keeps_the_tabled_modules_extension() {
    // Oracle, measured on OTP 29.1.2:
    //   `try lists:seq(1, 2, 0) catch error:R:S -> hd(S) end`
    //   -> `{lists,seq,[1,2,0],[{file,"lists.erl"},{line,541},
    //                          {error_info,#{module => erl_stdlib_errors}}]}`
    // `lists:seq/3` is a `beam_bifs` stdlib fast path (no `bif.tab` row), but its OTP body
    // raises through `error_with_info/2` — an Erlang-level `erlang:error/3` carrying the
    // extension — so a fast path keeps the extension exactly like a declared BIF. The only
    // residual is the source position, which no engine-side door can name.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    let item = head_item_for(
        &mut machine,
        &mut ctx,
        &module,
        "lists",
        "seq",
        &[1, 2, 0],
        &BeamError::BadArg,
    )
    .expect("a live catch frame and a nameable module synthesize the item");
    let expected = expected_item(
        &mut ctx,
        &module,
        "lists",
        "seq",
        &[make_small(1), make_small(2), make_small(0)],
        Some("erl_stdlib_errors"),
    );
    assert!(cmp(item, expected, &module.atom_table).is_eq());
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
        bif_head_item(
            &mut machine,
            &mut ctx,
            &module,
            "erlang",
            "hd",
            &[Operand::X(0)],
            &BeamError::BadArg,
        )
        .unwrap(),
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
        bif_head_item(
            &mut machine,
            &mut ctx,
            &module,
            "erlang",
            "hd",
            &[Operand::X(0)],
            &BeamError::BadArg,
        )
        .unwrap(),
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
            &BeamError::BadArg,
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

// ---------------------------------------------------------------------------
// the `cause` pair: C's `EXF_HAS_EXT_INFO` arm through the host seam
// ---------------------------------------------------------------------------

/// A context that answers [`ExecutionContext::bif_error_cause`] — C's
/// `#{cause => Fvalue, module => FormatModule}` arm.
///
/// The trait default is `Ok(None)` (the one-pair form), so `MinimalContext` — every
/// other row in this module — is the "no cause" host. The arena and the BIF floor are
/// `MinimalContext`'s, forwarded, so the item's reservation is served by the same heap
/// and a cause built on it is a real term.
struct CauseCtx {
    inner: MinimalContext,
    /// The seam's answer for any `(module, function, args)`.
    cause: Option<Eterm>,
    /// Answer `Err` instead of a value — a host that cannot decide.
    refuse: bool,
    /// How often the seam was consulted: `0` proves a shape gate dropped the item
    /// before the host was ever asked.
    calls: usize,
}

impl CauseCtx {
    /// A host carrying `cause` for whatever it is asked.
    fn answering(cause: Eterm) -> CauseCtx {
        CauseCtx {
            inner: MinimalContext::new(),
            cause: Some(cause),
            refuse: false,
            calls: 0,
        }
    }

    /// A host that models the seam and answers "no cause" (`Ok(None)`).
    fn silent() -> CauseCtx {
        CauseCtx {
            inner: MinimalContext::new(),
            cause: None,
            refuse: false,
            calls: 0,
        }
    }

    /// A host whose seam fails (`Err`) — the decline that must not replace the raise.
    fn refusing() -> CauseCtx {
        CauseCtx {
            inner: MinimalContext::new(),
            cause: None,
            refuse: true,
            calls: 0,
        }
    }

    /// The same, over a BIF floor that answers `badarg` (the end-to-end rows).
    fn with_badarg_floor(mut self) -> CauseCtx {
        self.inner = MinimalContext::with_bif_hook(Box::new(|_, _, _, _| Err(BeamError::BadArg)));
        self
    }
}

impl ExecutionContext for CauseCtx {
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

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }

    fn atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable {
        self.inner.atoms(module)
    }

    fn bif_error_cause(
        &mut self,
        module: &str,
        function: &str,
        args: &[Eterm],
    ) -> Result<Option<Eterm>> {
        let _ = (module, function, args);
        self.calls += 1;
        if self.refuse {
            return Err(BeamError::BadArg);
        }
        Ok(self.cause)
    }
}

/// [`head_item`] over any host (the seam rows need a context that is not a bare
/// `MinimalContext`).
fn head_item_with<C: ExecutionContext>(
    machine: &mut Machine,
    ctx: &mut C,
    module: &Module,
    bif_module: &str,
    bif_function: &str,
    values: &[i64],
) -> Option<Eterm> {
    for (i, value) in values.iter().enumerate() {
        machine.set_x(i as u32, make_small(*value)).unwrap();
    }
    let args: Vec<Operand> = (0..values.len()).map(|i| Operand::X(i as u32)).collect();
    bif_head_item(
        machine,
        ctx,
        module,
        bif_module,
        bif_function,
        &args,
        &BeamError::BadArg,
    )
    .expect("the arena never refuses a reservation")
}

/// The item's single `{error_info, Map}` location entry — the extension tuple itself,
/// the shape [`render_error_info_extension`] renders (and the one the order rows
/// below assert on). Panics unless the `Location` is exactly that one-entry list, so
/// a row that expects the empty-list location uses the item itself instead.
fn error_info_entry(module: &Module, item: Eterm) -> Eterm {
    let location = tuple_element(item, 3).expect("the item's Location slot");
    let entries = list_elements(location).expect("a proper Location list");
    assert_eq!(entries.len(), 1, "exactly one `{{error_info, Map}}` entry");
    assert_eq!(
        tuple_element(entries[0], 0).unwrap(),
        atom_of(module, "error_info"),
        "the location entry's key is `error_info`"
    );
    entries[0]
}

/// The `error_info` map of an item's `Location` — the single `{error_info, Map}`
/// entry, unwrapped.
fn error_info_map(module: &Module, item: Eterm) -> Eterm {
    tuple_element(error_info_entry(module, item), 1).expect("the entry's Map slot")
}

/// The map's `(key, value)` pairs in a **storage** order — the order `~p` renders and
/// the order a flat map's atom keys are sorted into.
fn stored_pairs(map: Eterm) -> Vec<(Eterm, Eterm)> {
    beam_term::map::map_pairs_storage_order(map).expect("a well-formed map")
}

#[test]
fn a_host_cause_renders_the_two_pair_extension() {
    // Oracle, measured on OTP 29.1.2 / erts-17.1.1:
    //   `try ets:lookup(unknown_atom_xyz, k) catch C:R:S -> hd(S) end`
    //   -> {ets,lookup,[unknown_atom_xyz,k],
    //         [{error_info,#{cause => id,module => erl_stdlib_errors}}]}
    // The cause is C's `c_p->fvalue` (`../../erts/emulator/beam/beam_common.c:992-1000`),
    // resolved through the host seam (the crate that owns the table registry decides
    // `id` vs `type`).
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = CauseCtx::answering(atom_of(&module, "id"));
    let item = head_item_with(&mut machine, &mut ctx, &module, "ets", "lookup", &[1, 2])
        .expect("a live catch frame and a nameable module synthesize the item");
    assert_eq!(ctx.calls, 1, "the seam is consulted exactly once");

    let map = error_info_map(&module, item);
    assert_eq!(
        beam_term::map::map_size(map, &module.atom_table).unwrap(),
        2,
        "the two-pair form"
    );
    assert_eq!(
        beam_term::map::map_get(map, atom_of(&module, "cause"), &module.atom_table).unwrap(),
        Some(atom_of(&module, "id")),
        "cause => the host's word"
    );
    assert_eq!(
        beam_term::map::map_get(map, atom_of(&module, "module"), &module.atom_table).unwrap(),
        Some(atom_of(&module, "erl_stdlib_errors")),
        "module => the closed table's format module"
    );
    // The rest of the item is untouched: the BIF's real argument list.
    assert_eq!(
        list_elements(tuple_element(item, 2).unwrap()).unwrap(),
        vec![make_small(1), make_small(2)],
        "the two-pair form must not disturb the argument list"
    );
    assert_eq!(
        tuple_element(item, 0).unwrap(),
        atom_of(&module, "ets"),
        "the item still names the BIF"
    );
}

/// A module whose table interns `cause` **before** `module` — the order `atom.names`
/// itself uses (`atom.names:178` vs `:471`) — because a flat map stores its atom keys
/// in **atom-index** order (C's `erts_cmp_flatmap_keys`, `beam_term::map::write_map`)
/// and that order is what `~p` renders.
fn cause_first_fixture() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    module.atom_table.intern("cause");
    for name in TABLE_ATOMS {
        module.atom_table.intern(name);
    }
    module
}

#[test]
fn the_two_pairs_are_stored_in_the_order_the_oracle_renders() {
    // Oracle rendering (measured): `#{cause => id,module => erl_stdlib_errors}` —
    // `cause` first. `write_map` sorts a flat map's atom keys by atom index, so on a
    // table in `atom.names` order the stored (and therefore rendered) order is
    // `cause, module` exactly. This is the same rule the oracle follows: its map is
    // authored `MAP2(hp, am_cause, …, am_module, …)` and printed in storage order.
    let module = cause_first_fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = CauseCtx::answering(atom_of(&module, "id"));
    let item = head_item_with(&mut machine, &mut ctx, &module, "ets", "lookup", &[1, 2])
        .expect("the item is synthesized");
    let pairs = stored_pairs(error_info_map(&module, item));
    assert_eq!(pairs.len(), 2);
    assert_eq!(
        (pairs[0].0, pairs[1].0),
        (atom_of(&module, "cause"), atom_of(&module, "module")),
        "the rendered order must be the oracle's `#{{cause => …, module => …}}`"
    );
    assert_eq!(pairs[0].1, atom_of(&module, "id"));
    assert_eq!(pairs[1].1, atom_of(&module, "erl_stdlib_errors"));
}

#[test]
fn the_two_pair_extension_renders_the_oracles_string() {
    // The exact text the oracle prints (`~p` of the item), measured on OTP 29.1.2 /
    // erts-17.1.1 with a compiled module:
    //   `try ets:lookup(unknown_atom_xyz, k) catch C:R:S -> hd(S) end`
    //   -> `{ets,lookup,[unknown_atom_xyz,k],
    //        [{error_info,#{cause => id,module => erl_stdlib_errors}}]}`
    // i.e. its `Location` renders as
    // `[{error_info,#{cause => id,module => erl_stdlib_errors}}]` — `cause` FIRST.
    //
    // **Who decides that order.** A flat map stores its atom keys in **atom-index**
    // order (C's `erts_cmp_flatmap_keys`, `../../erts/emulator/beam/erl_utils.h:267-272`:
    // two atoms compare by their raw words, i.e. by index) and `~p` prints them in
    // stored order, so the oracle's `cause, module` is a property of ITS atom table,
    // where `atom.names` interns `cause` (`:178`) long before `module` (`:471`). This
    // engine reproduces the rule, not the order:
    // `beam_term/src/compare.rs:483-489` (`cmp_flatmap_keys`) + the sort in
    // `beam_term/src/map.rs:379-390` (`write_map`).
    //
    // So this row must seed a table in that order — [`cause_first_fixture`] — to be
    // the oracle's string; a host whose table interns `module` first renders the pairs
    // the other way round (the next test). On `beam_rt` the question is latent today:
    // no host answers `ExecutionContext::bif_error_cause` yet, so only the one-pair
    // form is ever rendered there, and `beam_rt::boot::CORE_ATOMS` seats `module`
    // without seating `cause` — a host adopting the seam has to intern `cause` before
    // `module` to keep this row's text.
    let module = cause_first_fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = CauseCtx::answering(atom_of(&module, "id"));
    let item = head_item_with(&mut machine, &mut ctx, &module, "ets", "lookup", &[1, 2])
        .expect("the item is synthesized");
    assert_eq!(
        render_error_info_extension(error_info_entry(&module, item), &module),
        "{error_info,#{cause => id,module => erl_stdlib_errors}}",
        "the rendered extension must be the oracle's text"
    );
}

#[test]
fn the_two_pair_extension_follows_the_hosts_atom_index_order() {
    // The same item on a table that interns `module` **before** `cause` — the shape
    // `beam_rt::boot::CORE_ATOMS` has (`module` is seeded; `cause` is not seeded at
    // all) — renders `module` first. That is C's rule reproduced faithfully, and the
    // divergence it exposes is the *host's* atom ordering, not an engine choice:
    // `beam_term/src/compare.rs:483-489` orders two atom keys by index, so the stored
    // (and therefore rendered) order follows the table.
    //
    // This row exists so the dependency is pinned rather than assumed: it is the
    // engine's contract that the extension is `MAP2(cause, value, module, format)`
    // contents-wise and that its key order is the flat-map rule — see
    // `the_two_pair_extension_renders_the_oracles_string` for the table that yields
    // the oracle's text. Nothing here may be "fixed" by re-interning atoms: the atom
    // table is the host's.
    let module = fixture();
    // The premise, asserted rather than assumed: `fixture()` interns `module` first.
    let (module_index, cause_index) = (
        module.atom_table.lookup("module").expect("`module` is interned"),
        module.atom_table.lookup("cause").expect("`cause` is interned"),
    );
    assert!(
        module_index < cause_index,
        "the fixture must intern `module` before `cause`"
    );
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = CauseCtx::answering(atom_of(&module, "id"));
    let item = head_item_with(&mut machine, &mut ctx, &module, "ets", "lookup", &[1, 2])
        .expect("the item is synthesized");
    assert_eq!(
        render_error_info_extension(error_info_entry(&module, item), &module),
        "{error_info,#{module => erl_stdlib_errors,cause => id}}",
        "the rendered order is the atom table's index order, so it follows the host"
    );
}

#[test]
fn the_default_seam_keeps_the_one_pair_form() {
    // No host override at all (the trait default `Ok(None)`): the shape every
    // existing implementor renders — byte-identical to the pre-seam item.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    let item = head_item(&mut machine, &mut ctx, &module, "ets", "lookup", &[1, 2])
        .expect("a live catch frame and a nameable module synthesize the item");
    let expected = expected_item(
        &mut ctx,
        &module,
        "ets",
        "lookup",
        &[make_small(1), make_small(2)],
        Some("erl_stdlib_errors"),
    );
    assert!(cmp(item, expected, &module.atom_table).is_eq());
    let pairs = stored_pairs(error_info_map(&module, item));
    assert_eq!(
        pairs,
        vec![(
            atom_of(&module, "module"),
            atom_of(&module, "erl_stdlib_errors")
        )],
        "the default seam renders `#{{module => FormatModule}}` alone"
    );
}

#[test]
fn a_seam_answering_none_keeps_the_one_pair_form() {
    // A host that models the seam and says "no cause to attach" must render exactly
    // what the trait default renders.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = CauseCtx::silent();
    let item = head_item_with(&mut machine, &mut ctx, &module, "ets", "lookup", &[1, 2])
        .expect("the item is synthesized");
    assert_eq!(ctx.calls, 1, "the seam was consulted");
    let expected = expected_item(
        &mut ctx.inner,
        &module,
        "ets",
        "lookup",
        &[make_small(1), make_small(2)],
        Some("erl_stdlib_errors"),
    );
    assert!(cmp(item, expected, &module.atom_table).is_eq());
}

#[test]
fn a_module_outside_the_table_is_never_asked_for_a_cause() {
    // The shape rule runs first: `records:get/2` has no format module, so the item
    // has NO extension at all (`catch records:get(1,2)` -> `{records,get,[1,2],[]}`)
    // and the host is never consulted — a cause cannot resurrect a dropped extension.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = CauseCtx::answering(atom_of(&module, "id"));
    let item = head_item_with(&mut machine, &mut ctx, &module, "records", "get", &[1, 2])
        .expect("the item is synthesized");
    assert_eq!(ctx.calls, 0, "no extension means no cause question");
    assert_eq!(
        tuple_element(item, 3).unwrap(),
        NIL,
        "a module outside the closed table keeps the empty-list Location"
    );
}

#[test]
fn a_function_clause_raise_is_never_asked_for_a_cause() {
    // The reason gate likewise precedes the seam: a bare `function_clause` drops the
    // extension, so the host must not be asked (and must certainly not be rendered).
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = CauseCtx::answering(atom_of(&module, "id"));
    machine.set_x(0, make_small(1)).unwrap();
    let item = bif_head_item(
        &mut machine,
        &mut ctx,
        &module,
        "lists",
        "max",
        &[Operand::X(0)],
        &function_clause_error(),
    )
    .expect("the item is synthesized")
    .expect("a live catch frame and a nameable module");
    assert_eq!(ctx.calls, 0, "the reason gate drops the extension first");
    assert_eq!(tuple_element(item, 3).unwrap(), NIL);
}

#[test]
fn a_refusing_seam_keeps_the_one_pair_form() {
    // An `Err` from the seam declines the decoration: the item is built with the
    // one-pair form (never a marshalling error replacing the BIF's own raise).
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = CauseCtx::refusing();
    let item = head_item_with(&mut machine, &mut ctx, &module, "ets", "lookup", &[1, 2])
        .expect("an `Err` from the seam must not fail the item");
    assert_eq!(ctx.calls, 1);
    let pairs = stored_pairs(error_info_map(&module, item));
    assert_eq!(
        pairs,
        vec![(
            atom_of(&module, "module"),
            atom_of(&module, "erl_stdlib_errors")
        )],
        "a refused cause leaves the one-pair form"
    );
}

#[test]
fn a_boxed_cause_degrades_to_the_one_pair_form() {
    // The word is embedded verbatim and never re-rooted, so only an immediate can be
    // written (see `ExecutionContext::bif_error_cause`'s stability contract). A boxed
    // answer is DROPPED — the one-pair form — never a word the carve's own reservation
    // could relocate out from under the map.
    let module = fixture();
    let mut machine = machine_with_a_catch_frame();
    let boxed = {
        let mut scratch = MinimalContext::new();
        terms::make_tuple(&mut scratch, &[make_small(1), make_small(2)]).unwrap()
    };
    let mut ctx = CauseCtx::answering(boxed);
    let item = head_item_with(&mut machine, &mut ctx, &module, "ets", "lookup", &[1, 2])
        .expect("the item is synthesized");
    assert_eq!(ctx.calls, 1);
    let pairs = stored_pairs(error_info_map(&module, item));
    assert_eq!(
        pairs,
        vec![(
            atom_of(&module, "module"),
            atom_of(&module, "erl_stdlib_errors")
        )],
        "a non-immediate cause is dropped, not stored"
    );
}

#[test]
fn a_host_cause_reaches_the_cooked_trace_on_both_tiers() {
    // End to end on the compiled route: the innermost cooked item carries the two-pair
    // extension, identically on the interpreter and the native baseline tier.
    let (module, run_atom) = traced_bif_call("ets", "lookup", 2, NO_FAIL);
    let mut ctx = CauseCtx::answering(atom_of(&module, "id")).with_badarg_floor();
    let mut interpreter = Machine::new();
    let cooked = interpreter
        .run_export(&module, run_atom, 0, &[], &mut ctx)
        .expect("the handler returns the cooked trace");
    let mut jit = Machine::new();
    let cooked_jit = jit
        .run_with_tier(&module, run_atom, 0, &[], &mut ctx, Tier::BaselineJit)
        .expect("the handler returns the cooked trace on the native tier too");
    assert!(
        cmp(cooked, cooked_jit, &module.atom_table).is_eq(),
        "the interpreter and the baseline tier must cook the same trace"
    );
    let items = list_elements(cooked).unwrap();
    assert_eq!(items.len(), 2, "the BIF head item plus the raising frame");
    let map = error_info_map(&module, items[0]);
    assert_eq!(
        beam_term::map::map_get(map, atom_of(&module, "cause"), &module.atom_table).unwrap(),
        Some(atom_of(&module, "id")),
        "the cooked innermost item carries `cause => id`"
    );
    assert_eq!(
        beam_term::map::map_get(map, atom_of(&module, "module"), &module.atom_table).unwrap(),
        Some(atom_of(&module, "erl_stdlib_errors")),
        "…beside `module => erl_stdlib_errors`"
    );
}

#[test]
fn the_shape_rule_helpers_are_reachable_from_the_crate_root() {
    // The two halves of the shape rule are public and re-exported at the crate root
    // (`beam_engine::{format_module, is_non_bif_raise}`), because `beam_rt`'s `-eval`
    // route renders the same extension from the same table. Resolving them through the
    // re-export path is what a cross-crate caller does.
    assert_eq!(crate::format_module("ets"), Some("erl_stdlib_errors"));
    assert_eq!(crate::format_module("records"), None);
    assert!(crate::is_non_bif_raise("erlang", "error", 3));
    assert!(!crate::is_non_bif_raise("erlang", "hd", 1));
}
