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

//! Execution-trace emission (contract A.8): the engine *reports*, the host *matches*.
//!
//! `Machine` hands its host the `call`/`return_from`/`return_to` event family through
//! two **defaulted** [`ExecutionContext`] hooks -- [`ExecutionContext::trace_enabled`]
//! (the cheap per-call/return gate, `false` out of the box) and
//! [`ExecutionContext::trace_event`] (the report, a no-op out of the box) -- over the
//! borrowed [`TraceEvent`] enum. These tests build a small logging host and drive the
//! **committed** oracle fixtures through [`Machine::run_export`], asserting the exact
//! event sequences.
//!
//! # The emission rules this suite pins (source of truth: `src/interpreter/trace.rs`)
//!
//! * `call`/`call_last`/`call_only` -> `Call` for the **local** callee (its declared
//!   `{M,F,A}`, `args` = `x(0..arity)`).
//! * `call_ext`/`call_ext_only`/`call_ext_last` -> `Call` for the **imported** target,
//!   **entry only**: a BIF / cross-module completion reports no `return_from`/
//!   `return_to` (probed against the oracle -- `erlang:'+'/2` yields `call` and no
//!   `return_to`).
//! * `return` with a live `Continuation::Address { func: caller, .. }` -> `ReturnFrom`
//!   (the function containing the `return`, `value = x(0)`) followed by `ReturnTo`
//!   (the caller). A `return` whose continuation is `Continuation::Halt` reports
//!   nothing (the oracle's `return_to undefined`).
//! * The run's **entry function is not reported**: it is not a call site (the oracle's
//!   entry `call` comes from a spawn/function-entry breakpoint).
//!
//! # Two deliberate deviations from the task brief, pinned honestly here
//!
//! 1. **The committed `slice:add/2` reports no events as a run entry.** The brief's
//!    example (`Call{module:"slice", function:"add", arity:2, args:[2,3]}` +
//!    `ReturnFrom{value:5}` + a `'+'/2` ext-call event) cannot come from `slice:add/2`
//!    driven through `run_export`: the entry is not a call site, `A + B` compiles to a
//!    `gc_bif '+'/2` (not a traced `call_ext*`), and the final `return`'s continuation
//!    is `Halt`. [`slice_fixture_add_as_run_entry_reports_no_events`] pins exactly that
//!    (result `5`, zero events), and
//!    [`synthetic_call_and_return_sequence_reports_the_oracle_shapes`] reproduces the
//!    brief's required shapes on a **synthetic** module (named `slice`) whose `add/2`
//!    is *called* with `+'/2` as a `call_ext` -- the only way to obtain them.
//! 2. **The tail-call `return_to` names the original caller.** A `call_only`/
//!    `call_last` chain reuses one frame, so the eventual `return` reports
//!    `ReturnTo{c}` (the caller of the chain's **first** function), never the
//!    intermediate tail-callee. The committed fixtures never put a frame *under* a
//!    local tail chain (their tail-recursive functions are the run entry, so the chain
//!    ends at the bottom `Halt` frame), so the property is pinned here on a synthetic
//!    module and cross-checked against a live oracle that traces an equivalent compiled
//!    module ([`oracle_confirms_tail_call_returns_to_the_original_caller`], loud-skip).
//!
//! Every expected fixture value is cross-checked against the system `erl`; the tests
//! **skip loudly** (never silently pass) when the oracle is unavailable.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::Command;

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Result};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext, TraceEvent};
use beam_loader::{
    load_file, Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand,
};
use beam_term::{is_small, make_atom, make_small, signed_value, Atom, Eterm, NIL};

/// The `beam_loader` oracle fixture (reused by relative path -- one source of truth).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);
/// The directory `slice.beam` lives in (for the oracle's `-pz` code path).
const SLICE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../beam_loader/tests/fixtures");
/// The committed tail-call frame-discipline fixture.
const TAILCALL_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tailcall.beam");
/// The fixture directory the oracle's `-pz` (append) code path points at.
const FIXTURES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

// ---------------------------------------------------------------------------
// The owned, comparable event log
// ---------------------------------------------------------------------------

