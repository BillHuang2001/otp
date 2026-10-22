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

//! The **foreign record-definition** seam: `put_record`'s *create* form for another
//! module's `{Module,Name}` record id.
//!
//! [`Instr::PutRecord`](beam_loader::Instr::PutRecord) with `Src == nil` is the one
//! record instruction that must resolve a **definition** before it can build anything,
//! and `emu/ops.tab:1648-1651` splits its `Id` operand in two:
//!
//! * an **atom** `Id` names a record of the running module — the definition is right
//!   there in the module's `Recs`-derived table ([`beam_loader::Module::record`]), no
//!   host seam involved;
//! * a **`{Module,Name}`** pair is `i_create_native_record`
//!   (`emu/instrs.tab:1373`) → `erl_create_native_record` (`erl_record.c:356-410`),
//!   which asks the code server's record table (`erts_record_find_entry`, `:376`) and
//!   gates on `is_exported == am_true` (`:401`).
//!
//! The second shape's table belongs to the **host** — this crate never loads modules —
//! so `beam_engine` asks for it through the defaulted
//! [`ExecutionContext::foreign_record_definition`] seam and this module answers it.
//! The family's own half (the export gate and the `{badrecord, Id}` raise) is
//! `beam_engine/src/families/records/create.rs`; only the lookup is here.
//!
//! # The C's record table and this runtime's
//!
//! C's `erts_record_find_entry(module, name, code_ix)` (`erl_record.c:163-178`) is a
//! lookup keyed by the two `module`/`name` **atom terms** in the code's current
//! generation of record tables, and `erl_create_native_record` (`:354-410`) walks it,
//! applies the `is_exported == am_true` gate and otherwise raises
//! `p->fvalue = id; p->freason = EXC_BADRECORD`. A `.beam`'s `Recs` chunk populates it
//! at load time (`erts_record_put`, `:173-178`; `jit/asm_load.c:1246-1260`,
//! `emu/emu_load.c:735`).
//!
//! Here the same data lives **structurally** in the [`Module`] the code server retains
//! for the module's whole life: [`Module::records`] in declaration order,
//! [`Module::record_by_name`] the keyed lookup. [`loaded_record`] is that lookup, and
//! it is **shared** with the `beam_bifs` `RecordSource` bridge
//! ([`super::bridges::RecordBridge`]) so the two record-table consumers cannot drift.
//!
//! # The atom namespace (contract, not a convention)
//!
//! [`Atom`] is a **table-local index**, and the two sides of this seam live in *two*
//! different tables' worth of numbering:
//!
//! * the `owner`/`name` arguments are atom **values** the running module's code
//!   carries as operands (its `{Module,Name}` literal), i.e. global ids of this
//!   runtime's **one** live shared table;
//! * the returned definition's `name`/`fields[i].name` are ids of the **defining**
//!   module's load-time *clone* of that same table
//!   ([`Module::atom_table`](beam_loader::Module)). [`CodeServer::load_bytes`] interns
//!   the image's `AtU8` names into the live table first and then appends the
//!   literal-only atoms in index order, and interning only ever appends — so the two
//!   id spaces agree, and an id from the definition names the same atom live.
//! * the definition's `default` [`Eterm`]s are ordinary **terms**, stored verbatim.
//!   They live in the defining module's immutable literal/attribute heap, which the
//!   code server retains for the runtime's whole life, so a copied default is valid at
//!   any point.
//!
//! Consequently a name this module cannot spell in the **live** table is `Ok(None)` —
//! never a re-interned or invented atom — and the engine's own `live_atom`
//! (`create.rs`) re-checks every returned name before it reaches a term, so a foreign
//! index can never silently name a different atom.

use beam_common::Result;
use beam_loader::{Module, RecordDefinition};
use beam_term::Atom;

use super::runtime_context::RuntimeContext;
use crate::codeserver::CodeServer;

