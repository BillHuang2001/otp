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

//! The public `beam_common::StackTrace` → `Eterm` encoder
//! ([`crate::encode_supplied_stacktrace`] / the
//! [`Machine::encode_supplied_stacktrace`](crate::machine::Machine::encode_supplied_stacktrace)
//! twin).
//!
//! The encoder is a **public wrapper** over the engine's one
//! `carve_supplied_frames`, so these tests pin three things: the cooked
//! `[{Module,Function,Arity,Location}]` layout (against the system `erl` oracle and
//! against a hand-built expected term), that the two entries agree, and that the
//! public path produces **exactly** what the private carve produces for the same
//! frames. The unresolvable-atom contract (`error::unsupported`, never a substituted
//! atom) is pinned by its own test.
//!
//! The oracle test **skips loudly** (`eprintln!`) when no system `erl` is on `PATH`,
//! mirroring [`super::oracle`].
use super::*;

use super::oracle::{oracle_trace, render_item};

use beam_common::{AtomName, SourceLocation, StackFrame, StackTrace};
use beam_loader::Module;
use beam_term::{cmp, make_atom, make_small, Eterm, NIL};

// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity. (An explicit
// import shadows the glob-imported `terms` above.)
use crate::terms::test_support as terms;

// ------------------------------------------------------------------
// Fixtures
// ------------------------------------------------------------------

/// A module whose atom table names `m` (its own name), `f` (the frame's function)
/// and the `file`/`line` location keys — everything the encoded frames need.
fn module_with_frame_atoms() -> Module {
    Fixture::new(&["f", "file", "line"]).finish()
}

/// The frame `m:f/arity` with no source location.
fn frame(arity: u8) -> StackFrame {
    StackFrame::new(AtomName::new("m"), AtomName::new("f"), arity)
}

/// The frame `m:f/arity` carrying the source location `m.erl:7`.
fn located_frame(arity: u8) -> StackFrame {
    StackFrame {
        location: Some(SourceLocation {
            file: AtomName::new("m.erl"),
            line: 7,
        }),
        ..frame(arity)
    }
}

/// The one-frame trace `[frame]`.
fn trace_of(frame: StackFrame) -> StackTrace {
    StackTrace {
        frames: vec![frame],
    }
}

/// The atom `name` of `module`, or a harness panic.
fn atom(module: &Module, name: &str) -> beam_loader::Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("the fixture module does not name `{name}`"))
}

/// The cooked item `{Module,Function,Arity,Location}` for `m:f/arity` with the
/// already-built `location` term — the hand-built expectation the cooked output is
/// compared against.
fn item(ctx: &mut MinimalContext, module: &Module, arity: i64, location: Eterm) -> Eterm {
    terms::make_tuple(
        ctx,
        &[
            make_atom(atom(module, "m")),
            make_atom(atom(module, "f")),
            make_small(arity),
            location,
        ],
    )
    .expect("the item tuple fits the test arena")
}

/// The `[{file,"m.erl"},{line,7}]` location term the encoder must build for a frame
/// whose location is `m.erl:7` (the file name is a char list, as the oracle prints
/// it).
fn located_term(ctx: &mut MinimalContext, module: &Module) -> Eterm {
    let chars: Vec<Eterm> = "m.erl".chars().map(|ch| make_small(ch as i64)).collect();
    let name = terms::make_list(ctx, &chars).expect("the char list fits the test arena");
    let file_pair = terms::make_tuple(ctx, &[make_atom(atom(module, "file")), name])
        .expect("the {file,Name} pair fits the test arena");
    let line_pair = terms::make_tuple(ctx, &[make_atom(atom(module, "line")), make_small(7)])
        .expect("the {line,N} pair fits the test arena");
    terms::make_list(ctx, &[file_pair, line_pair]).expect("the location list fits the test arena")
}

/// The whole cooked list for the single-frame `frames` trace, rendered exactly like
/// [`super::oracle::oracle_trace`] renders the oracle's items.
fn rendered(cooked: Eterm, module: &Module) -> Vec<String> {
    crate::terms::list_elements(cooked)
        .expect("the cooked supplied trace is a proper list")
        .iter()
        .map(|item| render_item(*item, module))
        .collect()
}

// ------------------------------------------------------------------
// The cooked layout
// ------------------------------------------------------------------

#[test]
fn the_free_encoder_cooks_a_frame_without_a_location() {
    // `catch erlang:raise(error, r, [{m,f,0}])` renders `{m,f,0,[]}` — the location
    // slot is the empty list when the supplied frame carries none.
    let module = module_with_frame_atoms();
    let mut ctx = MinimalContext::new();
    let cooked =
        crate::encode_supplied_stacktrace(&module, &mut ctx, &trace_of(frame(0))).expect("cooked");

    let mut expected_ctx = MinimalContext::new();
    let expected_item = item(&mut expected_ctx, &module, 0, NIL);
    let expected = terms::make_list(&mut expected_ctx, &[expected_item])
        .expect("the expected list fits the test arena");
    assert!(
        cmp(cooked, expected, &module.atom_table).is_eq(),
        "the cooked supplied trace must be [{{m,f,0,[]}}]"
    );
}

