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

//! Per-variant unit tests for the native-record instruction family.
//!
//! Every one of the seven typed native-record [`Instr`] variants gets its own
//! group of tests covering the three decisions the family can make:
//!
//! * the **success** path (fall through, with the documented register effects),
//! * the **fail-branch** path (a record-semantics failure the C reports with
//!   `$FAIL($Fail)` / `$FAIL_HEAD_OR_BODY($Fail)` must produce
//!   `Step::Goto { func, pc }` for the resolved `fail` label), and
//! * the [`NO_FAIL`] path (the engine's single failed-clause raise, class `error` with
//!   the bare atom `function_clause`, or the typed `unsupported` when the namespace
//!   cannot name that atom — never a fall-through with a wrong result).
//!
//! The fixtures are real native records built through [`beam_term::write_record`]
//! (reached via the family's [`raw`](super::raw) forwarder) into the
//! [`MinimalContext`] arena, so the tests exercise the frozen layout without ever
//! re-deriving it. The instructions are hand-built [`Instr`]s driven straight
//! through [`exec`](super::exec) over a [`Machine`] and the mock context — the
//! loader is not involved.
//!
//! Everything asserted here is the family's own documented decision (see
//! `mod.rs`), which in turn mirrors `../../erts/emulator/beam/erl_record.c` and
//! `emu/instrs.tab`.
//!
//! The suite is split by responsibility, one submodule per opcode group:
//!
//! * [`native`] — `is_native_record` (the definition-identity test),
//! * [`elements`] — `get_record_elements` / `get_record_elements_id`,
//! * [`update`] — `put_record` (its update path and the create form's typed
//!   unsupported id cases) and `update_record_id`,
//! * [`access`] — `is_record_accessible`,
//! * [`field`] — `get_record_field`,
//! * [`rooted`] — the two build paths driven under the mock **moving** collector
//!   (the rooted-allocation reroute's regression proof).
//!
//! This file owns the fixtures/helpers those submodules share and the
//! family-membership test.

use crate::interpreter::LabelMap;

use beam_common::BeamError;
use beam_term::{make_atom, make_small, NIL};

use super::*;
use crate::context::MinimalContext;

mod access;
mod elements;
mod field;
mod native;
mod rooted;
mod update;

// ---------------------------------------------------------------------------
// Shared fixtures and helpers
// ---------------------------------------------------------------------------

/// A synthetic module (`name = m`) with an owned atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new() -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
        Fixture { module }
    }

    /// Intern `name`, returning its atom.
    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    fn module(&self) -> &Module {
        &self.module
    }
}

/// Build a native record with `fields` (`(key atom, value)` pairs, any order) and
/// return the tagged **instance** root.
///
/// The construction goes through [`beam_term::write_record`] (via the family's
/// `raw` forwarder) into the context arena, so the frozen layout stays owned by
/// `beam_term` — this test never re-derives it. The definition hash is an
/// arbitrary small integer (`beam_term` stores it verbatim; nothing in these
/// tests reads it).
fn record(
    ctx: &mut MinimalContext,
    owner: Atom,
    name: Atom,
    exported: bool,
    fields: &[(Atom, Eterm)],
) -> Eterm {
    let fields: Vec<(Eterm, Eterm)> = fields.iter().map(|(k, v)| (make_atom(*k), *v)).collect();
    let hp = crate::terms::test_support::reserve_words(ctx, beam_term::record_block_words(fields.len()))
        .expect("arena allocation");
    raw::write_record(hp, owner, name, exported, &fields, make_small(0))
        .expect("record construction")
}

/// The value of the field keyed `key` in `rec`, or `None`.
fn field(module: &Module, rec: Eterm, key: Atom) -> Option<Eterm> {
    raw::record_get(rec, key, &module.atom_table).expect("a valid record")
}

/// A record with fields `a = 11`, `b = 22` in module `m` named `r`.
fn two_field_record(fx: &mut Fixture, ctx: &mut MinimalContext) -> (Eterm, Atom, Atom, Atom) {
    let (m, r, a, b) = (fx.atom("m"), fx.atom("r"), fx.atom("a"), fx.atom("b"));
    let rec = record(
        ctx,
        m,
        r,
        false,
        &[(b, make_small(22)), (a, make_small(11))],
    );
    (rec, a, b, r)
}

/// A label map with `label` resolved to `(func, pc)`.
fn labels_with(label: Label, func: u32, pc: u32) -> LabelMap {
    LabelMap::from([(label, (func, pc))])
}

/// Drive one instruction through the family over `machine`/`ctx`.
fn run(
    module: &Module,
    labels: &LabelMap,
    machine: &mut Machine,
    instr: &Instr,
    ctx: &mut MinimalContext,
) -> beam_common::Result<Option<Step>> {
    exec(machine, module, labels, instr, 0, 0, ctx)
}

/// The reason **atom text** of an uncaught raise, for the two documented shapes the
/// `NO_FAIL` path produces (see `mod.rs`'s [`fail_step`](super::fail_step)):
///
/// * the **engine diagnostic** — `unsupported_opcode: <what>: no 'function_clause'
///   atom in the module`, carried as an atom [`ExitReason`] (no live reason term);
/// * the **program-level failed-clause raise** — class `error` whose reason is the
///   bare atom `function_clause`, resolved through the live atom namespace
///   (`machine::namespace_atom`) and carried out of the machine as the raise's
///   **live reason term**. An uncaught structured raise keeps
///   `ExitReason::Structured` (so `as_atom()` answers `None` by design,
///   `beam_common::ExitReason` is a unit variant) and the term rides
///   `Raise::live_reason` ([`Machine::throw_with_trace`]'s uncaught arm), so the
///   atom is rendered here through `module`'s own atom table — the table the raise
///   resolved the index against.
///
/// A reason that is neither shape is a test failure, never a silently accepted
/// value.
fn raise_reason_atom(module: &Module, result: beam_common::Result<Option<Step>>) -> String {
    let raise = match result.expect_err("expected a typed error") {
        BeamError::Raise(raise) => raise,
        other => panic!("expected a typed raise, got {other:?}"),
    };
    assert_eq!(
        raise.class,
        beam_common::ExceptionClass::Error,
        "both the diagnostic and the failed-clause raise are class `error`"
    );
    if let Some(diagnostic) = raise.reason.as_atom() {
        assert!(
            raise.live_reason().is_none(),
            "an engine diagnostic carries no live reason term"
        );
        return diagnostic.to_string();
    }
    let reason = raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
        .expect("an uncaught structured raise carries its live reason term");
    assert!(
        beam_term::is_atom(reason),
        "the failed-clause reason is the bare atom, got the term {reason:?}"
    );
    module
        .atom_table
        .name(beam_term::atom_val(reason))
        .expect("the reason atom is one the running module can name")
        .to_string()
}

// ---------------------------------------------------------------------------
// Family membership
// ---------------------------------------------------------------------------

#[test]
fn a_variant_outside_the_records_family_is_declined() {
    let fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::with_max_register(4);

    assert_eq!(
        run(
            fx.module(),
            &LabelMap::new(),
            &mut machine,
            &Instr::Return,
            &mut ctx
        )
        .unwrap(),
        None
    );
}
