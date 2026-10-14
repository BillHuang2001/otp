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

//! A raise that **arrives carrying explicit symbolic frames** renders THOSE frames
//! in a caught handler's cooked stacktrace.
//!
//! The oracle replaces the derived trace with the caller-supplied one: `erlang:raise/3`
//! whose third argument is a non-empty trace list makes `try … catch _C:_R:S -> S end`
//! bind `S` to that list **verbatim** —
//!
//! ```text
//! 1> try erlang:raise(error, boom, [{my,fake,2,[{file,"x.erl"},{line,7}]}])
//! 1>     catch _C:_R:S -> S end.
//! [{my,fake,2,[{file,"x.erl"},{line,7}]}]
//! ```
//!
//! This suite drives that shape through the engine: a synthetic module's `try` body
//! calls an import the mock host cannot resolve, and the host's pluggable BIF hook
//! ([`MinimalContext::with_bif_hook`]) raises a
//! [`BeamError::Raise`] carrying [`Raise::with_stacktrace`] frames. The handler's
//! `build_stacktrace` must cook the **supplied** frames, not the engine's
//! `raise_snapshot` heuristic.
//!
//! A raise whose supplied trace is **empty** (or absent) must leave every path
//! byte-identical to today's `raise_snapshot`-derived output — pinned by
//! [`an_empty_supplied_trace_is_byte_identical_to_no_supplied_trace`].
//!
//! Every name a supplied frame needs (`my`, `fake`, the `file`/`line` location keys)
//! is resolved from the running module's atom table and is **never substituted**: a
//! name it cannot intern is a typed `unsupported` — pinned by
//! [`a_supplied_frame_naming_an_unknown_atom_is_a_typed_error`].
//!
//! The oracle-backed case **skips loudly** when no runnable `erl` is on `PATH`
//! (the discipline of `tests/trace.rs::oracle_eval`).

use std::collections::BTreeMap;
use std::process::Command;

use beam_common::{AtomName, BeamError, ExitReason, Raise, SourceLocation, StackFrame, StackTrace};
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{
    Atom, Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand,
};
use beam_term::{cmp, make_atom, make_small, Eterm, NIL};

// ---------------------------------------------------------------------------
// The synthetic module: `run/0` wraps an unresolvable import in a live `try`
// ---------------------------------------------------------------------------

/// The label→index map the loader would build over a hand-written `code` vec.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A synthetic module whose `run/0` is
/// `try host:target() catch _:_:S -> S end` with the third binding built by
/// `build_stacktrace` — the compiler's handler shape (C:
/// `move {x,2} -> {x,0}` then `build_stacktrace`).
struct Fixture {
    module: Module,
}

impl Fixture {
    /// The atoms the fixture interns, in a fixed order (so two fixtures assign the
    /// same index to the same name): the function name, the class/reason, the
    /// supplied frame's names, and the unresolved import's target.
    const SEED: &'static [&'static str] = &[
        "run",
        "error",
        "boom",
        "my",
        "fake",
        "file",
        "line",
        "host",
        "target",
    ];

    fn new() -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("raise_with_trace");
        module.atom_table.seed(Self::SEED.iter().copied());
        let host = module.atom_table.lookup("host").expect("seeded");
        let target = module.atom_table.lookup("target").expect("seeded");
        module.imports.push(Import {
            module: host,
            function: target,
            arity: 0,
        });
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function; returns its entry label (the first label in `code`).
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

    /// The `run/0` body: a live `try` around `call_ext host:target/0`, whose handler
    /// lands the exception, moves the raw trace into `x(0)` and cooks it.
    fn run_body() -> Vec<Instr> {
        vec![
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
                target: ImportRef(0),
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
            // The compiled handler moves the exception's raw trace (x(2)) into x(0),
            // which is the operand `build_stacktrace` cooks.
            Instr::Move {
                src: Operand::X(2),
                dst: Operand::X(0),
            },
            Instr::BuildStacktrace,
            Instr::Deallocate(1),
            Instr::Return,
        ]
    }

    /// Build the module with `run/0` and its export.
    fn build() -> (Module, Atom) {
        let mut fixture = Fixture::new();
        let run_atom = fixture.atom("run");
        let entry = fixture.add_function("run", 0, Self::run_body());
        fixture.module.exports.push(Export {
            function: run_atom,
            arity: 0,
            label: entry,
        });
        (fixture.module, run_atom)
    }
}

// ---------------------------------------------------------------------------
// The raise the host's BIF hook returns
// ---------------------------------------------------------------------------

/// `error:boom`, optionally carrying the explicit symbolic frames `trace`.
fn raise_with(trace: Option<StackTrace>) -> BeamError {
    let raise = Raise::error(ExitReason::atom("boom"));
    match trace {
        Some(trace) => BeamError::Raise(raise.with_stacktrace(trace)),
        None => BeamError::Raise(raise),
    }
}

/// The supplied frames of the oracle example:
/// `[{my,fake,2,[{file,"x.erl"},{line,7}]}]`.
fn supplied_frames() -> StackTrace {
    let mut frame = StackFrame::new("my", "fake", 2);
    frame.location = Some(SourceLocation {
        file: AtomName::new("x.erl"),
        line: 7,
    });
    let mut trace = StackTrace::new();
    trace.push(frame);
    trace
}

