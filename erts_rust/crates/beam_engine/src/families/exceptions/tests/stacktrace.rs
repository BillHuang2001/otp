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

use std::sync::Arc;

use beam_loader::Atom;

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
fn build_stacktrace_cooks_the_raise_snapshot_innermost_first() {
    // The frames are cooked from the machine's **raise-time snapshot**
    // (`Machine::raise_snapshot`), not from the live call frames: the raise unwinds
    // `outer`'s frame away *before* `build_stacktrace` runs, yet the snapshot still
    // names it. The raising frame comes first, then every caller innermost-first.
    // No line marker is in effect, so each Location is NIL (C's
    // LINE_INVALID_LOCATION).
    let mut fixture = Fixture::new(&[]);
    let run_entry = fixture.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::BuildStacktrace,
            Instr::Return,
        ],
    );
    let caller_entry =
        fixture.add_function("caller", 0, vec![Instr::Label(Label(2)), Instr::Return]);
    let outer_entry =
        fixture.add_function("outer", 0, vec![Instr::Label(Label(3)), Instr::Return]);
    let module = fixture.finish();
    assert_eq!(run_entry, Label(1));
    assert_eq!(caller_entry, Label(2));
    assert_eq!(outer_entry, Label(3));

    let mut machine = Machine::new();
    // The bottom frame, then `caller` (resumes at `(1, 1)`) and `outer` (resumes at
    // `(2, 1)`) each suspended in a call.
    machine.push_frame(0).unwrap();
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(crate::machine::Continuation::Address {
            func: 1,
            pc: 1,
            context: 0,
        })
        .unwrap();
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(crate::machine::Continuation::Address {
            func: 2,
            pc: 1,
            context: 0,
        })
        .unwrap();
    // The raising instruction: `run/0` at code offset 1.
    machine.current_code = Some((0, 1));
    // A catch frame owned by frame 1: landing there unwinds frame 2 (`outer`) away.
    machine.push_catch_frame(CatchFrame::new(1, 0, 1));
    machine.set_class_atom(ExceptionClass::Error, make_atom(Atom::from_raw(11)));
    let step = machine
        .throw_with_trace(ExceptionClass::Error, Eterm::from_raw(1), NIL)
        .expect("a live frame catches");
    assert_eq!(step, Step::Goto { func: 1, pc: 0 });
    assert_eq!(machine.frame_depth(), 2, "the raise unwound `outer` away");

    // x(0) must hold a non-NIL raw stacktrace for the instruction to cook one.
    machine.set_x(0, make_small(1)).unwrap();
    let mut ctx = MinimalContext::new();
    let step = exec(
        &mut machine,
        &module,
        &module,
        &no_labels(),
        &Instr::BuildStacktrace,
        0,
        0,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));

    let m = module.atom_table.lookup("m").unwrap();
    let run = module.atom_table.lookup("run").unwrap();
    let caller = module.atom_table.lookup("caller").unwrap();
    let outer = module.atom_table.lookup("outer").unwrap();
    let item_run =
        terms::make_tuple(&mut ctx, &[make_atom(m), make_atom(run), make_small(0), NIL])
            .unwrap();
    let item_outer =
        terms::make_tuple(&mut ctx, &[make_atom(m), make_atom(outer), make_small(0), NIL])
            .unwrap();
    let item_caller =
        terms::make_tuple(&mut ctx, &[make_atom(m), make_atom(caller), make_small(0), NIL])
            .unwrap();
    let expected = terms::make_list(&mut ctx, &[item_run, item_outer, item_caller]).unwrap();
    assert!(
        cmp(machine.x(0).unwrap(), expected, &module.atom_table).is_eq(),
        "the cooked trace must be [{{m,run,0,[]}}, {{m,outer,0,[]}}, {{m,caller,0,[]}}] — the \
         unwound `outer` frame being present proves the snapshot is used, not the live stack"
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
    // The snapshot's raising frame is the `line`-marked code offset 2: capture it
    // through an (uncaught) raise, which snapshots before unwinding.
    machine.current_code = Some((0, 2));
    let err = machine
        .throw(ExceptionClass::Error, Eterm::from_raw(1))
        .expect_err("no catch frame");
    assert!(matches!(err, BeamError::Raise(_)));
    machine.set_x(0, make_small(1)).unwrap(); // a non-NIL raw trace
    let mut ctx = MinimalContext::new();
    let step = exec(&mut machine, &module, &module, &no_labels(), &Instr::BuildStacktrace, 0, 0, &mut ctx)
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
        // A snapshot frame with a real location, so `location_shape` is reached:
        // capture it through an (uncaught) raise.
        machine.current_code = Some((0, 2));
        let err = machine
            .throw(ExceptionClass::Error, Eterm::from_raw(1))
            .expect_err("no catch frame");
        assert!(matches!(err, BeamError::Raise(_)));
        machine.set_x(0, make_small(1)).unwrap();
        let mut ctx = MinimalContext::new();
        let err =
            exec(&mut machine, &module, &module, &no_labels(), &Instr::BuildStacktrace, 0, 0, &mut ctx)
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
    // The snapshot's raising frame names function index 3, absent from the empty
    // module. Capture it through an (uncaught) raise.
    machine.current_code = Some((3, 0));
    let err = machine
        .throw(ExceptionClass::Error, Eterm::from_raw(1))
        .expect_err("no catch frame");
    assert!(matches!(err, BeamError::Raise(_)));
    machine.set_x(0, make_small(1)).unwrap();
    let mut ctx = MinimalContext::new();
    let err = exec(
        &mut machine,
        &module,
        &module,
        &no_labels(),
        &Instr::BuildStacktrace,
        0,
        0,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)));
}

