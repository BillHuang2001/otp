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

//! **Dynamic per-variant execution census** for the interpreter's instruction funnel.
//!
//! This is a **measurement-only** instrument: it is not part of any execution path, it
//! never changes a result, and with the census disabled every hook below costs a single
//! `bool` load. It answers the question the baseline tier's *static* census
//! (`jit::baseline::census`, which counts the instructions a module **contains**) cannot:
//! how often does each [`Instr`] variant actually **execute** at run time, and how often
//! does it **raise**?
//!
//! # What is counted, and where
//!
//! Both recorders are called from exactly one place: inside
//! `Machine::step_instruction_inner` — the single per-instruction funnel that the
//! interpreter's dispatch loop (`Machine::drive`) **and** the native tiers'
//! per-instruction deopt helper (`jit_step_one`) both pass through.
//!
//! * [`on_instruction`] — one count per instruction entering the funnel, attributed to
//!   the variant by [`variant_name`], accumulated in [`Snapshot::executed`] and
//!   [`Snapshot::total_executed`].
//! * [`on_error`] — one count per instruction whose execution **returned** an
//!   [`Err(BeamError)`](BeamError) out of the funnel, accumulated in
//!   [`Snapshot::errors`]/[`Snapshot::total_errors`], with the **first** such error's
//!   exact [`Display`](std::fmt::Display) text kept in [`Snapshot::first_error`].
//!
//! # Semantics of the numbers (read these before quoting a table)
//!
//! * **Counting happens before the instruction's body runs.** An instruction that
//!   raises is therefore counted as `executed` **and** as an `error`; the two maps are
//!   not disjoint views of the same population.
//! * **An "error" here means the instruction returned `Err(BeamError)` out of the
//!   funnel** — a raise (or a typed capability gap) that escaped. A raise that lands in
//!   a live in-machine `try`/`catch` frame is folded into a [`Step`](crate::Step) by the
//!   exceptions family and comes back `Ok(..)`, so it is counted as **executed but not
//!   as an error**. A pre-emption (the host's `reduce` returning `Err`) is likewise
//!   counted as an error of the instruction it parked on, even though the instruction is
//!   re-applied on the resume.
//! * **A natively-inlined baseline-JIT instruction bypasses the funnel** and is therefore
//!   *not* observable: the interpreter tier is fully covered, while a native tier only
//!   covers the steps it deopts (plus every instruction of a module the tier has no
//!   compiled blocks for). The native baseline tier's own structural counters
//!   (`jit::baseline::counters`) remain the instrument for inlined code.
//! * **The disabled path is free** (one thread-local `bool` load, no allocation), and a
//!   counted run is byte-identical to an uncounted one — the census only reads the
//!   instruction and, on the error path, formats the variant name and the error text.
//!
//! # Where the numbers go
//!
//! [`snapshot`] returns this thread's counts; [`write_dump`] writes them as a compact
//! TSV under `std::env::temp_dir()` for a measurement run to collect.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use beam_common::BeamError;
use beam_loader::Instr;

thread_local! {
    /// Whether this thread is currently counting (default: off).
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    /// This thread's census.
    static CENSUS: RefCell<Snapshot> = RefCell::new(Snapshot::default());
}

/// A snapshot of this thread's dynamic per-variant execution census.
///
/// One map per event class (executed / errored) plus the first error text seen per
/// variant and the two totals. See the module docs for what an "executed" and an
/// "error" count mean.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Variant name -> the number of times an instruction of that variant entered the
    /// interpreter funnel (i.e. was about to execute).
    pub executed: BTreeMap<String, u64>,
    /// Variant name -> the number of times that variant's execution **returned** an
    /// [`Err(BeamError)`](BeamError) out of the funnel (see the module docs: a raise
    /// caught in-machine is not an error here).
    pub errors: BTreeMap<String, u64>,
    /// Variant name -> the exact [`Display`](std::fmt::Display) text of that variant's
    /// **first** error (`error unsupported_opcode: …`, `badarg`, …). Only the first
    /// text is retained, so the map stays one small entry per raising variant.
    pub first_error: BTreeMap<String, String>,
    /// Σ of [`executed`](Snapshot::executed)'s values.
    pub total_executed: u64,
    /// Σ of [`errors`](Snapshot::errors)'s values.
    pub total_errors: u64,
}

