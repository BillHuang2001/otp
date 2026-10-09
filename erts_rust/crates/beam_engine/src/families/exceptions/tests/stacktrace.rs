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

//! `build_stacktrace` — the symbolic stacktrace, the `NIL` handling and the
//! source-location items.
use super::*;

// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity. (An explicit
// import shadows the glob-imported `terms` above.)
use crate::terms::test_support as terms;

// ------------------------------------------------------------------
// build_stacktrace (the symbolic stacktrace)
// ------------------------------------------------------------------

#[test]
fn build_stacktrace_of_a_nil_raw_trace_is_nil() {
    // C: `get_trace_from_exc(NIL)` is NULL, so `build_stacktrace` yields NIL
    // (`beam_common.c:1219-1223`). This is the path every compiler-emitted
    // handler takes at M1 (a caught exception lands with x(2) = NIL).
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.set_x(0, NIL).unwrap();
    let mut ctx = MinimalContext::new();
    let step = exec_one(&module, &Instr::BuildStacktrace, &mut machine, &mut ctx).unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(0), Some(NIL));
}

#[test]
fn build_stacktrace_derives_the_live_callers_innermost_first() {
    // A non-NIL raw trace (only a synthetic module produces one at M1) is
    // approximated from the live frames: the current function first, then every
    // frame suspended in a call, innermost first. No line marker is in effect,
    // so each Location is NIL (C's LINE_INVALID_LOCATION).
    let mut fixture = Fixture::new(&[]);
    let entry = fixture.add_function(
        "f",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::BuildStacktrace,
            Instr::Return,
        ],
    );
    let g_entry = fixture.add_function(
        "g",
        2,
        vec![Instr::Label(Label(2)), Instr::Return],
    );
    let module = fixture.finish();
    assert_eq!(entry, Label(1));
    assert_eq!(g_entry, Label(2));

    let mut machine = Machine::new();
    // The bottom frame, then a frame that called into `f` — its continuation is
    // `f` at code offset 2 (`g/2`'s own frame-less `return`).
    machine.push_frame(0).unwrap();
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(crate::machine::Continuation::Address {
            func: 1,
            pc: 1,
            context: 0,
        })
        .unwrap();
    // x(0) must hold a non-NIL raw stacktrace for the instruction to cook one.
    machine.set_x(0, make_small(1)).unwrap();

    let mut ctx = MinimalContext::new();
    // Run the `build_stacktrace` at code offset 1 of function 0.
    let step = exec(
        &mut machine,
        &module,
        &no_labels(),
        &Instr::BuildStacktrace,
        0,
        1,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));

    let m = module.atom_table.lookup("m").unwrap();
    let f = module.atom_table.lookup("f").unwrap();
    let g = module.atom_table.lookup("g").unwrap();
    let item_f = terms::make_tuple(&mut ctx, &[make_atom(m), make_atom(f), make_small(0), NIL])
        .unwrap();
    let item_g = terms::make_tuple(&mut ctx, &[make_atom(m), make_atom(g), make_small(2), NIL])
        .unwrap();
    let expected = terms::make_list(&mut ctx, &[item_f, item_g]).unwrap();
    assert!(
        cmp(machine.x(0).unwrap(), expected, &module.atom_table).is_eq(),
        "the cooked trace must be [{{m,f,0,[]}}, {{m,g,2,[]}}]"
    );
}

#[test]
fn build_stacktrace_item_carries_the_source_location() {
    // The Location element is `[{file,Name},{line,N}]`, taken from the `line`
    // marker in effect at the frame's code offset (C's `erts_build_mfa_item`).
    let mut fixture = Fixture::new(&["file", "line"]);
    fixture.module.line = beam_loader::LineTable {
        flags: 0,
        instruction_count: 0,
        items: vec![
            LineEntry::default(),
            LineEntry { file: 0, line: 7 },
        ],
        files: vec!["m.erl".to_string()],
    };
    fixture.add_function(
        "f",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Line(1),
            Instr::BuildStacktrace,
            Instr::Return,
        ],
    );
    let module = fixture.finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.set_x(0, make_small(1)).unwrap(); // a non-NIL raw trace
    let mut ctx = MinimalContext::new();
    let step = exec(&mut machine, &module, &no_labels(), &Instr::BuildStacktrace, 0, 2, &mut ctx)
        .unwrap();
    assert_eq!(step, Some(Step::Next));

    let m = module.atom_table.lookup("m").unwrap();
    let f = module.atom_table.lookup("f").unwrap();
    let file = module.atom_table.lookup("file").unwrap();
    let line = module.atom_table.lookup("line").unwrap();
    let chars: Vec<Eterm> = "m.erl".chars().map(|c| make_small(c as i64)).collect();
    let name = terms::make_list(&mut ctx, &chars).unwrap();
    let file_pair = terms::make_tuple(&mut ctx, &[make_atom(file), name]).unwrap();
    let line_pair = terms::make_tuple(&mut ctx, &[make_atom(line), make_small(7)]).unwrap();
    let location = terms::make_list(&mut ctx, &[file_pair, line_pair]).unwrap();
    let item =
        terms::make_tuple(&mut ctx, &[make_atom(m), make_atom(f), make_small(0), location])
            .unwrap();
    let expected = terms::make_list(&mut ctx, &[item]).unwrap();
    assert!(
        cmp(machine.x(0).unwrap(), expected, &module.atom_table).is_eq(),
        "the item must carry [{{file,\"m.erl\"}},{{line,7}}]"
    );
}

#[test]
fn build_stacktrace_without_the_file_or_line_atom_is_a_typed_error() {
    // The location can only be named if the module's atom table holds
    // `file`/`line` (C reads its globals `am_file`/`am_line`).
    for extra in [&["line"][..], &["file"][..]] {
        let mut fixture = Fixture::new(extra);
        fixture.module.line = beam_loader::LineTable {
            flags: 0,
            instruction_count: 0,
            items: vec![LineEntry::default(), LineEntry { file: 0, line: 7 }],
            files: vec!["m.erl".to_string()],
        };
        fixture.add_function(
            "f",
            0,
            vec![Instr::Label(Label(1)), Instr::Line(1), Instr::BuildStacktrace],
        );
        let module = fixture.finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.set_x(0, make_small(1)).unwrap();
        let mut ctx = MinimalContext::new();
        let err =
            exec(&mut machine, &module, &no_labels(), &Instr::BuildStacktrace, 0, 2, &mut ctx)
                .unwrap_err();
        assert!(
            matches!(err, BeamError::Raise(_)),
            "a module that cannot name file/line must fail typed"
        );
    }
}

#[test]
fn build_stacktrace_of_an_unknown_function_index_is_a_typed_error() {
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.set_x(0, make_small(1)).unwrap();
    let mut ctx = MinimalContext::new();
    // Function index 3 does not exist in the empty module.
    let err = exec(
        &mut machine,
        &module,
        &no_labels(),
        &Instr::BuildStacktrace,
        3,
        0,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)));
}
