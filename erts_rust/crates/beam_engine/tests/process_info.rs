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

//! Execution-position introspection (`process_info/2`'s position items).
//!
//! Proves the engine's read-only accessors — [`Machine::current_function`],
//! [`Machine::current_location`], [`Machine::current_stacktrace`] and
//! [`Machine::initial_call`] — report the term **shapes** the real `erl` oracle
//! returns, and the right values for a known suspended process.
//!
//! The oracle shapes were captured from the system Erlang (OTP 29 / erts-17.1)
//! with the committed receive fixture on the code path:
//!
//! ```sh
//! erl -noshell -pa <fixtures> -eval '
//!   P = spawn(recv, wait, []), timer:sleep(100),
//!   io:format("~p~n", [erlang:process_info(P, current_location)]), halt().'
//! %% {current_location,{recv,wait,0,[{file,"recv.erl"},{line,12}]}}
//! ```
//!
//! so `current_function`/`initial_call` are `{M,F,A}` and `current_location` is the
//! 4-tuple `{M,F,A,[{file,File},{line,Line}]}`. A live cross-check
//! ([`oracle_shape`]) re-derives those facts from the installed `erl` and compares
//! them, **skipping loudly** when no oracle is present (the sandbox has one, but a
//! bare CI container may not).
//!
//! ## Why a synthetic module for the location shapes
//!
//! ERTS resolves `{file,_}`/`{line,_}` against its **globally interned** atoms
//! (`am_file`/`am_line`). The engine names atoms in the *running module's* table
//! (the [`build_stacktrace`](beam_engine::error) residual), and a typical compiled
//! module — `recv` included — does not itself mention the `file`/`line` atoms, so
//! for it the location is the documented typed `unsupported` (Handoff #2:
//! `beam_rt` seeds the global atoms). [`SyntheticModule`] interns them, exactly as
//! a real runtime's global table does, so the accessors' full term shapes are
//! pinned; the honest error is pinned on the real fixture.

use std::process::Command;

use beam_common::{BeamError, Result};
use beam_engine::{terms, Machine, Mailbox, MinimalContext, RunOutcome};
use beam_loader::{
    load_file, Export, Function, Instr, Label, LineEntry, LineTable, Module, Operand,
};
use beam_term::{eq, make_atom, make_small, signed_value, Atom, Eterm, NIL};

/// The committed receive fixture (`tests/fixtures/recv.beam`, built with `erlc`);
/// `recv:wait/0` is a bare `receive X -> X end` that suspends on an empty mailbox.
const RECV_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/recv.beam");
/// The fixture's directory (the oracle's `-pa` code path).
const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

// ---------------------------------------------------------------------------
// A mailbox that is always empty (enough to suspend a plain `receive`)
// ---------------------------------------------------------------------------

/// An in-memory mailbox that never holds a message, so a bare `receive` suspends.
struct EmptyMailbox;

impl Mailbox for EmptyMailbox {
    fn send_message(&mut self, _to: Eterm, _message: Eterm) -> Result<()> {
        Ok(())
    }
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(None)
    }
    fn receive_skip(&mut self) -> Result<()> {
        Ok(())
    }
    fn receive_remove(&mut self) -> Result<()> {
        Ok(())
    }
    fn receive_reset(&mut self) -> Result<()> {
        Ok(())
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
// A synthetic `mini:wait/0` module (line info, with the file/line atoms interned)
// ---------------------------------------------------------------------------

/// `mini:wait/0`: a `receive` over an empty mailbox that suspends at a `wait`
/// carrying a source-line marker (`mini.erl:12`), with the `file`/`line` atoms
/// interned the way a real runtime's global atom table holds them.
///
/// The code is the smallest shape that reaches the receive family:
/// `label 1 | line 12 | label 2 (entry) | loop_rec fail=3 -> 3 | wait -> 2 | label 3 | wait -> 2`.
fn synthetic_wait_module() -> Module {
    let mut module = Module::default();
    let name = module.atom_table.intern("mini");
    let wait = module.atom_table.intern("wait");
    // The atoms ERTS keeps globally interned and resolves a Location with.
    module.atom_table.intern("file");
    module.atom_table.intern("line");
    module.name = name;
    module.line = LineTable {
        flags: 0,
        instruction_count: 0,
        items: vec![LineEntry::default(), LineEntry { file: 0, line: 12 }],
        files: vec!["mini.erl".to_string()],
    };
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Line(1),
        Instr::Label(Label(2)),
        Instr::LoopRec {
            fail: Label(3),
            dst: Operand::X(0),
        },
        Instr::Wait { target: Label(2) },
        Instr::Label(Label(3)),
        Instr::Wait { target: Label(2) },
    ];
    let labels = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();
    module.functions = vec![Function {
        func_label: Label(1),
        entry_label: Label(2),
        module: name,
        name: wait,
        arity: 0,
        code,
        blocks: Vec::new(),
        labels,
    }];
    module.exports = vec![Export {
        function: wait,
        arity: 0,
        label: Label(2),
    }];
    module
}

