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

//! The interpreter's **capability-gap inventory**.
//!
//! Every remaining gap in the engine's execution surface, driven through the real
//! public entry (`Machine::run_label` / `Machine::resume_outcome` over a synthetic
//! `Module` + `MinimalContext`) and pinned to its **exact** typed error, printing a
//! `GAP: <name> — <error>` line. Never a wrong value and never the
//! `families::dispatch` "widened opcode not yet executed" sentinel; a regression
//! that turns a typed error into either fails here.
//!
//! This list lives in its own test target (rather than in `tests/stdlib_sweep.rs`)
//! so neither file grows past ~1000 lines. Its **real-corpus** counterpart is
//! `tests/stdlib_sweep.rs::blocked_mechanisms_report_their_typed_error`, which
//! calls real installed modules' exports and pins the same exact typed errors.

use std::collections::BTreeMap;

use beam_common::BeamError;
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{
    Function, Instr, Label, MapWriteArgs, Module, Operand, PutRecordArgs, TestKind, NO_FAIL,
};
use beam_term::{make_small, Atom, Eterm};

// ---------------------------------------------------------------------------
// The capability-gap inventory — every remaining engine gap pinned to its EXACT
// typed error
// ---------------------------------------------------------------------------
//
// Each gap below is driven through the **real** public execution path
// (`Machine::run_label` / `Machine::resume_outcome` over a synthetic `Module` +
// `MinimalContext`), exactly like `tests/opcodes.rs` and `tests/census.rs` do.
// The assertion is always the **exact** typed error (`unsupported_opcode: <what>`
// reason atom, or `BeamError::BadArg`) — never a wrong value, never the
// `families::dispatch` "widened opcode not yet executed" sentinel. A regression
// that turns a typed error into a value (or into the sentinel) fails here.
//
// Corpus reachability (evidence: `tests/census.rs`'s counts over the whole installed
// OTP — 1366 `.beam` over 34 applications, 2,888,257 instructions):
//   * CORPUS-REACHABLE  — the installed toolchain emits the instruction today, so
//     the gap is a live limitation a real module can hit.
//   * FORWARD-COMPAT-ONLY — the instruction has **zero** corpus occurrences, so the
//     gap exists only for a future toolchain that emits it. The census's full
//     zero-occurrence set is the 7 OTP-29 native-record opcodes (`IsNativeRecord`,
//     `GetRecordElements`, `PutRecord`, `IsRecordAccessible`, `GetRecordField`,
//     `GetRecordElementsId`, `UpdateRecordId`) plus `Deallocate` (the loader fuses
//     every corpus `deallocate;return` into `DeallocateReturn`, so the standalone
//     variant is unreached), `BsTestTail2`, `SetTupleElement` (the corpus lowers no
//     in-place tuple write: a general `setelement/3` builds a fresh tuple, and a
//     record update lowers to `update_record`), `ExecutableLine`, `DebugLine`,
//     `Generic` — 13 of the 108 `Instr` variants.
//     Toolchain evidence for the native-record family: `erl_features:configurable()`
//     ⇒ `[compr_assign,maybe_expr]` (`native_record` is not in it), `erlc
//     -enable-feature native_record` ⇒ "the feature 'native_record' does not
//     exist.", and a `-record`-update module (`f(X) -> X#r{a = 1}.`) disassembles to
//     `update_record`, never one of the eight native-record opcodes.
fn gap_labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A builder for the synthetic modules the gap inventory drives.
struct GapFixture {
    module: Module,
}

impl GapFixture {
    fn new() -> GapFixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("gap");
        GapFixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function; returns its entry label (the first label in `code`).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = gap_labels_of(&code);
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

/// Drive one function of `module` through the real public entry with the default
/// (`MinimalContext`) context: no BIF hook, no apply hook, **no mailbox**.
fn run_gap(module: &Module, entry: Label, args: &[Eterm]) -> std::result::Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.run_label(module, entry, args.len() as u32, args, &mut ctx)
}

/// Drive one function with a caller-supplied context (for a context that is *not*
/// `MinimalContext`).
fn run_gap_with(
    module: &Module,
    entry: Label,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> std::result::Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_label(module, entry, args.len() as u32, args, ctx)
}

