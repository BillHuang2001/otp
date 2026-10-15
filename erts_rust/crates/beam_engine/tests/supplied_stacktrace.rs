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

//! The **public supplied-stacktrace encoder**, exercised the way an **external**
//! host (another crate) reaches it: through the crate root re-export
//! [`beam_engine::encode_supplied_stacktrace`], the [`beam_engine::prelude`] glob and
//! the [`Machine`](beam_engine::Machine) method — never through the crate's internal
//! paths.
//!
//! The unit suite (`src/families/exceptions/tests/encoder.rs`) pins the cooked
//! layout against the oracle and against the engine's private carve. This suite pins
//! what only an out-of-crate caller can check: that both public entry points are
//! reachable and useful with **only** the public API (a `beam_loader::Module` whose
//! atom table is a clone of the host's shared table, and a `MinimalContext` for the
//! heap), and that a frame naming an atom the passed table lacks is the typed
//! `unsupported` error rather than a substituted atom.
use beam_common::{AtomName, ExceptionClass, SourceLocation, StackFrame, StackTrace};
use beam_engine::prelude::*;
use beam_loader::Module;
use beam_term::{cmp, make_atom, make_small, AtomTable, Eterm, NIL};

/// The frame `m:f/arity`, with `location` when given.
fn frame(arity: u8, location: Option<SourceLocation>) -> StackFrame {
    StackFrame {
        location,
        ..StackFrame::new(AtomName::new("m"), AtomName::new("f"), arity)
    }
}

/// The `m.erl:7` source location.
fn location() -> SourceLocation {
    SourceLocation {
        file: AtomName::new("m.erl"),
        line: 7,
    }
}

/// A `Module` whose atom table is a **clone** of `shared` — the shape a host whose
/// atoms live in a shared runtime namespace must pass (a clone preserves every
/// index, so the returned term's atom words are the shared namespace's).
fn module_over(shared: &AtomTable) -> Module {
    Module {
        atom_table: shared.clone(),
        ..Module::default()
    }
}

/// The cooked `[{M,F,A,Location}]` list `shared` names, built from the public term
/// builders on `ctx`.
fn expected(ctx: &mut MinimalContext, shared: &AtomTable, arity: i64, location: Eterm) -> Eterm {
    let atom = |name: &str| make_atom(shared.lookup(name).expect("the shared table names it"));
    let item = beam_engine::terms::make_tuple(
        ctx,
        &[atom("m"), atom("f"), make_small(arity), location],
    )
    .expect("the item tuple fits the context heap");
    beam_engine::terms::make_list(ctx, &[item]).expect("the expected list fits the context heap")
}

#[test]
fn the_root_re_export_cooks_a_supplied_trace_for_an_external_caller() {
    // The host interns the frames' names in its shared table (on `beam_rt`'s `-eval`
    // route the expression parser does this) and passes a module carrying it.
    let mut shared = AtomTable::new();
    for name in ["m", "f", "file", "line"] {
        shared.intern(name);
    }
    let module = module_over(&shared);

    let frames = StackTrace {
        frames: vec![frame(1, Some(location()))],
    };
    let mut ctx = MinimalContext::new();
    let cooked = beam_engine::encode_supplied_stacktrace(&module, &mut ctx, &frames)
        .expect("the shared table names every atom the frame uses");

    let mut expected_ctx = MinimalContext::new();
    let chars: Vec<Eterm> = "m.erl".chars().map(|ch| make_small(ch as i64)).collect();
    let name =
        beam_engine::terms::make_list(&mut expected_ctx, &chars).expect("the char list fits");
    let file_pair =
        beam_engine::terms::make_tuple(&mut expected_ctx, &[make_atom(shared.lookup("file").unwrap()), name])
            .expect("the {file,Name} pair fits");
    let line_pair = beam_engine::terms::make_tuple(
        &mut expected_ctx,
        &[make_atom(shared.lookup("line").unwrap()), make_small(7)],
    )
    .expect("the {line,N} pair fits");
    let location =
        beam_engine::terms::make_list(&mut expected_ctx, &[file_pair, line_pair]).expect("location");
    let expected = expected(&mut expected_ctx, &shared, 1, location);

    assert!(
        cmp(cooked, expected, &shared).is_eq(),
        "the crate-root encoder must cook [{{m,f,1,[{{file,\"m.erl\"}},{{line,7}}]}}]"
    );
}

#[test]
fn the_prelude_and_the_machine_method_reach_the_same_encoder() {
    let mut shared = AtomTable::new();
    for name in ["m", "f"] {
        shared.intern(name);
    }
    let module = module_over(&shared);
    let frames = StackTrace {
        frames: vec![frame(0, None)],
    };

    // The prelude glob's free function …
    let mut prelude_ctx = MinimalContext::new();
    let via_prelude = beam_engine::prelude::encode_supplied_stacktrace(&module, &mut prelude_ctx, &frames)
        .expect("cooked");
    // … and the `Machine` method, are the same encoder.
    let mut machine = Machine::new();
    let mut machine_ctx = MinimalContext::new();
    let via_machine = machine
        .encode_supplied_stacktrace(&module, &mut machine_ctx, &frames)
        .expect("cooked");

    let mut expected_ctx = MinimalContext::new();
    let expected = expected(&mut expected_ctx, &shared, 0, NIL);
    assert!(
        cmp(via_prelude, expected, &shared).is_eq(),
        "the prelude encoder must cook [{{m,f,0,[]}}]"
    );
    assert!(
        cmp(via_machine, expected, &shared).is_eq(),
        "the `Machine` method must cook the same term"
    );
}

#[test]
fn a_name_the_passed_table_lacks_is_the_typed_unsupported_error() {
    // The host passed a table that names `m` but not `f`: the encoder reports the
    // typed `unsupported`, naming the atom it could not resolve, and the caller
    // gets no cooked term at all — an atom the caller did not name is never
    // fabricated.
    let mut shared = AtomTable::new();
    shared.intern("m");
    let module = module_over(&shared);
    let frames = StackTrace {
        frames: vec![frame(0, None)],
    };
    let mut ctx = MinimalContext::new();
    let error = beam_engine::encode_supplied_stacktrace(&module, &mut ctx, &frames)
        .expect_err("a frame naming an atom the table lacks must not cook");
    let raise = error.into_raise().expect("the failure is a BEAM raise");
    assert_eq!(raise.class, ExceptionClass::Error, "the class of the failure");
    assert_eq!(
        raise.reason.as_atom(),
        Some(
            "unsupported_opcode: build_stacktrace: supplied frame names a function atom the \
             running module cannot resolve: f"
        ),
        "the encoder names the atom it could not resolve"
    );
}