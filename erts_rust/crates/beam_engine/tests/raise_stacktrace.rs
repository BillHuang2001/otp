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
//! The **stacktrace frames** an *uncaught* raise carries out of the machine.
//!
//! A raise leaves the machine as a bare `BeamError::Raise` whose reason names
//! neither the module nor the function that raised it (`error(badarg)`, the bare
//! atom `function_clause`, …). C captures the frames as the raw `StackTrace` object
//! hung off the exception (`c_p->ftrace`); the engine derives them from its live
//! call frames at raise time
//! ([`Machine::raise_stacktrace`](beam_engine::Machine::raise_stacktrace)) and
//! attaches them as the raise exits the machine
//! ([`Machine::with_uncaught_frames`](beam_engine::Machine)). The order is C's
//! (`build_stacktrace`): the **raising** function first, then every live caller,
//! innermost first.
//!
//! This suite is the regression gate for that derivation. It pins, per case:
//!
//! * **(a)** an uncaught `erlang:error(badarg)` in a real `erlc`-built module — the
//!   raising function then the caller that made the non-tail call;
//! * **(b)** a raise inside an in-machine cross-module `call_ext*` callee (reached
//!   through the [`ExecutionContext::resolve_external_call`] seam) — the **callee's**
//!   frame first, then the caller's, each named against its own module;
//! * **(c)** a raise inside a `call_fun` closure body (reached through the
//!   [`ExecutionContext::resolve_local_fun`] seam) — the lambda frame first, then
//!   the caller's;
//! * **(d)** TIER EQUIVALENCE — the native baseline tier derives **exactly** the
//!   interpreter's [`StackTrace`](beam_common::StackTrace) (locations included) for
//!   the same uncaught raise;
//! * **(e)** the documented GAP — an uncaught **bare** BIF error (`BeamError::BadArg`,
//!   no `Raise`, no catch frame) is **not** decorated: it propagates as its own
//!   variant, with no frames at all, because the funnel
//!   ([`Machine::with_uncaught_frames`](beam_engine::Machine)) only decorates a
//!   `BeamError::Raise`.
//!
//! The two real-module fixtures (`raise_frames`, `xmod_caller`/`xmod_callee`) are
//! committed `.beam`s produced by the system `erlc +deterministic` (see
//! `tests/fixtures/README.md`); the closure fixtures are hand-built module IR (the
//! loader is not involved). The oracle cross-check at the bottom re-derives the
//! semantics from the real ERTS and **skips loudly** when no `erl` is on `PATH`.

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;

use beam_common::{BeamError, ExceptionClass, ExitReason, Raise};
use beam_engine::context::BifHook;
use beam_engine::{ExecutionContext, Machine, MinimalContext, Tier};
use beam_loader::{
    load_file, Export, FunDescriptor, Function, HeapNeed, Import, ImportRef, Instr, Label, Module,
    Operand,
};
use beam_term::{make_small, Atom, Eterm};

// ---------------------------------------------------------------------------
// The committed oracle fixtures (produced by `erlc +deterministic`)
// ---------------------------------------------------------------------------

/// The `.beam` path of the fixture module `name` (committed under `tests/fixtures/`).
fn fixture_path(name: &str) -> String {
    format!("{}/tests/fixtures/{name}.beam", env!("CARGO_MANIFEST_DIR"))
}

/// Load a committed fixture by name.
fn load_fixture(name: &str) -> Module {
    let path = fixture_path(name);
    load_file(&path).unwrap_or_else(|error| panic!("load {path}: {error:?}"))
}

// ---------------------------------------------------------------------------
// The BIF floor
// ---------------------------------------------------------------------------

/// A BIF floor that models `erlang:error/1` by raising class `error` with the bare
/// atom `badarg` — exactly the shape `beam_bifs` produces for an atom reason (no
/// live reason term, so the *frames* are the only thing that localizes it). Every
/// other BIF is the typed `unsupported` capability gap.
fn raising_error_bif() -> BifHook {
    Box::new(|module, function, arity, _args| {
        if module == "erlang" && function == "error" && arity == 1 {
            return Err(BeamError::Raise(Raise::error(ExitReason::atom("badarg"))));
        }
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    })
}

/// A BIF floor that raises class `error` (bare atom `badarg`) for **any**
/// `erlang:*` call — the contrast hook of case (e): the same call site that
/// propagates a bare [`BeamError::BadArg`] verbatim is decorated when it produces a
/// [`BeamError::Raise`] instead.
fn raising_bif_for_any_erlang_call() -> BifHook {
    Box::new(|module, function, _arity, _args| {
        if module == "erlang" {
            return Err(BeamError::Raise(Raise::error(ExitReason::atom("badarg"))));
        }
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    })
}