/// Pin `result` to the exact typed `unsupported_opcode: <want>` error, print the
/// one-line `GAP:` inventory entry, and prove it is **not** the unclaimed sentinel.
fn pin(name: &str, result: std::result::Result<Eterm, BeamError>, want: &str) {
    let want_atom = format!("unsupported_opcode: {want}");
    match result {
        Err(BeamError::Raise(raise)) => {
            let got = raise.reason.as_atom().unwrap_or("<non-atom reason>");
            println!("GAP: {name} — {got}");
            assert_eq!(got, want_atom, "gap `{name}` produced the wrong typed error");
            assert!(
                !got.contains("widened opcode not yet executed"),
                "gap `{name}` must never be the unclaimed sentinel"
            );
        }
        other => panic!("GAP `{name}`: expected the typed error {want_atom:?}, got {other:?}"),
    }
}

/// Pin `result` to the typed `badarg` (`BeamError::BadArg` — `bad_register` /
/// `unknown_label` carry no atom) and print the inventory entry.
fn pin_badarg(name: &str, result: std::result::Result<Eterm, BeamError>) {
    println!("GAP: {name} — BeamError::BadArg (the BEAM `badarg` atom)");
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("GAP `{name}`: expected BeamError::BadArg, got {other:?}"),
    }
}