/// Suspend `mini:wait/0` (the synthetic module) on an empty mailbox and return the
/// module, the paused machine and the still-alive context.
fn suspended_synthetic() -> (Module, Machine, MinimalContext) {
    let module = synthetic_wait_module();
    let wait = module.atom_table.lookup("wait").expect("wait atom");
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let outcome = machine
        .run_export_outcome(&module, wait, 0, &[], &mut ctx)
        .expect("run mini:wait/0");
    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "an empty mailbox must suspend mini:wait/0, got {outcome:?}"
    );
    (module, machine, ctx)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// Load the receive fixture and suspend it in `recv:wait/0`'s `receive`.
///
/// Returns the module, the paused machine and the still-alive context (so its
/// arena outlives every term the accessors build).
fn suspended_wait() -> (Module, Machine, MinimalContext) {
    let module = load_file(RECV_BEAM).expect("load recv.beam");
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let outcome = machine
        .run_export_outcome(&module, atom(&module, "wait"), 0, &[], &mut ctx)
        .expect("run recv:wait/0");
    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "an empty mailbox must suspend recv:wait/0, got {outcome:?}"
    );
    (module, machine, ctx)
}

/// Read an Erlang string term (a list of code points) back into a `String`.
fn string_of(term: Eterm) -> String {
    let elements = terms::list_elements(term).expect("an Erlang string is a proper list");
    elements
        .iter()
        .map(|&ch| char::from_u32(signed_value(ch) as u32).expect("a valid code point"))
        .collect()
}

/// Asserts `got` is the `{M,F,A}` tuple `{module_name, function, arity}`.
fn assert_mfa(module: &Module, got: Eterm, module_name: &str, function: &str, arity: u32) {
    assert_eq!(
        terms::tuple_arity(got).expect("an MFA is a tuple"),
        3,
        "an MFA is a 3-tuple, got {got:?}"
    );
    let expected = [
        make_atom(atom(module, module_name)),
        make_atom(atom(module, function)),
        make_small(arity as i64),
    ];
    for (index, want) in expected.iter().enumerate() {
        let element = terms::tuple_element(got, index).expect("MFA element");
        assert!(
            eq(element, *want, &module.atom_table),
            "MFA element {index}: got {element:?}, want {want:?}"
        );
    }
}

/// The `[{file,File},{line,Line}]` of a location list, or `None` for the empty
/// list (a function with no source location).
fn location_parts(module: &Module, location: Eterm) -> Option<(String, i64)> {
    if eq(location, NIL, &module.atom_table) {
        return None;
    }
    let pairs = terms::list_elements(location).expect("a location is a proper list");
    assert_eq!(
        pairs.len(),
        2,
        "a location is [{{file,..}},{{line,..}}]: {location:?}"
    );
    let file_atom = terms::tuple_element(pairs[0], 0).expect("{{file,..}} key");
    let line_atom = terms::tuple_element(pairs[1], 0).expect("{{line,..}} key");
    assert!(eq(
        file_atom,
        make_atom(atom(module, "file")),
        &module.atom_table
    ));
    assert!(eq(
        line_atom,
        make_atom(atom(module, "line")),
        &module.atom_table
    ));
    let file = terms::tuple_element(pairs[0], 1).expect("{{file,File}} value");
    let line = terms::tuple_element(pairs[1], 1).expect("{{line,Line}} value");
    Some((string_of(file), signed_value(line)))
}

// ---------------------------------------------------------------------------
// current_function / initial_call — the real fixture, end to end
// ---------------------------------------------------------------------------