// ---------------------------------------------------------------------------
// Running to an uncaught raise
// ---------------------------------------------------------------------------

/// The `(module, function, arity)` triples of a raise's frames, in order.
fn frames(raise: &Raise) -> Vec<(&str, &str, u8)> {
    raise
        .stacktrace
        .frames
        .iter()
        .map(|frame| (frame.module.as_str(), frame.function.as_str(), frame.arity))
        .collect()
}

/// Run `module:function/arity` with `args` and require an uncaught raise; return
/// the raised [`Raise`] (frames included).
fn uncaught_raise(
    module: &Module,
    function: &str,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Raise {
    let function = module
        .atom_table
        .lookup(function)
        .expect("the function atom is interned in the module");
    let arity = args.len() as u32;
    let mut machine = Machine::new();
    let err = machine
        .run_export_outcome(module, function, arity, args, ctx)
        .expect_err("the raise must be uncaught");
    err.into_raise().expect("a raise, not a capability gap")
}

// ---------------------------------------------------------------------------
// (a) an uncaught `erlang:error/1` in a real `erlc`-built module
// ---------------------------------------------------------------------------

#[test]
fn an_uncaught_error_raise_names_the_raising_function_then_its_caller() {
    // `raise_frames:outer(1)` makes a non-tail call into `boom/1`, whose `_` clause
    // raises `erlang:error(badarg)`. The reason names no function, so the frames
    // are the only thing that localizes the failure: `boom/1` first, then `outer/1`.
    let module = load_fixture("raise_frames");
    let mut ctx = MinimalContext::with_bif_hook(raising_error_bif());
    let raise = uncaught_raise(&module, "outer", &[make_small(1)], &mut ctx);

    assert_eq!(raise.class, ExceptionClass::Error);
    assert_eq!(
        raise.reason.as_atom(),
        Some("badarg"),
        "the reason is the bare atom `badarg`, exactly what `erlang:error/1` raised"
    );
    assert_eq!(
        frames(&raise),
        vec![("raise_frames", "boom", 1), ("raise_frames", "outer", 1)],
        "the raising function first, then its live non-tail caller"
    );
}

// ---------------------------------------------------------------------------
// (b) a raise inside an in-machine cross-module `call_ext*` callee
// ---------------------------------------------------------------------------

#[test]
fn a_raise_inside_a_cross_module_callee_names_the_callee_then_the_caller() {
    // `xmod_caller:run/1` makes a non-tail `call_ext` into `xmod_callee:boom/1`,
    // which the external resolver runs **in this machine** (its own module context).
    // The raise must name the callee's frame first (`xmod_callee:boom/1`), then the
    // caller's (`xmod_caller:run/1`) — each against its own module.
    let caller = load_fixture("xmod_caller");
    let callee = Arc::new(load_fixture("xmod_callee"));
    let mut ctx = MinimalContext::with_bif_hook(raising_error_bif());
    ctx.set_external_resolver(Box::new(move |module, function, arity| {
        if module != "xmod_callee" {
            return Ok(None);
        }
        let Some(function) = callee.atom_table.lookup(function) else {
            return Ok(None);
        };
        Ok(callee
            .export(function, arity)
            .map(|label| (Arc::clone(&callee), label)))
    }));

    let raise = uncaught_raise(&caller, "run", &[make_small(1)], &mut ctx);

    assert_eq!(raise.class, ExceptionClass::Error);
    assert_eq!(raise.reason.as_atom(), Some("badarg"));
    assert_eq!(
        frames(&raise),
        vec![("xmod_callee", "boom", 1), ("xmod_caller", "run", 1)],
        "the callee's frame first, then the caller's, each in its own module"
    );
}

// ---------------------------------------------------------------------------
// Hand-built synthetic modules for (c) and (e) (the loader is not involved)
// ---------------------------------------------------------------------------

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A module builder with its own atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    fn import(&mut self, module_name: &str, function_name: &str, arity: u32) -> ImportRef {
        let module = self.atom(module_name);
        let function = self.atom(function_name);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef((self.module.imports.len() - 1) as u32)
    }

    /// Register a `FunT` descriptor; its position is the fun-table index.
    fn add_fun(&mut self, name: &str, arity: u32, num_free: u32, uniq: u32) -> u32 {
        let function = self.atom(name);
        let index = self.module.funs.len() as u32;
        self.module.funs.push(FunDescriptor {
            function,
            arity,
            label: Label(index + 1),
            index,
            num_free,
            uniq,
        });
        index
    }

    /// Append a function; its entry label is the first label in `code`.
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

    fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
    }

    fn finish(self) -> Module {
        self.module
    }
}

// ---------------------------------------------------------------------------
// (c) a raise inside a `call_fun` closure body
// ---------------------------------------------------------------------------