/// The whole inventory. Groups mirror the owning family/file; each line names the
/// exact typed error the engine produces today.
#[test]
fn interpreter_capability_gaps_report_their_exact_typed_error() {
    // --- core loop: the `NO_FAIL` guard branches (src/interpreter.rs) ---------
    // A `NO_FAIL` (`fail == 0`) failure is now a DEFINED raise, not an undefined
    // fall-through: the shared helper `raise_function_clause_no_fail` raises class
    // `error` with the bare atom `function_clause`, resolved ONLY from the running
    // module's own atom table. A module that cannot name `function_clause` degrades
    // to the honest typed gap pinned here. The installed `erlc` ALWAYS supplies a
    // real fail label for these instructions, so a `{f,0}` failure has NO `erl`
    // oracle — it is pinned from C ground truth
    // (`../../erts/emulator/beam/emu/macros.tab` `FAIL`/`FAIL_HEAD_OR_BODY`,
    // `beam_common.c` `handle_error`, `emu/beam_emu.c` `find_func_info`) plus the
    // engine's own `func_info` precedent.
    // CORPUS-REACHABLE: `test`/`select_val` are emitted for every match.
    {
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            // `is_tuple` on a small int fails with `fail == NO_FAIL` (the compiler's
            // "cannot fail" marker): the defined raise is class `error` / bare atom
            // `function_clause` from the module's own atom table, but this synthetic
            // module cannot name that atom, so it surfaces as the typed gap below.
            Instr::Test {
                kind: TestKind::IsTuple,
                fail: NO_FAIL,
                args: vec![Operand::X(0)],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "test/no_fail",
            run_gap(&module, entry, &[make_small(0)]),
            "test: no 'function_clause' atom in the module",
        );
    }
    {
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::SelectVal {
                value: Operand::X(0),
                fail: NO_FAIL,
                pairs: vec![(Operand::Integer(99), Label(2))],
            },
            Instr::Label(Label(2)),
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "select_val/no_fail",
            run_gap(&module, entry, &[make_small(0)]),
            "select_val: no 'function_clause' atom in the module",
        );
    }

    // --- maps family (src/families/maps.rs) ----------------------------------
    // A `get_map_elements`/`has_map_fields` `NO_FAIL` (`fail == 0`) failure is the same
    // DEFINED raise as the core loop: class `error` with the bare atom `function_clause`,
    // resolved ONLY from the running module's own atom table (the modules below cannot
    // name it, hence the typed gaps). The installed `erlc` ALWAYS supplies a real fail
    // label for these instructions, so a `{f,0}` failure has NO `erl` oracle — it is
    // pinned from C ground truth (`../../erts/emulator/beam/emu/macros.tab`
    // `FAIL`/`FAIL_HEAD_OR_BODY`, `beam_common.c` `handle_error`, `emu/beam_emu.c`
    // `find_func_info`) plus the engine's own `func_info` precedent. The
    // `put_map_assoc`/`put_map_exact` `{badmap,Source}`/`{badkey,Key}` raises are
    // UNCHANGED (see below).
    // CORPUS-REACHABLE: map reads/updates are emitted by every map-using module.
    {
        // `get_map_elements` with an absent key and `fail == NO_FAIL`: the defined
        // raise is class `error` / bare atom `function_clause` from the module's own
        // atom table, which this module cannot name, hence the typed gap. The map is
        // built in a separate context that stays alive for the run.
        let mut f = GapFixture::new();
        let a = f.atom("a");
        let b = f.atom("b");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::GetMapElements {
                fail: NO_FAIL,
                source: Operand::X(0),
                pairs: vec![(Operand::Atom(a), Operand::X(1))],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();

        let mut builder = MinimalContext::new();
        let map = terms::make_flatmap(&mut builder, &[(beam_term::make_atom(b), make_small(1))])
            .expect("a flat map");
        pin(
            "get_map_elements/no_fail",
            run_gap(&module, entry, &[map]),
            "maps: no 'function_clause' atom in the module",
        );
    }
    {
        // `has_map_fields` with an absent required key and `fail == NO_FAIL`: the
        // defined raise is class `error` / bare atom `function_clause` from the
        // module's own atom table, which this module cannot name, hence the typed gap.
        let mut f = GapFixture::new();
        let a = f.atom("a");
        let b = f.atom("b");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::HasMapFields {
                fail: NO_FAIL,
                source: Operand::X(0),
                keys: vec![Operand::Atom(a)],
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();

        let mut builder = MinimalContext::new();
        let map = terms::make_flatmap(&mut builder, &[(beam_term::make_atom(b), make_small(1))])
            .expect("a flat map");
        pin(
            "has_map_fields/no_fail",
            run_gap(&module, entry, &[map]),
            "maps: no 'function_clause' atom in the module",
        );
    }
    {
        // `put_map_assoc` / `put_map_exact` with a **non-map** source and
        // `fail == NO_FAIL`: the C ground truth is a `{badmap, Source}` raise, and
        // the engine can only raise it if the module can name the `badmap` atom.
        // This module deliberately cannot, so the gap surfaces as its exact
        // "the tag atom is absent" error rather than a fabricated atom.
        let mut f = GapFixture::new();
        let a = f.atom("a");
        let assoc = vec![
            Instr::Label(Label(1)),
            Instr::PutMapAssoc(Box::new(MapWriteArgs {
                fail: NO_FAIL,
                source: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                pairs: vec![(Operand::Atom(a), Operand::Integer(1))],
            })),
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, assoc);
        let module = f.finish();
        pin(
            "put_map_assoc/no_fail",
            run_gap(&module, entry, &[make_small(7)]),
            "maps: no 'badmap' atom in the module",
        );
    }
    {
        let mut f = GapFixture::new();
        let a = f.atom("a");
        let exact = vec![
            Instr::Label(Label(1)),
            Instr::PutMapExact(Box::new(MapWriteArgs {
                fail: NO_FAIL,
                source: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                pairs: vec![(Operand::Atom(a), Operand::Integer(1))],
            })),
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, exact);
        let module = f.finish();
        pin(
            "put_map_exact/no_fail",
            run_gap(&module, entry, &[make_small(7)]),
            "maps: no 'badmap' atom in the module",
        );
    }

    // --- records family (src/families/records/mod.rs) ------------------------
    // FORWARD-COMPAT: the installed toolchain never emits the `native_record`
    // family (`erl_features:configurable()` ⇒ `[compr_assign,maybe_expr]`; `erlc
    // -enable-feature native_record` ⇒ "the feature 'native_record' does not
    // exist"), so all 7 opcodes have ZERO corpus occurrences. Pinned anyway so a
    // future toolchain cannot regress them silently.
    //
    // A `NO_FAIL` (`fail == 0`) failure is the SAME DEFINED raise the core loop and
    // the maps family produce: `fail_step` routes it to
    // `raise_function_clause_no_fail`, i.e. class `error` with the bare atom
    // `function_clause` resolved through the live atom namespace
    // (`machine::namespace_atom`). The synthetic modules below cannot name that
    // atom, so the honest typed gap pinned here is the reason-atom lookup, never a
    // fall-through with a wrong value.
    {
        // `is_any_native_record` with a non-record source and `NO_FAIL`.
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::IsAnyNativeRecord {
                fail: NO_FAIL,
                src: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "is_any_native_record/no_fail",
            run_gap(&module, entry, &[make_small(0)]),
            "is_any_native_record: no 'function_clause' atom in the module",
        );
    }
    {
        // `put_record`'s **create** form (`Src == nil`) now builds the running
        // module's **own** record (`src/families/records/mod.rs::create_native_record`
        // over `beam_loader::Module::records`). The one genuine remaining gap is an
        // `Id` naming **another module's** record: C resolves that through the code
        // server's record table (`erl_create_native_record_jit`,
        // `erl_record.c:412-500`), a facility this runtime does not have — never a
        // guessed definition. The `{Module,Name}` pair is a real tuple term, so it is
        // materialized in `x(0)` and passed as a register operand (the foreign module
        // `other` differs from the running module `gap`). FORWARD-COMPAT (see above):
        // no installed toolchain emits the native-record family.
        let mut f = GapFixture::new();
        let foreign = f.atom("other");
        let rec = f.atom("rec");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Atom(foreign),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Atom(rec),
                dst: Operand::X(1),
            },
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: vec![Operand::X(0), Operand::X(1)],
            },
            Instr::PutRecord(Box::new(PutRecordArgs {
                fail: NO_FAIL,
                id: Operand::X(0),
                src: Operand::Nil,
                dst: Operand::X(2),
                live: 0,
                updates: vec![],
            })),
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "put_record/create-foreign-module",
            run_gap(&module, entry, &[]),
            "put_record: create: no record definition table for another module",
        );
    }
    {
        // `put_record`'s **update** form with a non-record source and `NO_FAIL`.
        let mut f = GapFixture::new();
        let rec = f.atom("rec");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::PutRecord(Box::new(PutRecordArgs {
                fail: NO_FAIL,
                id: Operand::Atom(rec),
                src: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                updates: vec![],
            })),
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "put_record/no_fail",
            run_gap(&module, entry, &[make_small(0)]),
            "put_record: no 'function_clause' atom in the module",
        );
    }

    // --- exceptions family (src/families/exceptions.rs) ----------------------
    // CORPUS-REACHABLE: `badmatch`/`raise`/`try_case` are emitted for every match.
    {
        // A `badmatch` raise on a module whose atom table cannot name the tag: the
        // engine refuses to substitute a *different* atom.
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Badmatch {
                value: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "exceptions/unnameable-raise-reason-atom",
            run_gap(&module, entry, &[make_small(0)]),
            "exceptions: no 'badmatch' atom in the module",
        );
    }
    {
        // `raise` with no exception currently being handled (`pending_class` unset).
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Raise {
                trace: Operand::Nil,
                value: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 1, code);
        let module = f.finish();
        pin(
            "exceptions/raise-with-nothing-handled",
            run_gap(&module, entry, &[make_small(0)]),
            "exceptions: raise with no exception being handled",
        );
    }
    {
        // `try_case` reached without an exception having landed: `x(0)` is not the
        // `THE_NON_VALUE` marker. `Try` pushes the (balanced) catch frame first.
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Try {
                frame: Operand::X(1),
                fail: Label(2),
            },
            Instr::TryCase {
                frame: Operand::X(2),
            },
            Instr::Label(Label(2)),
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "try_case/x0-not-THE_NON_VALUE",
            run_gap(&module, entry, &[]),
            "try_case: x(0) is not THE_NON_VALUE (no exception landed)",
        );
    }
    {
        // An unbalanced catch-frame pop: `try_case` with no frame pushed.
        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::TryCase {
                frame: Operand::X(1),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "try_case/unbalanced-catch-frames",
            run_gap(&module, entry, &[]),
            "try_case: unbalanced catch frames",
        );
    }

    // --- `ExecutionContext` trait defaults (src/context.rs) ------------------
    // CORPUS-REACHABLE: a `MinimalContext` with no hook installed.
    {
        // `apply_fun` with no apply hook (`call_fun2`).
        let mut f = GapFixture::new();
        let safe = f.atom("safe");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::CallFun2 {
                tag: Operand::Atom(safe),
                arity: 0,
                func: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "context/apply_fun-has-no-hook",
            run_gap(&module, entry, &[]),
            "apply_fun",
        );
    }
    {
        // The nine mailbox seam methods, each the trait's typed `unsupported` on a
        // bare `MinimalContext` (no mailbox installed).
        let cases: Vec<(&str, Instr, &str)> = vec![
            ("send", Instr::Send, "send_message"),
            (
                "loop_rec",
                Instr::LoopRec {
                    fail: Label(2),
                    dst: Operand::X(1),
                },
                "receive_next",
            ),
            (
                "loop_rec_end",
                Instr::LoopRecEnd { target: Label(2) },
                "receive_skip",
            ),
            ("timeout", Instr::Timeout, "receive_reset"),
            ("remove_message", Instr::RemoveMessage, "receive_remove"),
            // The four `recv_marker_*` opcodes *are* claimed by the receive family,
            // which names the missing capability by opcode rather than forwarding
            // the seam's bare `unsupported_opcode: receive_marker_<op>` default.
            (
                "recv_marker_reserve",
                Instr::RecvMarkerReserve {
                    marker: Operand::X(1),
                },
                "recv_marker_reserve: the host does not serve receive markers",
            ),
            (
                "recv_marker_bind",
                Instr::RecvMarkerBind {
                    marker: Operand::X(1),
                    reference: Operand::X(2),
                },
                "recv_marker_bind: the host does not serve receive markers",
            ),
            (
                "recv_marker_use",
                Instr::RecvMarkerUse {
                    reference: Operand::X(1),
                },
                "recv_marker_use: the host does not serve receive markers",
            ),
            (
                "recv_marker_clear",
                Instr::RecvMarkerClear {
                    reference: Operand::X(1),
                },
                "recv_marker_clear: the host does not serve receive markers",
            ),
        ];
        for (label, instr, reason) in cases {
            let mut f = GapFixture::new();
            let code = vec![Instr::Label(Label(1)), instr, Instr::Label(Label(2)), Instr::Return];
            let entry = f.add_function("f", 0, code);
            let module = f.finish();
            pin(
                &format!("context/mailbox-{label}"),
                run_gap(&module, entry, &[]),
                reason,
            );
        }
    }
    {
        // `receive` with no mailbox: `wait`/`wait_timeout(non-zero)` **suspend**, and
        // the non-blocking entry folds that into the documented typed error (there
        // is no host timer at M1, so a non-zero/`infinity` timeout suspends too).
        let mut f = GapFixture::new();
        let infinity = f.atom("infinity");
        let code = vec![
            Instr::Label(Label(1)),
            Instr::WaitTimeout {
                target: Label(2),
                time: Operand::Atom(infinity),
            },
            Instr::Label(Label(2)),
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        pin(
            "receive/wait_timeout-infinity-suspends",
            run_gap(&module, entry, &[]),
            "run: process blocked (no message available)",
        );
    }
    {
        // `alloc_words` on a context that does **not** override it (the trait
        // default): a core `move` of an out-of-range integer materializes an exact
        // bignum, which needs raw heap words.
        struct BareCtx;
        impl ExecutionContext for BareCtx {
            fn call_bif(
                &mut self,
                _module: &str,
                _function: &str,
                _arity: u32,
                _args: &[Eterm],
            ) -> std::result::Result<Eterm, BeamError> {
                Err(BeamError::BadArg)
            }
            fn ensure_heap(
                &mut self,
                _heap_need: u32,
                _live: u32,
                _x_regs: &mut [Eterm],
                _stack: &mut [Eterm],
            ) -> std::result::Result<(), BeamError> {
                Ok(())
            }
        }

        let mut f = GapFixture::new();
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Integer(beam_term::MAX_SMALL + 1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        let entry = f.add_function("f", 0, code);
        let module = f.finish();
        let mut ctx = BareCtx;
        pin(
            "context/alloc_words-trait-default",
            run_gap_with(&module, entry, &[], &mut ctx),
            "alloc_words",
        );
    }

    // --- interpreter / operand guards (src/interpreter.rs, src/operand.rs) ----
    // CORPUS-REACHABLE: malformed/edge bytecode paths.
    {
        let mut f = GapFixture::new();
        f.add_function("f", 0, vec![Instr::Label(Label(1)), Instr::Return]);
        let module = f.finish();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        // A `pc` past the function's last instruction is the loop's own typed error.
        pin(
            "run/pc-out-of-range",
            machine
                .resume_outcome(&module, 0, 99_999, &mut ctx)
                .map(|_| make_small(0)),
            "run: pc out of range",
        );
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        pin(
            "run/function-index-out-of-range",
            machine
                .resume_outcome(&module, 99, 0, &mut ctx)
                .map(|_| make_small(0)),
            "run: function index out of range",
        );
    }
    {
        // An out-of-range register (`bad_register`) and an absent jump target
        // (`unknown_label`) are both the typed BEAM `badarg`.
        let mut f = GapFixture::new();
        let entry = f.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: Operand::X(2000),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        );
        let module = f.finish();
        pin_badarg("operand/bad_register", run_gap(&module, entry, &[]));

        let mut f = GapFixture::new();
        let entry = f.add_function(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Jump { target: Label(99) },
                Instr::Return,
            ],
        );
        let module = f.finish();
        pin_badarg("jump/unknown_label", run_gap(&module, entry, &[]));
    }
}