#[test]
fn current_function_is_the_mfa_of_the_blocking_function() {
    let (module, machine, mut ctx) = suspended_wait();
    let value = machine
        .current_function(&module, &mut ctx)
        .expect("current_function")
        .expect("a suspended machine has a current function");
    // The oracle: {current_function,{recv,wait,0}}.
    assert_mfa(&module, value, "recv", "wait", 0);
}

#[test]
fn initial_call_is_the_entry_mfa() {
    let (module, machine, mut ctx) = suspended_wait();
    let value = machine
        .initial_call(&module, &mut ctx)
        .expect("initial_call")
        .expect("a machine that has run has an entry call");
    // The oracle for `spawn(recv, wait, [])`: {initial_call,{recv,wait,0}}.
    assert_mfa(&module, value, "recv", "wait", 0);

    // The raw accessor agrees: the entry `(func, arity)` is `recv:wait/0`.
    let (func, arity) = machine.entry_call().expect("entry call recorded");
    assert_eq!(arity, 0);
    assert_eq!(
        module.functions[func as usize].name,
        atom(&module, "wait"),
        "the entry function is recv:wait/0"
    );
}

// ---------------------------------------------------------------------------
// current_location / current_stacktrace — the oracle term shapes
// ---------------------------------------------------------------------------

#[test]
fn current_location_is_a_four_tuple_with_file_and_line() {
    let (module, machine, mut ctx) = suspended_synthetic();
    let value = machine
        .current_location(&module, &mut ctx)
        .expect("current_location")
        .expect("a suspended machine has a current location");
    // The oracle: {current_location,{M,F,A,[{file,"m.erl"},{line,N}]}}.
    assert_eq!(
        terms::tuple_arity(value).expect("a location is a tuple"),
        4,
        "current_location is a {{M,F,A,Location}} 4-tuple, got {value:?}"
    );
    let mfa0 = terms::tuple_element(value, 0).unwrap();
    let mfa1 = terms::tuple_element(value, 1).unwrap();
    let mfa2 = terms::tuple_element(value, 2).unwrap();
    assert!(eq(
        mfa0,
        make_atom(atom(&module, "mini")),
        &module.atom_table
    ));
    assert!(eq(
        mfa1,
        make_atom(atom(&module, "wait")),
        &module.atom_table
    ));
    assert!(eq(mfa2, make_small(0), &module.atom_table));

    let location = terms::tuple_element(value, 3).expect("the Location list");
    let (file, line) = location_parts(&module, location).expect("a source location");
    assert_eq!(
        file, "mini.erl",
        "the file name from the module's Line chunk"
    );
    assert_eq!(line, 12, "the line marker in effect at the suspended pc");
}

#[test]
fn current_stacktrace_head_is_the_current_location() {
    let (module, machine, mut ctx) = suspended_synthetic();
    let stacktrace = machine
        .current_stacktrace(&module, &mut ctx)
        .expect("current_stacktrace");
    let frames = terms::list_elements(stacktrace).expect("a stacktrace is a proper list");
    assert_eq!(
        frames.len(),
        1,
        "the suspended frame is `mini:wait/0` and its bottom caller contributes none"
    );
    // The oracle: {current_stacktrace,[{M,F,A,[{file,"m.erl"},{line,N}]}]}.
    let head = frames[0];
    assert_eq!(terms::tuple_arity(head).unwrap(), 4);
    let (file, line) = location_parts(&module, terms::tuple_element(head, 3).unwrap())
        .expect("the head's location");
    assert_eq!(file, "mini.erl");
    assert_eq!(line, 12);

    // The stacktrace head is byte-identical to `current_location` (one builder).
    let current = machine
        .current_location(&module, &mut ctx)
        .unwrap()
        .unwrap();
    assert!(
        eq(head, current, &module.atom_table),
        "the innermost stacktrace frame is the current location"
    );
}

#[test]
fn a_location_the_module_cannot_name_is_a_typed_error() {
    // ERTS resolves `file`/`line` from its global atom table; the engine names
    // atoms in the running module's table. `recv` has line info but does not itself
    // mention those atoms, so the location — and the stacktrace item that carries
    // it — is the documented typed error (Handoff #2: `beam_rt` seeds the atoms),
    // never an invented name.
    let (module, machine, mut ctx) = suspended_wait();
    for err in [
        machine.current_location(&module, &mut ctx).unwrap_err(),
        machine.current_stacktrace(&module, &mut ctx).unwrap_err(),
    ] {
        assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
    }
}