#[test]
fn the_free_encoder_cooks_a_frame_with_a_location() {
    // A supplied frame's `Location` is the `[{file,Name},{line,N}]` pair the frame's
    // `SourceLocation` describes, with the file name a char list — byte-identical to
    // the oracle's.
    let module = module_with_frame_atoms();
    let mut ctx = MinimalContext::new();
    let cooked =
        crate::encode_supplied_stacktrace(&module, &mut ctx, &trace_of(located_frame(1)))
            .expect("cooked");

    let mut expected_ctx = MinimalContext::new();
    let location = located_term(&mut expected_ctx, &module);
    let expected_item = item(&mut expected_ctx, &module, 1, location);
    let expected = terms::make_list(&mut expected_ctx, &[expected_item])
        .expect("the expected list fits the test arena");
    assert!(
        cmp(cooked, expected, &module.atom_table).is_eq(),
        "the cooked supplied trace must be [{{m,f,1,[{{file,\"m.erl\"}},{{line,7}}]}}]"
    );
}

// ------------------------------------------------------------------
// The two entries agree, and both are the private carve
// ------------------------------------------------------------------

#[test]
fn the_machine_method_agrees_with_the_free_encoder() {
    let module = module_with_frame_atoms();
    for frames in [trace_of(frame(0)), trace_of(located_frame(3))] {
        let mut free_ctx = MinimalContext::new();
        let free =
            crate::encode_supplied_stacktrace(&module, &mut free_ctx, &frames).expect("cooked");
        let mut machine = Machine::new();
        let mut method_ctx = MinimalContext::new();
        let method = machine
            .encode_supplied_stacktrace(&module, &mut method_ctx, &frames)
            .expect("cooked");
        assert!(
            cmp(free, method, &module.atom_table).is_eq(),
            "the free encoder and the `Machine` method must cook the same term"
        );
    }
}

#[test]
fn the_public_encoder_cooks_exactly_what_the_private_supplied_carve_does() {
    // The public entry points are wrappers over the engine's one carve: for the same
    // frames they must produce the same term, so a host that owns no machine cannot
    // get a different layout than a running compiled handler does.
    let module = module_with_frame_atoms();
    for frames in [trace_of(frame(0)), trace_of(located_frame(2))] {
        let mut public_ctx = MinimalContext::new();
        let public =
            crate::encode_supplied_stacktrace(&module, &mut public_ctx, &frames).expect("cooked");
        let mut machine = Machine::new();
        let mut private_ctx = MinimalContext::new();
        let private = crate::families::exceptions::stacktrace::carve_supplied_frames(
            &mut machine,
            &module,
            &mut private_ctx,
            &frames,
        )
        .expect("cooked");
        assert!(
            cmp(public, private, &module.atom_table).is_eq(),
            "the public encoder must cook the private carve's own term"
        );
    }
}

// ------------------------------------------------------------------
// The oracle
// ------------------------------------------------------------------

#[test]
fn the_public_encoder_cooks_what_the_system_oracle_prints() {
    // The oracle probe builds the `{Class,Reason,Stacktrace}` triple the shared
    // helper prints: the cooked list of a **caught** `erlang:raise/3` is the second
    // element of the raise reason (`{Reason, Stacktrace}`), i.e. the frames the
    // caller supplied, verbatim.
    let dir = std::env::temp_dir();
    let plain = oracle_trace(
        &dir,
        "{error, r, element(2, element(2, (catch erlang:raise(error, r, [{m,f,0}]))))}",
    );
    let located = oracle_trace(
        &dir,
        "{error, r, element(2, element(2, (catch erlang:raise(error, r, \
         [{m,f,1,[{file,\"m.erl\"},{line,7}]}]))))}",
    );
    let (Some(plain), Some(located)) = (plain, located) else {
        eprintln!("SKIP: no system `erl` on PATH -- the public-encoder oracle check is skipped");
        return;
    };
    // A sanity check on the probe itself: the class/reason line the oracle prints.
    assert_eq!(plain.head, "error|r", "the oracle probe's class|reason line");
    assert_eq!(located.head, "error|r", "the oracle probe's class|reason line");

    let module = module_with_frame_atoms();
    let mut plain_ctx = MinimalContext::new();
    let cooked = crate::encode_supplied_stacktrace(&module, &mut plain_ctx, &trace_of(frame(0)))
        .expect("cooked");
    assert_eq!(
        rendered(cooked, &module),
        plain.items,
        "the cooked supplied trace disagrees with the oracle"
    );

    let mut located_ctx = MinimalContext::new();
    let cooked =
        crate::encode_supplied_stacktrace(&module, &mut located_ctx, &trace_of(located_frame(1)))
            .expect("cooked");
    assert_eq!(
        rendered(cooked, &module),
        located.items,
        "the cooked located supplied trace disagrees with the oracle"
    );
}

// ------------------------------------------------------------------
// The unresolvable-atom contract
// ------------------------------------------------------------------

#[test]
fn a_name_the_module_cannot_resolve_is_a_typed_unsupported_error() {
    // The module names `m` (its own name) but not `f`: the encoder must report the
    // typed `unsupported` — never substitute an atom the caller did not name, which
    // would be a WRONG term.
    let module = Fixture::new(&[]).finish();
    let mut ctx = MinimalContext::new();
    let error = crate::encode_supplied_stacktrace(&module, &mut ctx, &trace_of(frame(0)))
        .expect_err("a frame naming an atom the module lacks must not cook");
    let BeamError::Raise(raise) = &error else {
        panic!("expected the typed `unsupported` raise, got {error:?}");
    };
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