/// Run `run/0` on a fresh machine + host whose BIF hook raises `error:boom` with
/// `trace`, and return the handler's cooked `S` (kept alive by the returned host).
fn run_caught(trace: Option<StackTrace>) -> (Eterm, Module, MinimalContext) {
    let (module, run_atom) = Fixture::build();
    let mut ctx =
        MinimalContext::with_bif_hook(Box::new(move |_, _, _, _| Err(raise_with(trace.clone()))));
    let mut machine = Machine::new();
    let cooked = machine
        .run_export(&module, run_atom, 0, &[], &mut ctx)
        .expect("the live catch frame returns the cooked S");
    assert_eq!(machine.catch_frame_depth(), 0, "try_case consumed the frame");
    (cooked, module, ctx)
}

// ---------------------------------------------------------------------------
// Expected terms, built from the oracle's printed shape
// ---------------------------------------------------------------------------

/// An Erlang string (a list of code points) as a term.
fn chars(ctx: &mut MinimalContext, text: &str) -> Eterm {
    let codes: Vec<Eterm> = text.chars().map(|c| make_small(c as i64)).collect();
    terms::make_list(ctx, &codes).expect("a char list")
}

/// `[{my,fake,2,[{file,"x.erl"},{line,7}]}]` — the supplied-frame list the oracle
/// prints, built from `module`'s own atoms.
fn expected_supplied(ctx: &mut MinimalContext, module: &Module) -> Eterm {
    let file_atom = make_atom(module.atom_table.lookup("file").expect("seeded"));
    let line_atom = make_atom(module.atom_table.lookup("line").expect("seeded"));
    let file_name = chars(ctx, "x.erl");
    let file_pair = terms::make_tuple(ctx, &[file_atom, file_name]).expect("a tuple");
    let line_pair = terms::make_tuple(ctx, &[line_atom, make_small(7)]).expect("a tuple");
    let location = terms::make_list(ctx, &[file_pair, line_pair]).expect("a list");
    let item = terms::make_tuple(
        ctx,
        &[
            make_atom(module.atom_table.lookup("my").expect("seeded")),
            make_atom(module.atom_table.lookup("fake").expect("seeded")),
            make_small(2),
            location,
        ],
    )
    .expect("a tuple");
    terms::make_list(ctx, &[item]).expect("a list")
}

/// `[{raise_with_trace,run,0,[]}]` — the single raising frame the
/// `raise_snapshot` heuristic derives for the `call_ext` that raised (the module
/// name, the enclosing `run/0` and no source location).
fn expected_snapshot(ctx: &mut MinimalContext, module: &Module) -> Eterm {
    let run_atom = make_atom(module.atom_table.lookup("run").expect("seeded"));
    let item = terms::make_tuple(ctx, &[make_atom(module.name), run_atom, make_small(0), NIL])
        .expect("a tuple");
    terms::make_list(ctx, &[item]).expect("a list")
}

// ===========================================================================
// 1. The supplied frames are rendered verbatim
// ===========================================================================

#[test]
fn a_raise_carrying_frames_renders_them_in_a_caught_handler() {
    // The supplied frame — `{my,fake,2,[{file,"x.erl"},{line,7}]}` — replaces the
    // engine's `raise_snapshot` heuristic entirely: the cooked `S` is exactly the
    // supplied list, and the raising frame the snapshot would have named (`run/0`)
    // is absent.
    let (cooked, module, _host) = run_caught(Some(supplied_frames()));
    let mut build = MinimalContext::new();
    let expected = expected_supplied(&mut build, &module);
    assert!(
        cmp(cooked, expected, &module.atom_table).is_eq(),
        "the caught S must be the supplied frames verbatim: got {cooked:?}"
    );
    let snapshot = expected_snapshot(&mut build, &module);
    assert!(
        !cmp(cooked, snapshot, &module.atom_table).is_eq(),
        "the snapshot-derived trace must NOT be rendered when frames were supplied"
    );
}

#[test]
fn a_supplied_frame_without_a_location_renders_an_empty_location() {
    // A frame with no `SourceLocation` cooks to the empty-list `Location`, exactly
    // as the oracle prints `{m,f,1,[]}` for a supplied item with no location pair.
    let mut frame = StackFrame::new("my", "fake", 1);
    frame.location = None;
    let mut trace = StackTrace::new();
    trace.push(frame);

    let (cooked, module, _host) = run_caught(Some(trace));
    let mut build = MinimalContext::new();
    let item = terms::make_tuple(
        &mut build,
        &[
            make_atom(module.atom_table.lookup("my").expect("seeded")),
            make_atom(module.atom_table.lookup("fake").expect("seeded")),
            make_small(1),
            NIL,
        ],
    )
    .expect("a tuple");
    let expected = terms::make_list(&mut build, &[item]).expect("a list");
    assert!(
        cmp(cooked, expected, &module.atom_table).is_eq(),
        "a location-less supplied frame cooks to {{m,f,1,[]}}: got {cooked:?}"
    );
}