/// One reported [`TraceEvent`] in owned, `PartialEq` form (the real event borrows the
/// running module's names and register slice).
///
/// `args`/`value` keep the raw [`Eterm`]s: for the immediates these fixtures use
/// (small ints, atoms) `Eterm`'s `PartialEq` is *value* equality, so the whole
/// vector compares exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ev {
    /// A `call` event.
    Call {
        /// The entered module's name.
        module: String,
        /// The entered function's name.
        function: String,
        /// The entered function's arity.
        arity: u32,
        /// The call's arguments (`x(0..arity)`).
        args: Vec<Eterm>,
    },
    /// A `return_from` event (the returning function and its value).
    ReturnFrom {
        /// The returning module's name.
        module: String,
        /// The returning function's name.
        function: String,
        /// Its arity.
        arity: u32,
        /// The returned value (`x(0)` at the `return`).
        value: Eterm,
    },
    /// A `return_to` event (the function control returns *to*).
    ReturnTo {
        /// The module of the caller.
        module: String,
        /// The name of the caller.
        function: String,
        /// Its arity.
        arity: u32,
    },
}

/// `Call{...}` builder.
fn call(module: &str, function: &str, arity: u32, args: &[Eterm]) -> Ev {
    Ev::Call {
        module: module.into(),
        function: function.into(),
        arity,
        args: args.to_vec(),
    }
}

/// `ReturnFrom{...}` builder.
fn retf(module: &str, function: &str, arity: u32, value: Eterm) -> Ev {
    Ev::ReturnFrom {
        module: module.into(),
        function: function.into(),
        arity,
        value,
    }
}

/// `ReturnTo{...}` builder.
fn rett(module: &str, function: &str, arity: u32) -> Ev {
    Ev::ReturnTo {
        module: module.into(),
        function: function.into(),
        arity,
    }
}

/// The `(kind, module, function, arity)` projection -- what a test asserts when the
/// argument term is a heap term (a cons cell) whose address is not comparable.
fn mfa(event: &Ev) -> (&'static str, &str, &str, u32) {
    match event {
        Ev::Call {
            module,
            function,
            arity,
            ..
        } => ("call", module, function, *arity),
        Ev::ReturnFrom {
            module,
            function,
            arity,
            ..
        } => ("return_from", module, function, *arity),
        Ev::ReturnTo {
            module,
            function,
            arity,
        } => ("return_to", module, function, *arity),
    }
}

/// Render a term for a diagnostic: a small int as itself, anything else via `Debug`.
fn render(t: Eterm) -> String {
    if is_small(t) {
        signed_value(t).to_string()
    } else {
        format!("{t:?}")
    }
}