/// The **loaded** module `module` and its record definition `record`, resolved by
/// **name** — the one lookup shared by the two consumers of the code server's record
/// table (the `records:get_definition/2` bridge and the engine's
/// [`ExecutionContext::foreign_record_definition`](beam_engine::ExecutionContext::foreign_record_definition)
/// seam).
///
/// `None` covers both misses C distinguishes only by *why* it looked: the module is not
/// loaded (`erts_record_find_entry` has no table for it) and the module is loaded but
/// declares no such record (the entry scan's own miss, `erl_record.c:163-178`). A caller
/// that must tell the two apart checks [`CodeServer::is_loaded`] itself — the create
/// form does, because a *loaded* module's missing record is the C's
/// `error:{badrecord, Id}` while an unloadable one is this host's honest
/// "cannot answer" (`create.rs`'s module docs).
///
/// The returned references borrow the code server's retained [`Module`], so the
/// definition outlives the call and its atoms/terms stay valid for the runtime's life.
pub(super) fn loaded_record<'m>(
    code_server: &'m CodeServer,
    module: &str,
    record: &str,
) -> Option<(&'m Module, &'m RecordDefinition)> {
    let module = code_server.module(module)?;
    let record = module.record_by_name(record)?;
    Some((module, record))
}

impl RuntimeContext<'_> {
    /// The host's answer to
    /// [`ExecutionContext::foreign_record_definition`](beam_engine::ExecutionContext::foreign_record_definition)
    /// — see the module docs for the C ground truth and the atom-namespace contract.
    ///
    /// The steps are ordered so that nothing is re-interned, nothing is guessed and every
    /// answer is the C's:
    ///
    /// 1. **Name both atoms in the live shared namespace**
    ///    ([`Runtime::atoms`](crate::Runtime::atoms)). This hook is asked from *inside* an
    ///    instruction of a module that is not the owner, so there is no running-module
    ///    snapshot to read; and a name the live table cannot spell means the host cannot
    ///    answer at all (`Ok(None)`) — never `Module::atom_name`, never an invented atom.
    ///    Both names are taken as **owned** `String`s here, before any load: the steps
    ///    below borrow the runtime mutably and interning allocates.
    /// 2. **Load the owner when the code server does not hold it**, exactly as
    ///    `code:ensure_loaded/1` does ([`crate::native::code`]): locate `<owner>.beam`
    ///    through the runtime's own lookup ([`Runtime::find_module_file`], which also seeds
    ///    the system library paths) and then [`CodeServer::load`]. This is the
    ///    **load-then-retry** half of the C (`erl_create_native_record_jit`,
    ///    `erl_record.c:412-507`, and `erl_finish_create_record_jit`, `:509-565`) that the
    ///    engine deliberately does not have — see the seam's "Deviation from C". A load
    ///    failure (no such module on the code path, a malformed or mis-named image) is
    ///    `Ok(None)`: the host cannot answer, and the engine keeps its typed
    ///    `… create: no record definition table for another module`.
    /// 3. **Resolve the definition** through [`loaded_record`]:
    ///
    ///    * found ⇒ returned **verbatim**, `is_exported` included. The flag is part of
    ///      the answer, not a filter: the engine raises the C's `error:{badrecord, Id}`
    ///      for `false` (`erl_record.c:401-409`) and would otherwise turn that raise into
    ///      a capability error.
    ///    * not found, and step 2 is why the owner **is** loaded ⇒ an empty,
    ///      non-exported definition. That reproduces the C's fall-through: the module's
    ///      table exists and has no such entry, so `erts_record_find_entry` misses and
    ///      `EXC_BADRECORD` is raised with the `id` term (`erl_record.c:392-408`). The
    ///      seam's only channel is `Option<RecordDefinition>`, and the engine reads just
    ///      `is_exported` on a `false` answer — it builds the `{Module,Name}` operand into
    ///      the reason itself — so no field data from this answer ever reaches a term and
    ///      nothing is fabricated. (After step 2 the owner is loaded on every path that
    ///      reaches here, so this arm is never "the module could not be found".)
    pub(super) fn foreign_record_definition(
        &mut self,
        owner: Atom,
        name: Atom,
    ) -> Result<Option<RecordDefinition>> {
        // 1. The live shared namespace — no running-module snapshot exists here.
        let Some(owner_name) = self.runtime.atoms().name(owner).map(str::to_owned) else {
            return Ok(None);
        };
        let Some(record_name) = self.runtime.atoms().name(name).map(str::to_owned) else {
            return Ok(None);
        };
        // 2. `code:ensure_loaded/1`'s route: locate the file, then load it. Interning and
        //    the module cache both change, so this must precede the borrow in step 3.
        if !self.runtime.code_server.is_loaded(&owner_name) {
            let _ = self.runtime.find_module_file(&owner_name);
            if self.runtime.code_server.load(&owner_name).is_err() {
                return Ok(None);
            }
        }
        // 3. The lookup, shared with the `records:get_definition/2` bridge.
        match loaded_record(&self.runtime.code_server, &owner_name, &record_name) {
            Some((_module, definition)) => Ok(Some(definition.clone())),
            None => Ok(Some(RecordDefinition {
                name,
                is_exported: false,
                fields: Vec::new(),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use beam_bifs::NullSink;
    use beam_common::BeamError;
    use beam_engine::terms;
    use beam_engine::ExecutionContext;
    use beam_term::{atom_val, signed_value, Atom};
    use std::path::Path;

    use super::*;
    use crate::context::test_support::{scratch_dir, skip_loudly};
    use crate::runtime::Runtime;

    /// The committed native-record fixture pair (compiled by the system `erlc`, see
    /// `beam_loader/tests/fixtures/README.md`) — reused by relative path, never copied
    /// and never written to.
    const NR_DEF_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/nr_def.beam"
    );
    /// The **using** half: `def_mk/0` is `#recdef:rec{a = 1, b = 2}`, which the compiler
    /// lowers to a create-form `put_record` whose `Id` is the literal `{recdef, rec}` —
    /// the OTP-29 prototype's cross-module id. It defines (and does not export) its own
    /// `#local{a, b}` too.
    const NR_USE_BEAM: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/nr_use.beam"
    );
    /// The owner name the fixture's `{Module,Name}` literal carries — deliberately not
    /// the atom `nr_def` (whose image really defines and exports `rec`).
    const PLACEHOLDER_OWNER: &str = "recdef";

    /// A runtime with the `nr_def` image registered under the placeholder owner name the
    /// fixture's literal names, and `nr_use` registered under its own.
    ///
    /// Both images are loaded through **the code server** ([`CodeServer::load_bytes`]),
    /// never `beam_loader::load_file`: that interns their `AtU8` names into the one live
    /// shared table, so the definition's name/field atoms are ids the live table can
    /// spell (which is exactly what the engine's `live_atom` guard requires, and what a
    /// loader-fresh atom table would break).
    ///
    /// `exported` is the `rec` record's `is_exported` flag in the registered image; the
    /// test flips it to pin the C's `badrecord` branch.
    fn runtime_with_fixtures(exported: bool) -> (Runtime, beam_loader::Module) {
        let mut rt = Runtime::new(crate::config::RuntimeConfig::default()).expect("runtime");
        let def_bytes = std::fs::read(NR_DEF_BEAM).expect("the committed nr_def fixture reads");
        let mut def = rt.code_server.load_bytes(&def_bytes).expect("nr_def decodes");
        // `nr_def`'s only record is `rec`, exported via `-export_record([rec])`.
        for record in &mut def.records {
            if rt.code_server.atoms().name(record.name) == Some("rec") {
                record.is_exported = exported;
            }
        }
        rt.code_server.insert_loaded(
            PLACEHOLDER_OWNER,
            def,
            crate::codeserver::RecordedName::Str(format!("{PLACEHOLDER_OWNER}.beam")),
        );
        let use_bytes = std::fs::read(NR_USE_BEAM).expect("the committed nr_use fixture reads");
        let use_module = rt.code_server.load_bytes(&use_bytes).expect("nr_use decodes");
        rt.code_server.insert_loaded(
            "nr_use",
            use_module.clone(),
            crate::codeserver::RecordedName::Str("nr_use.beam".to_string()),
        );
        (rt, use_module)
    }

    /// The name of an atom term in the runtime's live shared table.
    fn atom_name(rt: &Runtime, term: beam_term::Eterm) -> Option<String> {
        rt.atoms().name(atom_val(term)).map(str::to_owned)
    }

    /// Runs one registered BIF against a value the run returned, so a test can inspect a
    /// native record through the **safe**, semantic surface (`records:*` /
    /// `erlang:is_record/1`) instead of the layout readers, which are `unsafe` and
    /// therefore unusable in this crate (`#![deny(unsafe_code)]`).
    fn call(
        rt: &mut Runtime,
        module: &str,
        function: &str,
        args: &[beam_term::Eterm],
    ) -> beam_common::Result<beam_term::Eterm> {
        let mut ctx = RuntimeContext::new(rt);
        ctx.call_bif(module, function, args.len() as u32, args)
    }

    /// Asserts the term is the `{recdef, rec}` instance `#rec{a = 1, b = 2}` — the shape
    /// the engine's create form must have produced from the **host's** definition.
    fn assert_def_record(rt: &mut Runtime, record: beam_term::Eterm) {
        let is_record = call(rt, "erlang", "is_record", &[record]).expect("is_record/1");
        assert_eq!(
            atom_name(rt, is_record).as_deref(),
            Some("true"),
            "the create form must produce a native record, not a plain tuple"
        );
        let module = call(rt, "records", "get_module", &[record]).expect("get_module/1");
        assert_eq!(
            atom_name(rt, module).as_deref(),
            Some(PLACEHOLDER_OWNER),
            "the instance carries the `Id`'s own Module atom as its owner"
        );
        let name = call(rt, "records", "get_name", &[record]).expect("get_name/1");
        assert_eq!(
            atom_name(rt, name).as_deref(),
            Some("rec"),
            "the definition's record name comes from the defining module's Recs table"
        );
        let exported = call(rt, "records", "is_exported", &[record]).expect("is_exported/1");
        assert_eq!(atom_name(rt, exported).as_deref(), Some("true"));
        // Declaration order (`nr_def` declares `#rec{a, b}`), each field holding the value
        // the create form's update list supplied.
        let a = rt.intern("a");
        let b = rt.intern("b");
        for (key, expected) in [(a, 1), (b, 2)] {
            let value = call(rt, "records", "get", &[key, record]).expect("records:get/2");
            assert_eq!(
                signed_value(value),
                expected,
                "field {key:?} holds the create form's value"
            );
        }
        let names = call(rt, "records", "get_field_names", &[record]).expect("get_field_names/1");
        assert_eq!(
            terms::cons_head(names).ok().and_then(|head| atom_name(rt, head)),
            Some("a".to_string()),
            "the field list starts with the first declared field"
        );
    }

    /// The record-id reason `{badrecord, Id}` a non-exported/absent foreign record raises
    /// (`erl_record.c:407-409`) — `Id` is the `{Module,Name}` operand tuple itself,
    /// **not** the `{Tag, {{Module,Name}, Field}}` shape the other record failures use.
    fn assert_badrecord_reason(rt: &mut Runtime, error: &BeamError) {
        let BeamError::Raise(raise) = error else {
            panic!("a non-exported foreign record must raise, got {error:?}");
        };
        assert_eq!(raise.class, beam_common::ExceptionClass::Error);
        // A compound reason is term-independent here: `ExitReason::Structured`, with the
        // live term in the raise's `live_reason` slot (the engine's uncaught funnel,
        // `Machine::throw_with_trace`). The `badrecord` atom is the reason tuple's tag.
        assert_eq!(raise.reason, beam_common::ExitReason::Structured);
        let reason = crate::error::live_reason_term(error)
            .expect("a `{badrecord, Id}` raise carries its live reason term");
        assert_eq!(terms::tuple_arity(reason).expect("the reason is a tuple"), 2);
        let tag = terms::tuple_element(reason, 0).expect("the tag");
        assert_eq!(atom_name(rt, tag).as_deref(), Some("badrecord"));
        let id = terms::tuple_element(reason, 1).expect("the Id operand");
        assert_eq!(
            terms::tuple_arity(id).expect("the Id is the `{Module,Name}` tuple"),
            2
        );
        assert_eq!(
            atom_name(rt, terms::tuple_element(id, 0).expect("Module")).as_deref(),
            Some(PLACEHOLDER_OWNER)
        );
        assert_eq!(
            atom_name(rt, terms::tuple_element(id, 1).expect("Name")).as_deref(),
            Some("rec")
        );
    }

    /// **The exported foreign record, end to end, on the single-pass route.** The host's
    /// seed (an exported `rec` under the placeholder owner) makes `nr_use:def_mk/0` build
    /// the instance instead of answering the pre-existing typed capability gap.
    #[test]
    fn an_exported_foreign_record_is_created_end_to_end() {
        let (mut rt, use_module) = runtime_with_fixtures(true);
        let mut out = NullSink;
        let record = rt
            .run_module(&use_module, "def_mk", 0, &[], &mut out)
            .expect("an exported foreign record's create form must succeed");
        assert_def_record(&mut rt, record);
    }

    /// **The same on the scheduled path.** `run_scheduled` runs the module as a real
    /// process on a `BudgetContext`, so without its forward this would answer the trait
    /// default and fail with the typed capability gap.
    #[test]
    fn an_exported_foreign_record_is_created_on_the_scheduled_path() {
        let (mut rt, _use_module) = runtime_with_fixtures(true);
        let dir = scratch_dir("foreign_record_scheduled");
        let path = dir.join("nr_use.beam");
        std::fs::copy(NR_USE_BEAM, &path).expect("the fixture copies");
        let record = rt
            .run_scheduled(&path, "def_mk", 0, &[])
            .expect("the scheduled path must reach the host's record table");
        assert_def_record(&mut rt, record);
    }

    /// **A non-exported foreign record** is the C's `error:{badrecord, Id}`: the host
    /// returns the definition *unfiltered* and the engine applies the export gate.
    #[test]
    fn a_non_exported_foreign_record_raises_badrecord_with_the_id_tuple() {
        let (mut rt, use_module) = runtime_with_fixtures(false);
        let mut out = NullSink;
        let error = rt
            .run_module(&use_module, "def_mk", 0, &[], &mut out)
            .expect_err("a non-exported foreign record may not be created");
        assert_badrecord_reason(&mut rt, &error);
    }

    /// **An owner the host cannot resolve keeps the pre-existing typed gap**: nothing is
    /// registered under the placeholder name, and `nr_def.beam` is on no code path, so the
    /// load half of the seam fails and the family answers its honest `unsupported`.
    #[test]
    fn an_unresolvable_owner_keeps_the_typed_unsupported() {
        let mut rt = Runtime::new(crate::config::RuntimeConfig::default()).expect("runtime");
        let use_bytes = std::fs::read(NR_USE_BEAM).expect("the committed nr_use fixture reads");
        let use_module = rt.code_server.load_bytes(&use_bytes).expect("nr_use decodes");
        let mut out = NullSink;
        let error = rt
            .run_module(&use_module, "def_mk", 0, &[], &mut out)
            .expect_err("no record definition table can answer for `recdef` here");
        let reason = crate::error::unsupported_reason(&error)
            .unwrap_or_else(|| panic!("the gap must stay a typed capability gap: {error:?}"));
        assert!(
            reason.contains("no record definition table for another module"),
            "the pre-existing gap's own text must survive: {reason}"
        );
    }

    /// **The hook's verbatim-flag contract**, asked directly: `nr_use` defines `local`
    /// with no `-export_record/1`, and the host must answer the definition anyway —
    /// `is_exported == false` is part of the answer, not a filter.
    #[test]
    fn the_hook_returns_a_non_exported_definition_unfiltered() {
        let (mut rt, _use_module) = runtime_with_fixtures(true);
        let owner = atom_val(rt.intern("nr_use"));
        let local = atom_val(rt.intern("local"));
        // The answer is an owned clone, so the context borrow ends here and the atom
        // names below read the live table again.
        let definition = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.foreign_record_definition(owner, local)
                .expect("the hook reports, never errors")
                .expect("`nr_use`'s own non-exported `local` must be answered")
        };
        assert!(
            !definition.is_exported,
            "the export flag is returned unfiltered — the engine decides the badrecord branch"
        );
        assert_eq!(definition.name, local);
        assert_eq!(
            definition.fields.len(),
            2,
            "`#local{{a, b}}` has two declared fields"
        );
        let names: Vec<Option<&str>> = definition
            .fields
            .iter()
            .map(|field| rt.atoms().name(field.name))
            .collect();
        assert_eq!(names, vec![Some("a"), Some("b")]);
        assert!(
            definition.fields.iter().all(|field| field.default.is_none()),
            "neither field of `#local{{a, b}}` declares a default"
        );
    }

    /// **An unknown owner / unknown record is `Ok(None)`** — never a guessed definition,
    /// even when the owner *is* loaded (a loaded module with no such record is the C's
    /// `badrecord`, which the engine reads off the `is_exported == false` answer).
    #[test]
    fn the_hook_never_guesses_a_definition() {
        let (mut rt, _use_module) = runtime_with_fixtures(true);
        // An owner no module on any code path names.
        let unknown_owner = atom_val(rt.intern("no_such_owner_mod"));
        let anything = atom_val(rt.intern("anything"));
        let missing = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.foreign_record_definition(unknown_owner, anything)
                .expect("the hook reports, never errors")
        };
        assert!(
            missing.is_none(),
            "an owner that is on no code path is the host's honest `Ok(None)`"
        );

        // a **loaded** owner with no such record: the C's table miss, which the seam
        // expresses as a non-exported empty answer (the engine raises `badrecord`).
        let loaded_owner = atom_val(rt.intern(PLACEHOLDER_OWNER));
        let absent = atom_val(rt.intern("no_such_record"));
        let definition = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.foreign_record_definition(loaded_owner, absent)
                .expect("the hook reports, never errors")
                .expect("a loaded owner with no such record answers the table miss")
        };
        assert!(!definition.is_exported);
        assert!(definition.fields.is_empty());
        assert_eq!(definition.name, absent);
    }

    /// **Load-then-retry**: an owner that is on the code path but not loaded is loaded by
    /// the seam and the lookup then succeeds — the C's `erl_create_native_record_jit`
    /// trap (`erl_record.c:412-565`), whose load half the engine does not have.
    ///
    /// `bootstrap/lib/compiler/ebin/beam_types.beam` is a real OTP-30 module that defines
    /// and exports native records (`-export_record([t_atom, …])`), so this pins the trap
    /// against a genuine image rather than a fixture. The suite skips **loudly** when the
    /// committed bootstrap tree is not next to this crate.
    #[test]
    fn a_code_path_owner_is_loaded_and_the_lookup_retried() {
        let lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../bootstrap/lib/compiler/ebin");
        if !lib.join("beam_types.beam").is_file() {
            skip_loudly(
                "foreign record definition",
                "no bootstrap/lib/compiler/ebin/beam_types.beam beside the crate",
            );
            return;
        }
        let mut rt = Runtime::new(crate::config::RuntimeConfig::default()).expect("runtime");
        rt.add_code_path(lib);
        assert!(
            !rt.code_server.is_loaded("beam_types"),
            "the precondition: the owner is on the code path but not loaded"
        );
        let owner = atom_val(rt.intern("beam_types"));
        let name = atom_val(rt.intern("t_atom"));
        let definition = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.foreign_record_definition(owner, name)
                .expect("the hook reports, never errors")
                .expect("the seam must load the owner and retry the lookup")
        };
        assert!(
            definition.is_exported,
            "`beam_types` exports `t_atom` via -export_record/1"
        );
        assert_eq!(definition.name, name);
        assert!(
            !definition.fields.is_empty(),
            "`#t_atom{{elements = any}}` declares a field"
        );
        let fields: Vec<Option<&str>> = definition
            .fields
            .iter()
            .map(|field| rt.atoms().name(field.name))
            .collect();
        assert_eq!(fields, vec![Some("elements")]);
        assert!(
            rt.code_server.is_loaded("beam_types"),
            "the seam's load half really loaded the owner"
        );
    }

    /// **`BudgetContext` forwards the hook** — pinned in `crate::runner::tests`
    /// (`the_slice_wrapper_forwards_the_foreign_record_definition_seam`), where the wrapper's
    /// `pub(super)` type is reachable; the scheduled-path case above is the end-to-end proof.

    /// The fixture pair is what every case above rests on: `nr_use`'s `{recdef, rec}`
    /// literal resolves to an owner name no image in the tree carries, and `nr_def`'s
    /// `Recs` table holds exactly the one exported `rec` — if either fixture is
    /// regenerated differently, this fails first and says so.
    #[test]
    fn the_fixture_pair_is_the_shape_every_case_assumes() {
        let mut rt = Runtime::new(crate::config::RuntimeConfig::default()).expect("runtime");
        let def_bytes = std::fs::read(NR_DEF_BEAM).expect("the committed nr_def fixture reads");
        let def = rt.code_server.load_bytes(&def_bytes).expect("nr_def decodes");
        let rec_name = def.record_by_name("rec").expect("`rec` is declared");
        assert!(rec_name.is_exported, "`-export_record([rec])` is exported");
        assert_eq!(
            rec_name
                .fields
                .iter()
                .filter_map(|field| rt.atoms().name(field.name))
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "`#rec{{a, b}}` in declaration order"
        );
        let use_bytes = std::fs::read(NR_USE_BEAM).expect("the committed nr_use fixture reads");
        let use_module = rt.code_server.load_bytes(&use_bytes).expect("nr_use decodes");
        assert!(
            use_module.record_by_name("rec").is_none(),
            "`nr_use` declares no `rec` of its own — the id is foreign"
        );
        let local = use_module
            .record_by_name("local")
            .expect("`nr_use` declares its own `local`");
        assert!(
            !local.is_exported,
            "`#local{{a, b}}` has no -export_record/1 — the hook's unfiltered-flag case"
        );
        assert!(
            use_module
                .records
                .iter()
                .all(|record| rt.atoms().name(record.name) != Some(PLACEHOLDER_OWNER)),
            "`nr_use`'s own Recs table must carry no `recdef` entry — the id is foreign"
        );
    }

    /// `FixtureDir` is intentionally kept alive by the scheduled case only; this proves the
    /// scratch helper really yields a directory this suite can copy into (so a regression in
    /// the shared harness is not silently absorbed).
    #[test]
    fn the_scratch_dir_is_usable_for_a_scheduled_fixture_copy() {
        let dir = scratch_dir("foreign_record_scratch");
        let path = dir.join("nr_use.beam");
        std::fs::copy(NR_USE_BEAM, &path).expect("the fixture copies");
        assert!(path.is_file());
    }

    /// The `Atom` type is what the seam's operands are; this pins that an atom the live
    /// table holds round-trips through the hook's first step (name lookup) and that an
    /// index beyond it does not (the `Ok(None)` arm).
    #[test]
    fn an_atom_the_live_table_cannot_spell_is_an_honest_none() {
        let (mut rt, _use_module) = runtime_with_fixtures(true);
        let absent = Atom::from_raw(4096);
        let rec = atom_val(rt.intern("rec"));
        let mut ctx = RuntimeContext::new(&mut rt);
        assert!(
            ctx.foreign_record_definition(absent, rec)
                .expect("the hook reports, never errors")
                .is_none(),
            "an owner atom the live table cannot name is never answered for"
        );
        assert!(
            ctx.foreign_record_definition(rec, absent)
                .expect("the hook reports, never errors")
                .is_none(),
            "a record-name atom the live table cannot name is never answered for"
        );
    }
}