/// Turn counting on or off for this thread.
pub fn set_enabled(on: bool) {
    ENABLED.with(|e| e.set(on));
}

/// Turn counting on for this thread (see [`set_enabled`]).
pub fn enable() {
    set_enabled(true);
}

/// Turn counting off for this thread (see [`set_enabled`]).
pub fn disable() {
    set_enabled(false);
}

/// Whether this thread is currently counting.
pub fn is_enabled() -> bool {
    ENABLED.with(Cell::get)
}

/// Zero this thread's census (the enable state is left as it is).
pub fn reset() {
    CENSUS.with(|c| *c.borrow_mut() = Snapshot::default());
}

/// This thread's census, cloned.
pub fn snapshot() -> Snapshot {
    CENSUS.with(|c| c.borrow().clone())
}

/// The variant name of an `Instr`: the leading identifier of its `Debug` rendering
/// (`Move { .. }` -> `Move`, `Send` -> `Send`).
///
/// A `Test` is keyed by its **kind** (`Test(IsEqExact)`, `Test(IsTuple)`, …) because the
/// inline subset decides a `test` predicate by predicate: the per-kind split is what
/// makes "which `test` shapes still deopt" readable from the counter table.
///
/// Measurement-only, so the allocation and the `Debug` formatting are acceptable: this
/// runs only while a census (or a test-only structural counter) is enabled.
pub fn variant_name(instr: &Instr) -> String {
    if let Instr::Test { kind, .. } = instr {
        return format!("Test({kind:?})");
    }
    let rendered = format!("{instr:?}");
    let end = rendered
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rendered.len());
    rendered[..end].to_string()
}

/// Record that `instr` is about to execute (one count), if this thread is counting.
pub(crate) fn on_instruction(instr: &Instr) {
    if !ENABLED.with(Cell::get) {
        return;
    }
    let name = variant_name(instr);
    CENSUS.with(|c| {
        let mut census = c.borrow_mut();
        census.total_executed += 1;
        *census.executed.entry(name).or_insert(0) += 1;
    });
}

/// Record that `instr`'s execution returned `error`, if this thread is counting.
///
/// Both strings are formatted **only** on this (enabled, error) path, so a clean run
/// never pays for them.
pub(crate) fn on_error(instr: &Instr, error: &BeamError) {
    if !ENABLED.with(Cell::get) {
        return;
    }
    let name = variant_name(instr);
    let text = format!("{error}");
    CENSUS.with(|c| {
        let mut census = c.borrow_mut();
        census.total_errors += 1;
        *census.errors.entry(name.clone()).or_insert(0) += 1;
        census.first_error.entry(name).or_insert(text);
    });
}