/// `m:run/0` — builds a one-free-variable closure and calls it with `call_fun 1`,
/// exactly the shape `call1(fun(A) -> … end)` compiles to:
///
/// ```text
/// run/0:
///     allocate 0 2
///     move 7 -> x0                    ; the capture source
///     make_fun3 <funT> x1 x0          ; F = fun(A) -> … end  (num_free = 1)
///     move 10 -> x0                   ; the call argument
///     call_fun 1                      ; F(10)
///     deallocate 0
///     return
/// ```
fn closure_caller() -> Module {
    let mut f = Fixture::new("m");
    let index = f.add_fun("lambda", 2, 1, 0xCAFE);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 2,
            },
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(0),
            },
            Instr::MakeFun3 {
                index,
                dst: Operand::X(1),
                env: vec![Operand::X(0)],
            },
            Instr::Move {
                src: Operand::Integer(10),
                dst: Operand::X(0),
            },
            Instr::CallFun { arity: 1 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    f.finish()
}

/// `c:lambda_raise/2` — the closure body the seam resolves to, and its entry label.
/// A tail `call_ext_last erlang:error/1` (the compiled shape of a raising lambda):
/// a raising `call_ext_last` never pops the frame, so the caller's frame stays live
/// and the raise names the lambda first, then the caller.
fn closure_callee() -> (Arc<Module>, Label) {
    let mut f = Fixture::new("c");
    let error = f.import("erlang", "error", 1);
    let badarg = f.atom("badarg");
    let lambda = f.add_function(
        "lambda_raise",
        2,
        vec![
            Instr::Label(Label(30)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 2,
            },
            Instr::Move {
                src: Operand::Atom(badarg),
                dst: Operand::X(0),
            },
            Instr::CallExtLast {
                target: error,
                arity: 1,
                deallocate: 0,
            },
        ],
    );
    (Arc::new(f.finish()), lambda)
}

#[test]
fn a_raise_inside_a_closure_names_the_lambda_then_the_caller() {
    // The closure caller's `call_fun` is resolved **in this machine** by the local-fun
    // seam, so the lambda body runs in the caller's process: the raise must name the
    // lambda frame first (`c:lambda_raise/2`), then the caller's (`m:run/0`).
    let caller = closure_caller();
    let (callee, lambda) = closure_callee();
    let mut ctx = MinimalContext::with_bif_hook(raising_error_bif());
    ctx.set_local_fun_resolver(Box::new(move |_fun, _args| {
        Ok(Some((Arc::clone(&callee), lambda)))
    }));

    let raise = uncaught_raise(&caller, "run", &[], &mut ctx);

    assert_eq!(raise.class, ExceptionClass::Error);
    assert_eq!(raise.reason.as_atom(), Some("badarg"));
    assert_eq!(
        frames(&raise),
        vec![("c", "lambda_raise", 2), ("m", "run", 0)],
        "the lambda frame first, then the caller's"
    );
}

// ---------------------------------------------------------------------------
// (d) tier equivalence: the baseline tier derives the interpreter's frames
// ---------------------------------------------------------------------------

#[test]
fn the_baseline_tier_derives_the_same_frames_as_the_interpreter() {
    // The raise funnel runs at the boundary both the interpreter's dispatch loop and
    // the native baseline driver pass through, so a natively-executed raise must carry
    // exactly the interpreter's frames — the `(module, function, arity)` triples AND
    // the source locations (`error:badarg` is raised by the deopt-to-interpreter path
    // of `call_ext_only`, whose `(func, pc)` is exact, so the locations agree too).
    assert!(
        Tier::BaselineJit.is_available(),
        "the native baseline tier is built in"
    );
    let module = load_fixture("raise_frames");
    let outer = module
        .atom_table
        .lookup("outer")
        .expect("the fixture exports `outer`");

    let interpreted = uncaught_raise(
        &module,
        "outer",
        &[make_small(1)],
        &mut MinimalContext::with_bif_hook(raising_error_bif()),
    );

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(raising_error_bif());
    let err = machine
        .run_export_outcome_with_tier(
            &module,
            outer,
            1,
            &[make_small(1)],
            &mut ctx,
            Tier::BaselineJit,
        )
        .expect_err("the raise is uncaught on the baseline tier too");
    let baseline = err.into_raise().expect("a raise, not a capability gap");

    assert_eq!(
        baseline.stacktrace, interpreted.stacktrace,
        "the baseline tier must derive exactly the interpreter's frames (locations included)"
    );
    assert_eq!(baseline.class, interpreted.class);
    assert_eq!(baseline.reason, interpreted.reason);
    assert_eq!(
        frames(&baseline),
        vec![("raise_frames", "boom", 1), ("raise_frames", "outer", 1)],
        "and the frames are the raising function then its caller"
    );
}