// ===========================================================================
// 2. Regression: an empty supplied trace is byte-identical to today's output
// ===========================================================================

#[test]
fn an_empty_supplied_trace_is_byte_identical_to_no_supplied_trace() {
    // Two runs of the SAME synthetic module, differing only in whether the raise
    // carried an explicitly EMPTY `StackTrace`: an empty trace arms nothing, so the
    // two cooked traces are byte-identical.
    let (without, module, _keep_without) = run_caught(None);
    let (empty, module_b, _keep_empty) = run_caught(Some(StackTrace::new()));
    assert!(
        cmp(without, empty, &module.atom_table).is_eq(),
        "an empty supplied trace must be byte-identical to no supplied trace"
    );
    assert!(
        cmp(without, empty, &module_b.atom_table).is_eq(),
        "the two fixtures agree under either module's atom table"
    );

    // ... and both are the `raise_snapshot`-derived single raising frame (`run/0`),
    // proving the empty case keeps today's heuristic.
    let mut build = MinimalContext::new();
    let snapshot = expected_snapshot(&mut build, &module);
    for (label, cooked) in [("no trace", without), ("empty trace", empty)] {
        assert!(
            cmp(cooked, snapshot, &module.atom_table).is_eq(),
            "{label}: the cooked S must stay the snapshot-derived trace: got {cooked:?}"
        );
    }
}

// ===========================================================================
// 3. A supplied frame naming an atom the module cannot intern is a typed error
// ===========================================================================

#[test]
fn a_supplied_frame_naming_an_unknown_atom_is_a_typed_error() {
    // `ghost` is not in the module's atom table (the engine cannot intern atoms, and
    // substituting one would be a wrong result), so the cooked trace is the typed
    // `unsupported`, never a fabricated frame.
    let mut frame = StackFrame::new("ghost", "fake", 1);
    frame.location = None;
    let mut trace = StackTrace::new();
    trace.push(frame);

    let (module, run_atom) = Fixture::build();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(move |_, _, _, _| {
        Err(raise_with(Some(trace.clone())))
    }));
    let mut machine = Machine::new();
    let err = machine
        .run_export(&module, run_atom, 0, &[], &mut ctx)
        .expect_err("a supplied frame naming an uninterned atom is a typed error");
    match err {
        BeamError::Raise(raise) => {
            let atom = raise.reason.as_atom().expect("an atom reason");
            assert!(
                atom.starts_with(
                    "unsupported_opcode: build_stacktrace: supplied frame names a module atom"
                ),
                "got {atom}"
            );
        }
        other => panic!("expected a typed unsupported for the unknown atom, got {other:?}"),
    }
}

// ===========================================================================
// 4. Oracle cross-check (loud skip when `erl` is absent)
// ===========================================================================

/// Run `expr` under the system `erl` and return its stdout trimmed, or `None` when
/// no runnable `erl` could be spawned (the caller skips loudly).
///
/// A spawned `erl` that exits non-zero is a **harness** failure, not "no oracle":
/// it panics with the captured stderr, so a broken oracle can never masquerade as a
/// silent skip — the discipline of `tests/trace.rs::oracle_eval`.
fn oracle(expr: &str) -> Option<String> {
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-eval")
        .arg(expr)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for `{expr}`\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
fn the_oracle_replaces_the_derived_trace_with_the_supplied_one() {
    // The canonical case the suite models: the oracle's `S` is the supplied list
    // verbatim (`{my,fake,2,[{file,"x.erl"},{line,7}]}`), which is the shape
    // `expected_supplied` builds and the engine renders.
    let expr = "try erlang:raise(error, boom, [{my,fake,2,[{file,\"x.erl\"},{line,7}]}]) \
                catch _C:_R:S -> io:format(\"~p\", [S]) end, halt().";
    match oracle(expr) {
        Some(actual) => assert_eq!(
            actual,
            "[{my,fake,2,[{file,\"x.erl\"},{line,7}]}]",
            "the oracle returns the caller-supplied trace verbatim"
        ),
        None => eprintln!(
            "SKIP raise_with_trace: no system `erl` on PATH — the oracle cross-check \
             was skipped"
        ),
    }
}

#[test]
fn the_oracle_replaces_the_derived_trace_the_engine_heuristic_would_have_produced() {
    // Contrast: a raise that carries NO trace (`erlang:raise/3` with `[]`) is not an
    // oracle value this suite pins — the engine deliberately keeps its existing
    // `raise_snapshot` heuristic for an empty supplied trace, so only the NON-EMPTY
    // case is cross-checked here.
    let expr = "try erlang:raise(error, boom, []) catch _C:_R:S -> io:format(\"~w\", [S]) end, halt().";
    match oracle(expr) {
        Some(actual) => assert_eq!(
            actual, "[]",
            "the oracle's empty-trace answer (the engine's documented divergence)"
        ),
        None => eprintln!(
            "SKIP raise_with_trace: no system `erl` on PATH — the empty-trace oracle \
             observation was skipped"
        ),
    }
}