/// A compact rendering of one event for a failure message.
fn describe(event: &Ev) -> String {
    match event {
        Ev::Call {
            module,
            function,
            arity,
            args,
        } => format!(
            "call {module}:{function}/{arity}({})",
            args.iter()
                .map(|t| render(*t))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Ev::ReturnFrom {
            module,
            function,
            arity,
            value,
        } => format!("return_from {module}:{function}/{arity} -> {}", render(*value)),
        Ev::ReturnTo {
            module,
            function,
            arity,
        } => format!("return_to {module}:{function}/{arity}"),
    }
}

/// Assert a logged sequence equals `expected`, with a readable diff.
fn assert_events(actual: &[Ev], expected: &[Ev]) {
    if actual != expected {
        let show = |events: &[Ev]| {
            events
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join("\n    ")
        };
        panic!(
            "trace event mismatch\n  expected:\n    {}\n  actual:\n    {}",
            show(expected),
            show(actual)
        );
    }
}

// ---------------------------------------------------------------------------
// The logging host
// ---------------------------------------------------------------------------

/// A minimal host: a `MinimalContext` arena (for `ensure_heap`/`alloc_words`) plus the
/// BIF floor the fixtures reach, and the execution-trace seam.
///
/// `enabled` drives the gate; the report appends every event to `reported`, and -- when
/// `filter` is `None` or the event's `{M,F,A}` is in it -- also to `emitted`. Keeping the
/// two logs apart is exactly the engine-reports / host-matches split the oracle gives
/// via `erlang:trace_pattern/3`.
struct Host {
    arena: MinimalContext,
    enabled: bool,
    filter: Option<HashSet<(String, String, u32)>>,
    reported: Vec<Ev>,
    emitted: Vec<Ev>,
}

impl Host {
    /// A host with tracing on and no matching filter (every report is also emitted).
    fn tracing() -> Host {
        Host {
            arena: MinimalContext::new(),
            enabled: true,
            filter: None,
            reported: Vec::new(),
            emitted: Vec::new(),
        }
    }

    /// The default host: the gate is off, and a report would be a bug -- it panics.
    fn silent() -> Host {
        Host {
            enabled: false,
            ..Host::tracing()
        }
    }

    /// A host whose pattern table (a set of `{M,F,A}`) decides which reports are
    /// *emitted*; every report still lands in `reported`.
    fn matching(entries: &[(&str, &str, u32)]) -> Host {
        let filter = entries
            .iter()
            .map(|(m, f, a)| ((*m).to_string(), (*f).to_string(), *a))
            .collect();
        Host {
            filter: Some(filter),
            ..Host::tracing()
        }
    }

    /// Run `function/args.len()` of `module` and return `x(0)`.
    fn run(&mut self, module: &Module, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let atom = module
            .atom_table
            .lookup(function)
            .unwrap_or_else(|| panic!("function {function:?} not in the module"));
        let mut machine = Machine::new();
        machine.run_export(module, atom, args.len() as u32, args, self)
    }
}

impl ExecutionContext for Host {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        match (module, function, arity) {
            // A raising BIF carrying its argument as the live reason term -- the shape
            // `beam_bifs`/`beam_rt` give `erlang:error/1` (what `gap(6)` reaches).
            ("erlang", "error", 1) if args.len() == 1 => {
                let raise = beam_common::Raise::new(ExceptionClass::Error, ExitReason::Structured)
                    .with_live_reason(LiveReason::new(args[0]));
                Err(BeamError::Raise(raise))
            }
            ("lists", "reverse", 1) if args.len() == 1 => {
                let mut items = terms::list_elements(args[0])?;
                items.reverse();
                terms::make_list(self, &items)
            }
            ("erlang", "+", 2) if args.len() == 2 => {
                Ok(make_small(signed_value(args[0]) + signed_value(args[1])))
            }
            ("erlang", "-", 2) if args.len() == 2 => {
                Ok(make_small(signed_value(args[0]) - signed_value(args[1])))
            }
            _ => Err(BeamError::BadArg),
        }
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.arena.alloc_words(n)
    }

    fn trace_enabled(&self) -> bool {
        self.enabled
    }

    fn trace_event(&mut self, event: TraceEvent<'_>) {
        // The gate is the whole contract: the engine must never report unless
        // `trace_enabled()` said so. A report on a disabled host is a bug this suite
        // turns into a hard failure rather than letting it pass unnoticed.
        assert!(
            self.enabled,
            "trace_event reported while trace_enabled() returned false"
        );
        let owned = match event {
            TraceEvent::Call {
                module,
                function,
                arity,
                args,
            } => call(module, function, arity, args),
            TraceEvent::ReturnFrom {
                module,
                function,
                arity,
                value,
            } => retf(module, function, arity, value),
            TraceEvent::ReturnTo {
                module,
                function,
                arity,
            } => rett(module, function, arity),
        };
        if let Some(filter) = &self.filter {
            let (_, module, function, arity) = mfa(&owned);
            if filter.contains(&(module.to_string(), function.to_string(), arity)) {
                self.emitted.push(owned.clone());
            }
        } else {
            self.emitted.push(owned.clone());
        }
        self.reported.push(owned);
    }
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// `load_file` plus a readable failure.
fn load(path: &str) -> Module {
    load_file(path).unwrap_or_else(|e| panic!("load {path}: {e:?}"))
}

/// The atom term for a name interned in the module's own atom table.
fn atom_term(module: &Module, name: &str) -> Eterm {
    make_atom(
        module
            .atom_table
            .lookup(name)
            .unwrap_or_else(|| panic!("atom {name:?} not interned in the module")),
    )
}

// ---------------------------------------------------------------------------
// A synthetic module exercising shapes the committed fixtures cannot produce
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

/// A `Module` builder (the loader is not involved) with module-wide unique labels.
struct Fx {
    module: Module,
}

impl Fx {
    fn new(name: &str) -> Fx {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fx { module }
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

    /// Append a function; returns its entry label (the first label in its code).
    ///
    /// The function is also exported (its entry label recorded in `Module::exports`),
    /// which is what `Machine::run_export` resolves a run entry through.
    fn function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.exports.push(Export {
            function: name,
            arity,
            label: entry,
        });
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

/// A synthetic module named `slice`, in two shapes the committed fixtures cannot give:
///
/// * `add/2` + `caller/0` -- a **genuine non-tail local call** whose callee's `+` is an
///   imported `call_ext`, reproducing the brief's required `Call{slice,add,2}` /
///   `ReturnFrom{value:5}` / `+'/2` entry events.
/// * `chain/0` -> `mid/0` -> `leaf/0` -- a frame-owning caller, a non-tail call, and a
///   `call_only` tail call, so the eventual `return` reports the **original** caller.
///
/// * `caller/0`: `allocate 0`, `move 2 -> x0`, `move 3 -> x1`, `call add/2`,
///   `deallocate 0`, `return`.
/// * `add/2`: `call_ext erlang:'+'/2`, `return` (a real compiler emits a `gc_bif`
///   here instead, which is not a call and reports nothing).
/// * `chain/0`: `allocate 0`, `call mid/0`, `deallocate 0`, `return`.
/// * `mid/0`: `call_only leaf/0`.
/// * `leaf/0`: `move 42 -> x0`, `return`.
fn synthetic_slice() -> Module {
    let mut fx = Fx::new("slice");
    let plus = fx.import("erlang", "+", 2);

    let add = fx.function(
        "add",
        2,
        vec![
            Instr::Label(Label(1)),
            Instr::CallExt {
                arity: 2,
                target: plus,
            },
            Instr::Return,
        ],
    );
    fx.function(
        "caller",
        0,
        vec![
            Instr::Label(Label(2)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Move {
                src: Operand::Integer(2),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(3),
                dst: Operand::X(1),
            },
            Instr::Call { arity: 2, target: add },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    let leaf = fx.function(
        "leaf",
        0,
        vec![
            Instr::Label(Label(3)),
            Instr::Move {
                src: Operand::Integer(42),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    let mid = fx.function(
        "mid",
        0,
        vec![
            Instr::Label(Label(4)),
            Instr::CallOnly {
                arity: 0,
                target: leaf,
            },
        ],
    );
    fx.function(
        "chain",
        0,
        vec![
            Instr::Label(Label(5)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Call { arity: 0, target: mid },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fx.finish()
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

/// Run `expr` under the system `erl` with `dir` **appended** to the code path and
/// return its stdout.
///
/// The append flag is `-pz`, never `-pa`: a fixture directory contains stand-in
/// modules (e.g. this crate's `tests/fixtures/lists.beam`) that would **shadow** the
/// real stdlib once *prepended*, crashing the emulator at boot (`lists:usort/1`
/// undefined). Appending keeps the real stdlib ahead of the fixtures.
///
/// `None` means no runnable `erl` at all (the caller skips loudly). A spawned `erl`
/// that exits non-zero is a **harness** failure, not "no oracle" -- it panics with
/// the captured stderr so a broken oracle can never masquerade as a silent skip.
fn oracle_eval(dir: &str, expr: &str) -> Option<String> {
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-pz")
        .arg(dir)
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
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Cross-check a `~p`-printed value under the oracle, skipping **loudly** when the
/// oracle is absent.
///
/// `expr` must print exactly `expected` and halt. `dir` is the fixture directory the
/// module under test lives in.
fn cross_check_value(dir: &str, expr: &str, expected: &str) {
    match oracle_eval(dir, expr) {
        Some(actual) => assert_eq!(
            actual.trim(),
            expected,
            "the oracle disagrees with the pinned value for `{expr}`"
        ),
        None => eprintln!("SKIP: no system `erl` on PATH -- oracle value check for `{expr}` skipped"),
    }
}

/// Trace `Module:Function(Args)` under the system `erl` (spawned so the process's
/// `call`/`return_to` events are traceable) and return the raw trace output, or
/// `None` when no usable oracle is available.
///
/// The process is spawned blocked on a `go` message so the trace flags and pattern are
/// installed **before** the traced call runs.
fn oracle_trace(dir: &str, module: &str, function: &str, args: &str) -> Option<String> {
    let expr = format!(
        "{{module, M}} = code:ensure_loaded({module}), \
         Pid = spawn(fun() -> receive go -> ok end, apply(M, {function}, {args}) end), \
         erlang:trace(Pid, true, [call, return_to]), \
         erlang:trace_pattern({{{module}, '_', '_'}}, true, [local]), \
         Pid ! go, \
         Collect = fun G() -> receive Msg -> io:format(\"~p~n\", [Msg]), G() after 300 -> ok end end, \
         Collect(), halt(0)."
    );
    oracle_eval(dir, &expr)
}

/// The real-Erlang equivalent of [`synthetic_slice`]'s `chain/0` shape: `run/0` calls
/// `mid/1` **non-tail**, `mid/1` `call_only`s `leaf/1`, `leaf/1` returns.
///
/// `run/0` is forced non-tail (`X + 1` after the call) so it owns a frame whose
/// continuation `leaf` returns to -- the shape the property needs.
const TRACEWRAP_SRC: &str = "\
-module(tracewrap).
-export([run/0]).
run() ->
    X = mid(0),
    X + 1.
mid(N) ->
    leaf(N).
leaf(N) ->
    N.
";

/// Compile [`TRACEWRAP_SRC`] into a fresh temp dir, returning that dir; `None` when no
/// runnable `erlc` is on `PATH` at all (the caller skips loudly).
///
/// `None` means `erlc` could not be **spawned**. An `erlc` that *ran* and exited
/// non-zero is a **harness** failure (the source is valid Erlang), so it panics with
/// the captured stderr rather than masquerading as an absent toolchain — the same
/// policy as [`oracle_eval`].
fn compile_tracewrap() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("beam_engine_trace_oracle");
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    std::fs::write(dir.join("tracewrap.erl"), TRACEWRAP_SRC)
        .unwrap_or_else(|error| panic!("write tracewrap.erl: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("tracewrap.erl")
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling tracewrap.erl\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(dir)
}

// ---------------------------------------------------------------------------
// (1) the committed `slice:add/2` as a run entry reports nothing
// ---------------------------------------------------------------------------

#[test]
fn slice_fixture_add_as_run_entry_reports_no_events() {
    // The honest behavior: `add/2` is the run's *entry* (not a call site), `A + B` is a
    // `gc_bif '+'/2` (not a traced `call_ext*`), and the final `return`'s continuation
    // is `Halt`. So the brief's literal `Call{slice,add,2}` + `ReturnFrom{5}` cannot be
    // produced from this fixture -- the value is right and the report is empty.
    let module = load(SLICE_BEAM);
    let mut host = Host::tracing();
    let result = host
        .run(&module, "add", &[make_small(2), make_small(3)])
        .expect("add(2, 3)");
    assert_eq!(result, make_small(5));
    assert_events(&host.reported, &[]);
    assert_events(&host.emitted, &[]);
    cross_check_value(SLICE_DIR, "io:format(\"~p\",[slice:add(2,3)]),halt(0).", "5");
}

// ---------------------------------------------------------------------------
// (2) a real local call/return sequence from a committed fixture
// ---------------------------------------------------------------------------

#[test]
fn fixture_local_call_and_return_are_reported_in_oracle_order() {
    // `tailcall:gap/1` allocates a frame and calls `hold/1` **non-tail**, so the callee
    // reports `Call` + `ReturnFrom` and the caller's frame continuation reports
    // `ReturnTo` -- the full triple from a real committed fixture.
    let module = load(TAILCALL_BEAM);
    let mut host = Host::tracing();
    let result = host.run(&module, "gap", &[make_small(3)]).expect("gap(3)");
    assert_eq!(result, atom_term(&module, "three"));
    assert_events(
        &host.reported,
        &[
            call("tailcall", "hold", 1, &[make_small(3)]),
            retf("tailcall", "hold", 1, atom_term(&module, "ok")),
            rett("tailcall", "gap", 1),
        ],
    );

    // The oracle agrees on the observable shape (the entry `call`/final `undefined`
    // events are the spawn root's, which the engine deliberately does not invent).
    match oracle_trace(FIXTURES_DIR, "tailcall", "gap", "[3]") {
        Some(trace) => {
            assert!(
                trace.contains("call,{tailcall,hold,[3]}}"),
                "the oracle must report the same callee call; trace was:\n{trace}"
            );
            assert!(
                trace.contains("return_to,{tailcall,gap,1}}"),
                "the oracle must report the same return_to; trace was:\n{trace}"
            );
        }
        None => eprintln!("SKIP: no system `erl` on PATH -- gap/1 trace cross-check skipped"),
    }
    cross_check_value(FIXTURES_DIR, "io:format(\"~p\",[tailcall:gap(3)]),halt(0).", "three");
}

// ---------------------------------------------------------------------------
// (3) the brief's required `add/2` shapes, on a module that can produce them
// ---------------------------------------------------------------------------

#[test]
fn synthetic_call_and_return_sequence_reports_the_oracle_shapes() {
    // A synthetic module named `slice` whose `add/2` is **called** (and whose `+` is an
    // imported `call_ext`) is the only way to obtain the brief's exact required events.
    let module = synthetic_slice();
    let mut host = Host::tracing();
    let result = host.run(&module, "caller", &[]).expect("caller/0");
    assert_eq!(result, make_small(5));
    assert_events(
        &host.reported,
        &[
            call("slice", "add", 2, &[make_small(2), make_small(3)]),
            call("erlang", "+", 2, &[make_small(2), make_small(3)]),
            retf("slice", "add", 2, make_small(5)),
            rett("slice", "caller", 0),
        ],
    );
    // The `call_ext '+'/2` is report-on-entry: no return event names it anywhere.
    assert!(
        host.reported.iter().all(|event| !matches!(
            event,
            Ev::ReturnFrom { module, .. } | Ev::ReturnTo { module, .. }
                if module.as_str() == "erlang"
        )),
        "a BIF completion must not invent a return event: {:?}",
        host.reported
    );
}

// ---------------------------------------------------------------------------
// (4) `call_ext*` reports the entry only
// ---------------------------------------------------------------------------

#[test]
fn ext_call_reports_the_entry_only() {
    // `tailrev/1` reaches `lists:reverse/1` three times: twice as `call_ext` and once
    // as the tail `call_ext_last`. Each reports one `Call`; the import completion
    // reports no `return_from`/`return_to` at all.
    let module = load(TAILCALL_BEAM);
    let mut host = Host::tracing();
    let result = host
        .run(&module, "tailrev", &[make_small(7)])
        .expect("tailrev(7)");
    assert_eq!(host.reported.len(), 3, "{:?}", host.reported);
    for event in &host.reported {
        assert_eq!(mfa(event), ("call", "lists", "reverse", 1));
    }
    assert!(
        !host
            .reported
            .iter()
            .any(|e| matches!(e, Ev::ReturnFrom { .. } | Ev::ReturnTo { .. })),
        "an ext call must be entry-only: {:?}",
        host.reported
    );
    // `tailrev(7) -> lists:reverse([7]) -> [7]`.
    assert_eq!(terms::list_elements(result).unwrap(), vec![make_small(7)]);
    cross_check_value(FIXTURES_DIR, "io:format(\"~p\",[tailcall:tailrev(7)]),halt(0).", "[7]");
}

// ---------------------------------------------------------------------------
// (5) the tail-call property: `return_to` names the ORIGINAL caller
// ---------------------------------------------------------------------------

#[test]
fn tail_call_returns_to_the_original_caller_not_the_intermediate() {
    // `chain/0` (with a frame) calls `mid/0` non-tail; `mid/0` `call_only`s `leaf/0`;
    // `leaf/0` returns. `mid/0`'s frame *is* `chain/0`'s, so `leaf/0`'s return reports
    // `return_to slice:chain/0` -- the ORIGINAL caller -- and never `mid/0`.
    let module = synthetic_slice();
    let mut host = Host::tracing();
    let result = host.run(&module, "chain", &[]).expect("chain/0");
    assert_eq!(result, make_small(42));
    assert_events(
        &host.reported,
        &[
            call("slice", "mid", 0, &[]),
            call("slice", "leaf", 0, &[]),
            retf("slice", "leaf", 0, make_small(42)),
            rett("slice", "chain", 0),
        ],
    );
    assert!(
        !host.reported.iter().any(|e| matches!(
            e,
            Ev::ReturnTo { function, .. } if function.as_str() == "mid"
        )),
        "the tail-called intermediate must never be named as a return target: {:?}",
        host.reported
    );
}

#[test]
fn oracle_confirms_tail_call_returns_to_the_original_caller() {
    // The same shape through the system `erl`: `tracewrap:run/0` owns a frame, calls
    // `mid/1` non-tail, which `call_only`s `leaf/1`. The oracle's `return_to` after
    // `leaf` returns must name `run` (the original caller), never `mid`.
    let Some(dir) = compile_tracewrap() else {
        eprintln!("SKIP: no usable `erlc` on PATH -- tail-call oracle cross-check skipped");
        return;
    };
    let Some(dir) = dir.to_str() else {
        eprintln!("SKIP: temp dir is not valid UTF-8 -- tail-call oracle cross-check skipped");
        return;
    };
    let Some(trace) = oracle_trace(dir, "tracewrap", "run", "[]") else {
        eprintln!("SKIP: no usable `erl` on PATH -- tail-call oracle cross-check skipped");
        return;
    };
    assert!(
        trace.contains("call,{tracewrap,mid,[0]}}"),
        "the oracle must report the non-tail call to mid; trace was:\n{trace}"
    );
    assert!(
        trace.contains("call,{tracewrap,leaf,[0]}}"),
        "the oracle must report the call_only into leaf; trace was:\n{trace}"
    );
    assert!(
        trace.contains("return_to,{tracewrap,run,0}}"),
        "the oracle's return_to must name the ORIGINAL caller `run`; trace was:\n{trace}"
    );
    assert!(
        !trace.contains("return_to,{tracewrap,mid,1}}"),
        "the oracle must never name the intermediate `mid` as a return target; trace was:\n{trace}"
    );
}

// ---------------------------------------------------------------------------
// (6) a `call_only` chain reports one `Call` per hop
// ---------------------------------------------------------------------------

#[test]
fn mutual_tail_recursion_reports_each_hop_and_never_an_out_of_module_return() {
    // `even/1`/`odd/1` bounce through `call_only`; the whole chain reuses ONE frame, so
    // only `Call` events are produced and the final `return` (the bottom `Halt` frame --
    // the oracle's `return_to undefined`) reports nothing.
    let module = load(TAILCALL_BEAM);
    let mut host = Host::tracing();
    let result = host.run(&module, "even", &[make_small(4)]).expect("even(4)");
    assert_eq!(result, atom_term(&module, "true"));
    assert_events(
        &host.reported,
        &[
            call("tailcall", "odd", 1, &[make_small(3)]),
            call("tailcall", "even", 1, &[make_small(2)]),
            call("tailcall", "odd", 1, &[make_small(1)]),
            call("tailcall", "even", 1, &[make_small(0)]),
        ],
    );

    // The oracle emits the same four hop calls (plus its spawn-entry call, which is not
    // a call site), then `return_to undefined` -- which the engine must NOT invent.
    match oracle_trace(FIXTURES_DIR, "tailcall", "even", "[4]") {
        Some(trace) => {
            for hop in [
                "call,{tailcall,odd,[3]}}",
                "call,{tailcall,even,[2]}}",
                "call,{tailcall,odd,[1]}}",
                "call,{tailcall,even,[0]}}",
            ] {
                assert!(
                    trace.contains(hop),
                    "the oracle must report the hop `{hop}`; trace was:\n{trace}"
                );
            }
            assert!(
                !trace.contains("return_to,{tailcall,"),
                "the oracle's tail chain returns to the spawn root, not a module function; \
                 trace was:\n{trace}"
            );
        }
        None => eprintln!("SKIP: no system `erl` on PATH -- even/4 trace cross-check skipped"),
    }
    cross_check_value(FIXTURES_DIR, "io:format(\"~p\",[tailcall:even(4)]),halt(0).", "true");
}

// ---------------------------------------------------------------------------
// (7) the gate: OFF is the default and the run is unchanged
// ---------------------------------------------------------------------------

#[test]
fn gate_off_is_the_default_and_reports_nothing() {
    // A host whose `trace_enabled()` is `false` and whose `trace_event` would panic:
    // the same runs produce the same results and zero reports.
    let slice = load(SLICE_BEAM);
    let tailcall = load(TAILCALL_BEAM);

    // The trait default is off (the `MinimalContext` embedding host).
    assert!(
        !MinimalContext::new().trace_enabled(),
        "tracing is off by default"
    );

    let mut silent = Host::silent();
    assert_eq!(
        silent
            .run(&slice, "fib", &[make_small(10)])
            .expect("fib(10)"),
        make_small(55)
    );
    assert_eq!(
        silent
            .run(&tailcall, "gap", &[make_small(3)])
            .expect("gap(3)"),
        atom_term(&tailcall, "three")
    );
    let looped = silent
        .run(&tailcall, "loop", &[make_small(3), NIL])
        .expect("loop(3, [])");
    assert_eq!(
        terms::list_elements(looped).unwrap(),
        vec![make_small(1), make_small(2), make_small(3)],
        "loop(3, []) == [1,2,3]"
    );
    assert!(
        silent.reported.is_empty(),
        "a disabled gate must report nothing: {:?}",
        silent.reported
    );
    assert!(silent.emitted.is_empty());
}

// ---------------------------------------------------------------------------
// (8) the engine-reports / host-matches split
// ---------------------------------------------------------------------------

#[test]
fn host_matching_filters_emissions_but_the_engine_reports_everything() {
    // A host given a pattern table (like the oracle's `erlang:trace_pattern/3`) sees
    // every report but *emits* (builds a message for) only the matched `{M,F,A}`.
    let module = load(TAILCALL_BEAM);
    let mut host = Host::matching(&[("tailcall", "hold", 1)]);
    host.run(&module, "gap", &[make_small(3)]).expect("gap(3)");

    // The engine reported all three (call + return_from + return_to)...
    assert_events(
        &host.reported,
        &[
            call("tailcall", "hold", 1, &[make_small(3)]),
            retf("tailcall", "hold", 1, atom_term(&module, "ok")),
            rett("tailcall", "gap", 1),
        ],
    );
    // ...while the host emitted only the two whose `{M,F,A}` is `{tailcall,hold,1}`.
    assert_events(
        &host.emitted,
        &[
            call("tailcall", "hold", 1, &[make_small(3)]),
            retf("tailcall", "hold", 1, atom_term(&module, "ok")),
        ],
    );

    // An unmatched function: the engine still reports every call, the host emits none.
    let mut host = Host::matching(&[("tailcall", "nonexistent", 9)]);
    host.run(&module, "even", &[make_small(4)]).expect("even(4)");
    assert_eq!(host.reported.len(), 4, "{:?}", host.reported);
    assert!(
        host.emitted.is_empty(),
        "nothing matches the pattern table: {:?}",
        host.emitted
    );
}

// ---------------------------------------------------------------------------
// (9) tracing never changes the outcome
// ---------------------------------------------------------------------------

#[test]
fn tracing_does_not_change_the_outcome() {
    let module = load(TAILCALL_BEAM);

    // A successful run: the same value with the gate on and off.
    let mut on = Host::tracing();
    let mut off = Host::silent();
    let traced = on.run(&module, "gap", &[make_small(3)]).expect("gap(3)");
    let untraced = off.run(&module, "gap", &[make_small(3)]).expect("gap(3)");
    assert_eq!(traced, untraced);
    assert_eq!(traced, atom_term(&module, "three"));
    assert!(!on.reported.is_empty());

    // A raising run: the exact same typed error, live reason included.
    let mut on = Host::tracing();
    let mut off = Host::silent();
    let traced = on
        .run(&module, "gap", &[make_small(6)])
        .expect_err("gap(6) must raise");
    let untraced = off
        .run(&module, "gap", &[make_small(6)])
        .expect_err("gap(6) must raise");
    assert_eq!(
        format!("{traced:?}"),
        format!("{untraced:?}"),
        "tracing changed the raised error"
    );
    // The raise carries the BIF's live reason term, and the traced run reported the
    // path that led to it (including the `erlang:error/1` entry).
    match &traced {
        BeamError::Raise(raise) => {
            assert_eq!(raise.class, ExceptionClass::Error);
            assert_eq!(
                raise
                    .live_reason()
                    .and_then(|live| live.downcast_ref::<Eterm>())
                    .copied(),
                Some(atom_term(&module, "unsupported_gap")),
                "the raise must carry the fixture's own reason term"
            );
        }
        other => panic!("expected a raise, got {other:?}"),
    }
    assert!(
        on.reported
            .iter()
            .any(|e| matches!(e, Ev::Call { function, .. } if function.as_str() == "error")),
        "the traced run must report the raising ext call's entry: {:?}",
        on.reported
    );
}