// ---------------------------------------------------------------------------
// (e) the documented gap: a bare BIF error carries no frames
// ---------------------------------------------------------------------------

/// `m2:run/0` — a single tail `call_ext` into `erlang:bare/0` (the BIF the two floors
/// below answer differently).
fn bare_bif_module() -> Module {
    let mut f = Fixture::new("m2");
    let target = f.import("erlang", "bare", 0);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::CallExtOnly { target, arity: 0 },
        ],
    );
    f.add_export("run", 0, run);
    f.finish()
}

#[test]
fn an_uncaught_bare_bif_error_carries_no_stacktrace() {
    // A BIF whose failure is a **bare variant** (`BeamError::BadArg`) — no `Raise`,
    // no catch frame — is not decorated by the funnel, which only touches
    // `BeamError::Raise`. It propagates verbatim, so it carries no frames at all.
    let module = bare_bif_module();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|_m, _f, _a, _args| Err(BeamError::BadArg)));
    let mut machine = Machine::new();
    let err = machine
        .run_export_outcome(&module, module.atom_table.lookup("run").expect("interned"), 0, &[], &mut ctx)
        .expect_err("the BIF fails");
    assert!(
        matches!(err, BeamError::BadArg),
        "an uncaught bare BIF error propagates as its own variant, never as a framed \
         `Raise`; got {err:?}"
    );

    // The contrast: the SAME module and call site, but a `Raise` from the BIF, IS
    // decorated — so the missing frames above are the variant's, not the site's.
    let module = bare_bif_module();
    let raise = uncaught_raise(
        &module,
        "run",
        &[],
        &mut MinimalContext::with_bif_hook(raising_bif_for_any_erlang_call()),
    );
    assert_eq!(
        frames(&raise),
        vec![("m2", "run", 0)],
        "the same call site as a `Raise` does carry the raising frame"
    );
}

// ---------------------------------------------------------------------------
// The oracle cross-check (real ERTS), skipped loudly without `erl`
// ---------------------------------------------------------------------------

/// Run `expr` under the system `erl` with the fixture directory on the code path and
/// return its stdout, or `None` when no runnable `erl` is on `PATH` at all.
///
/// The append flag is `-pz`, never `-pa`: a fixture directory contains stand-in
/// modules (e.g. this crate's `tests/fixtures/lists.beam`) that would **shadow** the
/// real stdlib if it were prepended, leaving the oracle with `lists` undefined.
/// Appending keeps the real stdlib ahead of the fixtures (the same policy as
/// `tests/trace.rs::oracle_eval`).
///
/// `None` means the `erl` process could not be **spawned** (the caller skips
/// loudly). A spawned `erl` that exits non-zero is a **harness** failure, not "no
/// oracle": it panics with the captured stderr, so a broken oracle can never
/// masquerade as a silent skip (the same policy as `tests/trace.rs::oracle_eval`).
fn oracle(expr: &str) -> Option<String> {
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-pz")
        .arg(format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR")))
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
fn the_oracle_names_the_raising_function_in_its_innermost_frame() {
    // `raise_frames:outer(1)` raises `error:badarg` from inside `boom/1`; the real
    // ERTS raw stacktrace must name **that** function in its head item — exactly the
    // contract the engine's frames implement above.
    let expr = "S = try raise_frames:outer(1) catch error:_:St -> St end, \
                [{M,F,A,_}|_] = S, io:format(\"~p\", [{M,F,A}]), halt().";
    match oracle(expr) {
        Some(actual) => assert_eq!(
            actual, "{raise_frames,boom,1}",
            "the oracle's innermost stacktrace frame names the raising function"
        ),
        None => eprintln!(
            "SKIP raise_stacktrace: no system `erl` on PATH — the oracle cross-check was skipped"
        ),
    }
}

#[test]
fn the_oracle_names_the_cross_module_callee_then_the_caller() {
    // The same chain as case (b): the real ERTS raw stacktrace's first two frames are
    // the callee's then the caller's, each in its own module — the engine's contract.
    let expr = "S = try xmod_caller:run(1) catch error:_:St -> St end, \
                [{M1,F1,A1,_},{M2,F2,A2,_}|_] = S, \
                io:format(\"~p\", [{{M1,F1,A1},{M2,F2,A2}}]), halt().";
    match oracle(expr) {
        Some(actual) => assert_eq!(
            actual, "{{xmod_callee,boom,1},{xmod_caller,run,1}}",
            "the oracle's first two frames are the callee's then the caller's"
        ),
        None => eprintln!(
            "SKIP raise_stacktrace: no system `erl` on PATH — the oracle cross-check was skipped"
        ),
    }
}