// ---------------------------------------------------------------------------
// Honesty: nothing is invented before the machine has run
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_machine_reports_no_position() {
    let module = load_file(RECV_BEAM).expect("load recv.beam");
    let machine = Machine::new();
    let mut ctx = MinimalContext::new();

    assert_eq!(
        machine.current_code(),
        None,
        "no instruction interpreted yet"
    );
    assert_eq!(machine.entry_call(), None, "no run has begun");
    assert_eq!(machine.current_function(&module, &mut ctx).unwrap(), None);
    assert_eq!(machine.initial_call(&module, &mut ctx).unwrap(), None);
    assert_eq!(machine.current_location(&module, &mut ctx).unwrap(), None);
    assert_eq!(
        machine.current_stacktrace(&module, &mut ctx).unwrap(),
        NIL,
        "an un-started machine has an empty stacktrace"
    );
}

#[test]
fn reset_clears_the_recorded_position() {
    let (module, mut machine, mut ctx) = suspended_wait();
    // Sanity: the suspended machine does report a position.
    assert!(machine
        .current_function(&module, &mut ctx)
        .unwrap()
        .is_some());
    assert!(machine.entry_call().is_some());

    machine.reset();
    assert_eq!(machine.current_code(), None);
    assert_eq!(machine.entry_call(), None);
    assert_eq!(machine.current_function(&module, &mut ctx).unwrap(), None);
    assert_eq!(machine.current_location(&module, &mut ctx).unwrap(), None);
}

// ---------------------------------------------------------------------------
// A live cross-check against the installed `erl` oracle (skip loudly if absent)
// ---------------------------------------------------------------------------

/// The oracle's shape facts for a `spawn(recv, wait, [])` process, as a
/// deterministic string: `"<current_function> <current_location-MFA>
/// <location-shape> <initial_call>"`.
///
/// `None` when no `erl` runs (the caller skips loudly).
fn oracle_shape() -> Option<String> {
    let eval = concat!(
        "P = spawn(recv, wait, []), timer:sleep(50),",
        "{current_function, MFA} = erlang:process_info(P, current_function),",
        "{current_location, {M2, F2, A2, Loc}} = erlang:process_info(P, current_location),",
        "{initial_call, IC} = erlang:process_info(P, initial_call),",
        "Shape = [element(1, X) || X <- Loc],",
        "io:format(\"~w ~w ~w ~w\", [MFA, {M2, F2, A2}, Shape, IC]), halt()."
    );
    let output = Command::new("erl")
        .args(["-noshell", "-pa", FIXTURES_DIR, "-eval", eval])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
fn the_accessor_shapes_match_a_live_oracle() {
    let Some(oracle) = oracle_shape() else {
        eprintln!(
            "SKIP process_info::the_accessor_shapes_match_a_live_oracle: \
             no `erl` oracle on PATH"
        );
        return;
    };
    // current_function {M,F,A} · current_location's {M,F,A} · [{file,..},{line,..}] ·
    // initial_call {M,F,A} — every field is the oracle's own, so a shape drift in
    // any of the four items shows up here.
    assert_eq!(
        oracle, "{recv,wait,0} {recv,wait,0} [file,line] {recv,wait,0}",
        "the oracle's process_info/2 position shapes changed"
    );

    // The engine agrees with the oracle's MFA facts for its own suspended run.
    let (module, machine, mut ctx) = suspended_wait();
    let current = machine
        .current_function(&module, &mut ctx)
        .unwrap()
        .unwrap();
    assert_mfa(&module, current, "recv", "wait", 0);
    let initial = machine.initial_call(&module, &mut ctx).unwrap().unwrap();
    assert_mfa(&module, initial, "recv", "wait", 0);

    // And it produces the oracle's location *shape* where it can name the atoms.
    let (synthetic, machine, mut ctx) = suspended_synthetic();
    let location = machine
        .current_location(&synthetic, &mut ctx)
        .unwrap()
        .unwrap();
    assert_eq!(terms::tuple_arity(location).unwrap(), 4);
    let (_file, _line) = location_parts(&synthetic, terms::tuple_element(location, 3).unwrap())
        .expect("a source location");
}