/// Write this thread's census as a compact TSV to
/// `std::env::temp_dir()/beam_engine_exec_census_<section>.tsv`.
///
/// The file holds one `# …` comment line naming the section and the two totals, a
/// `# variant\texecuted\terrors\tfirst_error` header line, and then one row per variant
/// — the sorted union of the two maps — with `-` in the `first_error` column when that
/// variant never errored. Any character of `section` that is not alphanumeric, `-` or
/// `_` is replaced by `_` so the name cannot escape the temp directory.
///
/// A write failure is reported on stderr and yields `None`; this function never panics.
pub fn write_dump(section: &str) -> Option<PathBuf> {
    let census = snapshot();
    let safe: String = section
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = std::env::temp_dir().join(format!("beam_engine_exec_census_{safe}.tsv"));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "# beam_engine execution census (section={safe}) total_executed={} total_errors={}",
        census.total_executed, census.total_errors
    );
    out.push_str("# variant\texecuted\terrors\tfirst_error\n");
    let variants: BTreeSet<&String> = census.executed.keys().chain(census.errors.keys()).collect();
    for variant in variants {
        let executed = census.executed.get(variant).copied().unwrap_or(0);
        let errors = census.errors.get(variant).copied().unwrap_or(0);
        // The error text is a single line by construction, but flatten it anyway so a
        // multi-line `Display` can never break the TSV shape.
        let first = census
            .first_error
            .get(variant)
            .map(|text| text.replace(['\t', '\n', '\r'], " "))
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(out, "{variant}\t{executed}\t{errors}\t{first}");
    }

    match std::fs::write(&path, out) {
        Ok(()) => Some(path),
        Err(error) => {
            eprintln!("beam_engine exec census: cannot write {}: {error}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::context::MinimalContext;
    use crate::jit::Tier;
    use crate::machine::test_support::Fixture;
    use crate::Machine;
    use beam_loader::{Label, Operand};
    use beam_term::Eterm;

    /// Start each test from a known state: libtest may run several tests on **one**
    /// thread (`--test-threads=1`), and the census is thread-local.
    fn fresh() {
        disable();
        reset();
    }

    /// The executed count of `variant`, or 0 when it never ran.
    fn executed(snapshot: &Snapshot, variant: &str) -> u64 {
        snapshot.executed.get(variant).copied().unwrap_or(0)
    }

    /// `run/0`: `put_list [|[]]`, `move 42 -> x0`, `return` — one instruction of each
    /// of the four variants the funnel must observe, returning a **heap-independent**
    /// small integer so the counted and uncounted runs' results compare exactly (two
    /// runs use two fresh heaps, so a list result would differ by pointer alone).
    fn put_list_module() -> (beam_loader::Module, beam_term::Atom) {
        let mut f = Fixture::new("census_exec");
        let run = f.add_function(
            "run",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::PutList {
                    head: Operand::X(0),
                    tail: Operand::X(1),
                    dst: Operand::X(1),
                },
                Instr::Move {
                    src: Operand::Integer(42),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        );
        f.add_export("run", 0, run);
        let name = f.atom("run");
        (f.finish(), name)
    }

    /// `f/0` whose body is the failed-clause raise: `func_info census_raise f 0`.
    fn func_info_module() -> (beam_loader::Module, beam_term::Atom) {
        let mut f = Fixture::new("census_raise");
        // The raise resolves its reason/class atoms through the module's own atom
        // table, so both must be interned for the raise to be the typed
        // `error:function_clause` rather than a "cannot name the atom" gap.
        f.atom("function_clause");
        f.atom("error");
        let name = f.atom("f");
        let module_atom = f.atom("census_raise");
        let entry = f.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::FuncInfo {
                    module: module_atom,
                    function: name,
                    arity: 0,
                },
                Instr::Return,
            ],
        );
        f.add_export("f", 0, entry);
        (f.finish(), name)
    }

    /// Run `put_list_module` on the interpreter tier with the census in state `on`.
    fn run_put_list(on: bool) -> (Eterm, Snapshot) {
        let (module, name) = put_list_module();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        fresh();
        set_enabled(on);
        let value = machine
            .run_with_tier(&module, name, 0, &[], &mut ctx, Tier::Interpreter)
            .expect("run/0 returns");
        let snapshot = snapshot();
        fresh();
        (value, snapshot)
    }

    #[test]
    fn the_census_is_off_by_default_and_reset_zeroes_it() {
        fresh();
        assert!(!is_enabled(), "the census starts off");
        assert_eq!(snapshot(), Snapshot::default());

        enable();
        assert!(is_enabled());
        on_instruction(&Instr::Send);
        assert_eq!(executed(&snapshot(), "Send"), 1);

        disable();
        assert!(!is_enabled());
        on_instruction(&Instr::Send);
        assert_eq!(
            executed(&snapshot(), "Send"),
            1,
            "a disabled recorder adds nothing"
        );

        reset();
        assert_eq!(snapshot(), Snapshot::default(), "reset zeroes the census");
        fresh();
    }

    #[test]
    fn a_test_instruction_is_keyed_by_its_kind() {
        fresh();
        enable();
        on_instruction(&Instr::Test {
            kind: beam_loader::TestKind::IsEqExact,
            fail: Label(0),
            args: vec![Operand::X(0), Operand::X(1)],
        });
        on_instruction(&Instr::Test {
            kind: beam_loader::TestKind::IsEqExact,
            fail: Label(0),
            args: vec![Operand::X(0), Operand::X(1)],
        });
        on_instruction(&Instr::Test {
            kind: beam_loader::TestKind::IsTuple,
            fail: Label(0),
            args: vec![Operand::X(0)],
        });
        let snapshot = snapshot();
        assert_eq!(executed(&snapshot, "Test(IsEqExact)"), 2);
        assert_eq!(executed(&snapshot, "Test(IsTuple)"), 1);
        assert_eq!(snapshot.total_executed, 3);
        fresh();
    }

    #[test]
    fn first_error_keeps_only_the_first_text() {
        fresh();
        enable();
        on_instruction(&Instr::Send);
        on_error(&Instr::Send, &BeamError::BadArg);
        on_instruction(&Instr::Send);
        on_error(
            &Instr::Send,
            &BeamError::Raise(beam_common::Raise::error(beam_common::ExitReason::atom(
                "undef",
            ))),
        );
        let first = snapshot();
        assert_eq!(executed(&first, "Send"), 2, "counted before each body");
        assert_eq!(first.errors.get("Send"), Some(&2));
        assert_eq!(first.total_errors, 2);
        assert_eq!(
            first.first_error.get("Send").map(String::as_str),
            Some("badarg"),
            "the FIRST error's text is the one retained"
        );
        on_error(&Instr::Return, &BeamError::BadArg);
        let after = snapshot();
        assert_eq!(after.errors.get("Return"), Some(&1));
        assert!(
            !after.executed.contains_key("Return"),
            "an error on a variant that never entered the funnel still records its error"
        );
        fresh();
    }

    #[test]
    fn the_dump_is_a_tab_separated_table() {
        fresh();
        enable();
        on_instruction(&Instr::Send);
        on_instruction(&Instr::Send);
        on_error(&Instr::Send, &BeamError::BadArg);
        on_instruction(&Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(1),
        });

        let path = write_dump("unit_test_dump").expect("the dump is written");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("beam_engine_exec_census_unit_test_dump.tsv")
        );
        assert!(
            path.starts_with(std::env::temp_dir()),
            "the dump lives in the temp directory: {}",
            path.display()
        );
        let text = std::fs::read_to_string(&path).expect("the dump is readable");
        let mut lines = text.lines();
        let comment = lines.next().expect("a comment line");
        assert!(comment.starts_with("# "), "comment line: {comment}");
        assert!(comment.contains("section=unit_test_dump"));
        assert!(comment.contains("total_executed=3"));
        assert!(comment.contains("total_errors=1"));
        let header = lines.next().expect("a header line");
        assert_eq!(header, "# variant\texecuted\terrors\tfirst_error");

        let rows: Vec<Vec<&str>> = lines.map(|line| line.split('\t').collect()).collect();
        assert_eq!(rows.len(), 2, "one row per variant, sorted: {rows:?}");
        assert_eq!(rows[0], vec!["Move", "1", "0", "-"]);
        assert_eq!(rows[1], vec!["Send", "2", "1", "badarg"]);
        for row in &rows {
            assert_eq!(row.len(), 4, "every row has four columns: {row:?}");
        }

        std::fs::remove_file(&path).expect("the dump is removed");
        fresh();
    }

    #[test]
    fn the_dump_sanitises_the_section_name() {
        fresh();
        enable();
        let path = write_dump("../escape/attempt").expect("the dump is written");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("beam_engine_exec_census____escape_attempt.tsv"),
            "no path separator survives"
        );
        std::fs::remove_file(&path).expect("the dump is removed");
        fresh();
    }

    /// **The interpreter funnel is counted, and counting changes nothing.** Run the
    /// same synthetic module with the census off and on: the returned value must be
    /// identical, and the enabled run must attribute one execution to each of the four
    /// variants its body executes.
    #[test]
    fn the_executed_census_counts_the_interpreter_funnel() {
        let (uncounted, off) = run_put_list(false);
        assert_eq!(
            off,
            Snapshot::default(),
            "a disabled run records nothing at all"
        );

        let (counted, snapshot) = run_put_list(true);
        assert_eq!(
            counted,
            uncounted,
            "the census changes no result (both runs return 42)"
        );
        assert_eq!(counted, beam_term::make_small(42));

        for variant in ["Label", "Move", "PutList", "Return"] {
            assert!(
                executed(&snapshot, variant) > 0,
                "{variant} must be observed: {:?}",
                snapshot.executed
            );
        }
        assert_eq!(
            executed(&snapshot, "Return"),
            1,
            "the run returns once: {:?}",
            snapshot.executed
        );
        assert_eq!(
            executed(&snapshot, "PutList"),
            1,
            "one put_list in the body: {:?}",
            snapshot.executed
        );
        assert_eq!(
            snapshot.total_executed,
            snapshot.executed.values().sum::<u64>(),
            "the total is Σ the per-variant counts"
        );
        assert_eq!(snapshot.total_errors, 0, "nothing raised: {snapshot:?}");
        assert!(snapshot.errors.is_empty());
        fresh();
    }

    /// **A raising instruction is counted as executed AND as an error**, with its exact
    /// typed error text in `first_error`.
    #[test]
    fn a_raising_instruction_is_counted_as_executed_and_as_an_error() {
        let (module, name) = func_info_module();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        fresh();
        enable();
        let error = machine
            .run_with_tier(&module, name, 0, &[], &mut ctx, Tier::Interpreter)
            .expect_err("a func_info body raises function_clause");
        let raised = snapshot();
        fresh();

        assert_eq!(
            raised.executed.get("FuncInfo"),
            Some(&1),
            "the raising instruction IS counted as executed: {:?}",
            raised.executed
        );
        assert_eq!(
            executed(&raised, "Label"),
            1,
            "the label before it ran too: {:?}",
            raised.executed
        );
        assert_eq!(
            executed(&raised, "Return"),
            0,
            "the raise ends the run before `return`"
        );
        assert_eq!(raised.errors.get("FuncInfo"), Some(&1));
        assert_eq!(raised.total_errors, 1);
        let first = raised
            .first_error
            .get("FuncInfo")
            .expect("the raising variant keeps its error text");
        assert_eq!(first, &format!("{error}"), "the exact typed error text");
        // Every engine raise (`Machine::throw`) carries its reason as a live reason
        // *term* with `ExitReason::Structured` as the term-independent class, so `Display`
        // renders the placeholder — this is the reason the census stores the `Display`
        // text verbatim rather than inventing a friendlier rendering.
        assert_eq!(first, "error <structured reason>");
        assert!(error.is_raise(), "a raise, not a capability gap");

        // A typed *capability* error, whose `Display` IS self-describing: `move` from a
        // register outside `MAX_REGISTER` is `BeamError::BadArg`.
        let mut f = Fixture::new("census_badarg");
        let entry = f.add_function(
            "g",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: Operand::X(crate::machine::MAX_REGISTER + 1),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        );
        f.add_export("g", 0, entry);
        let name = f.atom("g");
        let bad = f.finish();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        fresh();
        enable();
        let error = machine
            .run_with_tier(&bad, name, 0, &[], &mut ctx, Tier::Interpreter)
            .expect_err("an out-of-range register is a typed badarg");
        let badarg = snapshot();
        fresh();
        assert_eq!(
            badarg.errors.get("Move"),
            Some(&1),
            "snapshot: {badarg:?} error: {error:?}"
        );
        assert_eq!(
            badarg.first_error.get("Move").map(String::as_str),
            Some("badarg"),
            "the exact typed error text"
        );
        assert_eq!(
            badarg.first_error.get("Move"),
            Some(&format!("{error}")),
            "the text the funnel returned, verbatim"
        );
        assert_eq!(
            executed(&badarg, "Move"),
            1,
            "and it is counted as executed too"
        );
    }

    /// The census is per thread: another thread's counts never leak into this one.
    #[test]
    fn the_census_is_per_thread() {
        fresh();
        enable();
        on_instruction(&Instr::Send);
        let mine = snapshot();
        let other = std::thread::spawn(|| {
            fresh();
            enable();
            on_instruction(&Instr::Return);
            let theirs = snapshot();
            fresh();
            theirs
        })
        .join()
        .expect("the worker thread finishes");
        assert_eq!(executed(&other, "Return"), 1);
        assert_eq!(
            executed(&other, "Send"),
            0,
            "the worker never saw this thread's counts"
        );
        assert_eq!(snapshot(), mine, "and this thread's census is unchanged");
        fresh();
    }
}