#[test]
fn build_stacktrace_resolves_a_snapshot_frame_in_its_own_module_context() {
    // A snapshot frame whose context is a registered (non-zero) module context
    // resolves in **that** module; a context-`0` frame resolves in the run's
    // **entry** module — the `entry_module` argument — *whatever* module context the
    // handler cooking the trace executes in.
    //
    // ORACLE (verified): `ent:g/0 -> mid3:run/0 -> callee:h/1 -> erlang:error(badarg)`
    // with the handler inside the CALLEE module and every call non-tail:
    //   $ erl -noshell -eval 'io:format("~p~n",[ent:g()]), halt().'
    //   [{callee,h,1,[{file,"callee.erl"},{line,5}]},
    //    {mid3,run,0,[{file,"mid3.erl"},{line,4}]},
    //    {ent,g,0,[{file,"ent.erl"},{line,4}]} | <host erl_eval frames>]
    // — the entry module's frame IS present. The old heuristic (skip a context-`0`
    // frame whenever the handler is in a non-zero context) dropped it.
    //
    // This test's construction: two context-`id` frames (the raising `boom/1` and a
    // caller returning into the callee — both in the callee), then one context-`0`
    // frame (the entry module's `run/0`). So the cooked list is
    // `[{callee,boom,1}, {callee,boom,1}, {m,run,0}]`, innermost first.
    let mut fixture = Fixture::new(&[]);
    let callee_name = fixture.module.atom_table.intern("callee");
    fixture.module.name = callee_name;
    let boom_entry = fixture.add_function("boom", 1, vec![Instr::Label(Label(1)), Instr::Return]);
    assert_eq!(boom_entry, Label(1));
    // `boom` is the callee's first (and only) function, so its function index is 0.
    let boom: u32 = 0;
    let callee = Arc::new(fixture.finish());

    // The run's entry module (context 0): a `run/0` the context-0 frame resolves to.
    let mut entry_fixture = Fixture::new(&[]);
    entry_fixture.add_function("run", 0, vec![Instr::Label(Label(1)), Instr::Return]);
    let entry_module = entry_fixture.finish();

    let mut machine = Machine::new();
    // A frame chain holding BOTH a context-`0` frame and context-`id` frames: the
    // bottom frame, then a frame returning into context 0, then the raising `boom`
    // frame in the callee context.
    machine.push_frame(0).unwrap();
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(crate::machine::Continuation::Address {
            func: 0,
            pc: 0,
            context: 0,
        })
        .unwrap();
    let id = machine.register_context(&callee);
    assert!(id >= 1, "a registered context id is non-zero");
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(crate::machine::Continuation::Address {
            func: boom,
            pc: 0,
            context: id,
        })
        .unwrap();
    machine.switch_context(id).unwrap();
    machine.current_code = Some((boom, 0));

    // An uncaught raise (no catch frame) still captures the snapshot.
    let err = machine
        .throw_with_trace(ExceptionClass::Exit, Eterm::from_raw(2), NIL)
        .expect_err("no catch frame");
    assert!(matches!(err, BeamError::Raise(_)));
    assert_eq!(
        machine.current_context(),
        id,
        "the handler runs in the callee's context"
    );

    machine.set_x(0, make_small(1)).unwrap();
    let mut ctx = MinimalContext::new();
    let step = exec(
        &mut machine,
        &entry_module,
        &entry_module,
        &no_labels(),
        &Instr::BuildStacktrace,
        0,
        0,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));

    // The cooked trace holds all three frames: the two context-`id` frames resolve in
    // the callee, and the context-`0` frame in the entry module (`m`).
    let callee_atom = callee.atom_table.lookup("callee").unwrap();
    let boom_atom = callee.atom_table.lookup("boom").unwrap();
    let m_atom = entry_module.atom_table.lookup("m").unwrap();
    let run_atom = entry_module.atom_table.lookup("run").unwrap();
    let frames = crate::terms::list_elements(machine.x(0).unwrap()).unwrap();
    assert_eq!(
        frames.len(),
        3,
        "the context-0 snapshot frame is named in the entry module, not skipped"
    );
    let item = |ctx: &mut MinimalContext, module: Atom, function: Atom, arity: i64| {
        terms::make_tuple(
            ctx,
            &[make_atom(module), make_atom(function), make_small(arity), NIL],
        )
        .unwrap()
    };
    let raising_frame = item(&mut ctx, callee_atom, boom_atom, 1);
    let caller_frame = item(&mut ctx, callee_atom, boom_atom, 1);
    let entry_frame = item(&mut ctx, m_atom, run_atom, 0);
    let expected =
        terms::make_list(&mut ctx, &[raising_frame, caller_frame, entry_frame]).unwrap();
    assert!(
        cmp(machine.x(0).unwrap(), expected, &entry_module.atom_table).is_eq(),
        "the cooked trace must be [{{callee,boom,1,[]}}, {{callee,boom,1,[]}}, {{m,run,0,[]}}]"
    );
}