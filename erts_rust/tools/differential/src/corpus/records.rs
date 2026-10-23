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

//! Contracts **A.1 / A.2 / A.7** — native records (the OTP-29 `-record #r{}` form).
//!
//! OTP-29 added **native records** (`-record #r{}`) as a first-class runtime value next
//! to the classic "tagged tuple" records. Two surfaces reach a black-box CLI:
//!
//! * the **native-record opcode family** (generic opcodes 186-193 in
//!   `lib/compiler/src/genop.tab`), lowered by the Rust loader to typed IR and executed by
//!   the engine's `families/records` — exercised end to end by the [`CaseKind::Module`]
//!   cases below, whose embedded module is compiled with the system `erlc` and run under
//!   **both** runtimes;
//! * the `records:*` **BIFs** (`records:create/4`, `records:update/4`,
//!   `records:get_definition/2`, `records:is_exported/1`, plus the
//!   `get`/`get_module`/`get_name`/`get_field_names` introspection family), exercised by
//!   the [`CaseKind::Eval`] cases.
//!
//! A native record compares as its **canonical ETF bytes** (`term_to_binary/1`, contract
//! A.2 — the default [`Probe::TermToBinary`]), so the record-valued cases pin the whole
//! encoding (field count, the exported flag, the definition's module/name and the values
//! in ascending-key order). The scalar/list introspection results use [`Probe::Value`],
//! which is the other rendering a runtime must reproduce exactly.
//!
//! # The module case: a record built at runtime, then read through record *syntax*
//! Most of the compiled module's records come from the `records:create/4` **BIF** (with the
//! same module/name the compile-time `-record #rec{}` declares), and ordinary record
//! *syntax* is then used on them, so the access/update/matching opcodes really do execute end
//! to end. `create_local/0` is the other half: a compile-time `#rec{a = 1, b = 2}` literal,
//! which the compiler lowers to `put_record`'s **create** form (`Src` is the literal `nil`)
//! with the running module's own record identity.
//!
//! # `put_record`'s create form, and the *foreign* record id
//!
//! The create form's `Id` operand has two shapes (`emu/ops.tab:1648-1651`), and the group pins
//! both:
//!
//! * an **atom** — one of the *running* module's own records (`i_create_local_native_record`,
//!   no export requirement, `erl_record.c:254-353`) — `create_local/0`;
//! * a **`{Module,Name}`** pair — `i_create_native_record`, which resolves the definition
//!   through the code server's record table (`erts_record_find_entry`, `erl_record.c:376`)
//!   and **requires** `is_exported` (`:401`), else raises `error:{badrecord, Id}` (`:407`) —
//!   the `nr_foreign` cases below.
//!
//! The second shape is the one whose definition the engine cannot read out of the running
//! module's own `Recs` chunk: it asks the host through the defaulted
//! `beam_engine::ExecutionContext::foreign_record_definition` seam, which `beam_rt` answers
//! from its code server's retained modules. `nr_foreign` writes `#recdef:rec{a = 1, b = 2}` —
//! an explicit `#Module:Name{}` literal, so the system `erlc` emits the `{recdef, rec}`
//! **pair** even though the field list comes from the module's own local `-record #rec{a, b}`
//! (verified with `erlc -S`) — and is run as a [`CaseKind::ModuleEnv`] case whose `recdef`/
//! `recbad` helper modules live in the compiled directory, reachable only through the case
//! environment (`ERL_AFLAGS=-pa {dir}`).
//!
//! Two measured facts shape that fixture:
//!
//! * **the oracle's create form does not load a not-yet-loaded owner.** Measured on the
//!   installed OTP-29 (`erl -noshell -pa <dir> -eval 'nr_foreign:mk_field()'`, nothing else
//!   loaded) it raises `error:{badrecord,{recdef,rec}}`; loading the owner first
//!   (`code:ensure_loaded(recdef)`) makes the same call answer `1`. The module therefore
//!   loads it explicitly, which is what makes the case about the **definition lookup for an
//!   already-loaded foreign module** — precisely the host seam — rather than about a
//!   code-server load trap.
//! * **the export flag is part of the answer, not a filter.** `recbad` declares the same
//!   record **without** `-export_record([rec])`, and `mk_not_exported/0` pins the C's
//!   `error:{badrecord,{recbad,rec}}` for it: a host that answered `Ok(None)` there would turn
//!   that raise into a capability gap, and one that filtered the definition away would too.
//!
//! # Which native-record opcodes the embedded module executes
//! The mapping below is read off the system `erlc`'s own disassembly (`erlc -S`), the only
//! honest evidence for opcode reachability:
//!
//! | function | native-record opcodes executed |
//! |---|---|
//! | `fld_a/0`, `fld_b/0`, `total/0` | `get_record_field` (191) |
//! | `guard/0` | `is_any_native_record` (186) |
//! | `upd_a/0`, `upd_total/0` | `is_any_native_record` (186), `is_native_record` (187), `get_record_elements` (188), `put_record` update (189) |
//! | `match_scale/0` | `is_any_native_record` (186), `is_native_record` (187), `get_record_elements` (188) |
//! | `ext_fld/0` | `get_record_field` (191), keyed by an external `{nr_ext,ext}` record id |
//! | `ext_acc/0` | `is_any_native_record` (186), `is_native_record` (187), `is_record_accessible` (190), `get_record_elements` (188) |
//! | `ext_upd/0` | `is_any_native_record` (186), `is_native_record` (187), `is_record_accessible` (190), `put_record` (189), `get_record_field` (191) |
//! | `def_fields/0` | no record opcode — reaches the `records:get_definition/2` BIF |
//!
//! Six of the eight family members (**186-191**) are therefore genuinely executed.
//! `is_record_accessible` (190) is emitted for an **external** record, which the module
//! reaches with `-import_record(nr_ext, [ext])` plus `#nr_ext:ext{}` syntax over a value
//! built by `records:create(nr_ext, ext, …)` — no second compiled module is needed.
//! Generic opcodes **192/193** (`get_record_elements_id`, `update_record_id`) are OTP-30
//! instructions and stay unreachable: the installed OTP-29 `erlc` (`compiler-10.0.6`) has
//! neither opcode anywhere in its own source, so nothing it compiles can emit them (their
//! lowering is covered by the loader/engine unit tests).
//!
//! The system `erlc` needs **no extra flag** for the native-record syntax or for
//! `-import_record` (verified against `lib/compiler/test/native_record_SUITE.erl` and the
//! plain `erlc -o …` used by `crate::module_build`), so this group adds no compiler option.
//!
//! `records:get_definition/2` is reached from `def_fields/0` inside the compiled module.
//! The BIF only consults the code server's native-record table, which is populated when a
//! module declaring `-record #rec{}` is **loaded**; an eval-side (`-eval`) positive case is
//! therefore not constructible, because no module in the OTP-29 installation declares a
//! native record (every `.beam` under the install has an empty `Recs` chunk). The lookup is
//! reachable only from inside a loaded module.
//!
//! # Honest coverage (cases deliberately *not* in this group)
//!
//! One observed divergence is **not** pinned here (it is a `mismatch`, a real behavioral
//! difference to hand off, not a capability gap): a `{badfield, _}` reason at the wrong
//! *shape*. `records:update(R, m, r, #{z => 1})` gives the oracle
//! `error:{badfield,{{m,r},z}}` but the Rust runtime `error:{badfield,{m,r},z}` — the nested
//! `{Module,Name}` tuple is flattened into the outer tuple — so any case would be a
//! guaranteed mismatch.
//!
//! The other structured reasons round-trip through the probe's `try … catch` with the
//! oracle's exact shape and **do** agree, and are covered below: `{badrecord, {x,y}}` from
//! `records:is_exported({x,y})` and `{badmap, badopts}` from a non-map options argument.
//! The plain-atom `badarg` reasons (a duplicate field key, a malformed option map, an
//! absent field, a non-record) and the `records:is_exported/1` boolean **do** agree too and
//! are covered below.

use super::{Case, CaseKind, Contract, EmbeddedModule, Golden, ModuleRun, Probe};

/// The embedded module the native-record [`CaseKind::Module`] cases compile and run.
///
/// Every exported function returns an **integer** (the shape a module-run golden pins).
/// `mk/0` builds the local record through the `records:create/4` BIF (see the module docs
/// for why it is not a compile-time `#rec{}` literal) and `ext_mk/0` builds an *external*
/// one; the functions then use native-record *syntax* — field access, a field update, a
/// `#rec{}` match and an external `#nr_ext:ext{}` match/update — so the native-record
/// opcodes execute under both runtimes.
pub(crate) const NATIVE_RECORD_SOURCE: &str = r##"-module(nr_native).
-export([fld_a/0, fld_b/0, total/0, upd_a/0, upd_total/0, guard/0, match_scale/0,
         ext_fld/0, ext_acc/0, ext_upd/0, def_fields/0, create_local/0]).

-import_record(nr_ext, [ext]).
-record #rec{a, b}.

mk() ->
    records:create(nr_native, rec, [{a,1},{b,2}], #{is_exported=>true}).

ext_mk() ->
    records:create(nr_ext, ext, [{a,7},{b,8}], #{is_exported=>true}).

fld_a() -> R = mk(), R#rec.a.
fld_b() -> R = mk(), R#rec.b.
total() -> R = mk(), R#rec.a + R#rec.b.
upd_a() -> R = mk(), R2 = R#rec{a = 9}, R2#rec.a.
upd_total() -> R = mk(), R2 = R#rec{a = 9, b = 8}, R2#rec.a + R2#rec.b.
guard() -> R = mk(), case is_record(R) of true -> 1; false -> 0 end.
match_scale() -> R = mk(), #rec{a = A, b = B} = R, A * 10 + B.
ext_fld() -> R = ext_mk(), R#nr_ext:ext.a.
ext_acc() -> R = ext_mk(), #nr_ext:ext{a = A, b = B} = R, A * 10 + B.
ext_upd() -> R = ext_mk(), R2 = R#nr_ext:ext{a = 5}, R2#nr_ext:ext.a.
def_fields() -> {_, Fs} = records:get_definition(nr_native, rec), length(Fs).
create_local() -> R = #rec{a = 1, b = 2}, R#rec.a + R#rec.b.
"##;

/// The [`CaseKind::ModuleEnv`] module behind the **foreign** create-form cases: every record
/// it reads is built by a `#Module:Name{}` create-form `put_record` whose definition lives in
/// *another* module's `Recs` chunk (see the module docs).
///
/// `#recdef:rec{a = 1, b = 2}` names `recdef` explicitly, so the compiler emits the
/// `{recdef, rec}` identity pair; the local `-record #rec{a, b}` only supplies the field list.
/// The owner is loaded with `code:ensure_loaded/1` first, because the oracle's create form
/// does not load an absent owner (a measured fact — see the module docs).
pub(crate) const FOREIGN_RECORD_SOURCE: &str = r##"-module(nr_foreign).
-export([mk_field/0, mk_total/0, mk_definition/0, mk_not_exported/0]).

-record #rec{a, b}.

mk() ->
    {module, recdef} = code:ensure_loaded(recdef),
    #recdef:rec{a = 1, b = 2}.

mk_field() -> R = mk(), R#recdef:rec.a.
mk_total() -> R = mk(), R#recdef:rec.a + R#recdef:rec.b.

mk_definition() ->
    R = mk(),
    case {records:get_module(R), records:get_name(R), records:get_field_names(R)} of
        {recdef, rec, [a, b]} -> 1;
        _ -> 0
    end.

mk_not_exported() ->
    {module, recbad} = code:ensure_loaded(recbad),
    try
        _Created = #recbad:rec{a = 1, b = 2},
        0
    catch
        error:{badrecord, Id} when Id =:= {recbad, rec} -> 1;
        error:_ -> 2
    end.
"##;

/// The **owner** the foreign create form's `{recdef, rec}` id names: it declares `rec` and
/// exports it with `-export_record([rec])`, so the create form may build an instance of it.
pub(crate) const RECORD_DEFINER_SOURCE: &str = r##"-module(recdef).
-export([marker/0]).
-export_record([rec]).
-record #rec{a, b}.
marker() -> #rec{a = 0, b = 0}.
"##;

/// The **non-exported** owner: the same record, declared but never exported, so the create
/// form must raise `error:{badrecord, {recbad, rec}}` instead of building anything.
pub(crate) const RECORD_UNEXPORTED_SOURCE: &str = r##"-module(recbad).
-export([marker/0]).
-record #rec{a, b}.
marker() -> #rec{a = 0, b = 0}.
"##;
/// Build one eval case.
///
/// `exit_code` is `0` for a value line and `1` for an `exception …` line.
fn eval(
    id: &'static str,
    description: &'static str,
    contracts: &'static [Contract],
    probe: Probe,
    expr: &'static str,
    golden: &'static str,
    exit_code: i32,
) -> Case {
    Case {
        id,
        description,
        contracts,
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code,
        },
    }
}

/// Build one module-run case over [`NATIVE_RECORD_SOURCE`] (a 0-arity integer function).
fn module_case(
    id: &'static str,
    description: &'static str,
    function: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[
            Contract::BeamAsm,
            Contract::LanguageSemantics,
            Contract::OtpApps,
        ],
        kind: CaseKind::Module(ModuleRun {
            source: NATIVE_RECORD_SOURCE,
            module: "nr_native",
            function,
            arity: 0,
            args: &[],
            nif: None,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// Build one [`CaseKind::ModuleEnv`] case over [`FOREIGN_RECORD_SOURCE`] (a 0-arity integer
/// function whose record comes from `recdef`/`recbad`, a *foreign* module's `Recs` chunk).
///
/// The two owner modules are compiled into the case's own temp dir and reach the code path
/// **only** through `ERL_AFLAGS=-pa {dir}` — the oracle argv carries no `-pa` — so the
/// environment is what makes the foreign definition resolvable at all (see the module docs).
fn foreign_module_case(
    id: &'static str,
    description: &'static str,
    function: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[
            Contract::BeamAsm,
            Contract::LanguageSemantics,
            Contract::OtpApps,
        ],
        kind: CaseKind::ModuleEnv {
            run: ModuleRun {
                source: FOREIGN_RECORD_SOURCE,
                module: "nr_foreign",
                function,
                arity: 0,
                args: &[],
                nif: None,
            },
            extra: &[
                EmbeddedModule {
                    module: "recdef",
                    source: RECORD_DEFINER_SOURCE,
                },
                EmbeddedModule {
                    module: "recbad",
                    source: RECORD_UNEXPORTED_SOURCE,
                },
            ],
            env: &[("ERL_AFLAGS", "-pa {dir}")],
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}
/// The native-record corpus group (contracts A.1/A.2/A.7).
pub(crate) fn group() -> Vec<Case> {
    // A native record compares as canonical ETF bytes (A.2); the producing/consuming BIFs
    // are the OTP-app surface (A.7), and the plain-`badarg` failures are language
    // exception semantics (A.1).
    let record_bif = &[Contract::BeamAsm, Contract::OtpApps];
    let introspection = &[Contract::OtpApps];
    let error_semantics = &[Contract::LanguageSemantics, Contract::OtpApps];
    let t2b = Probe::TermToBinary;
    let val = Probe::Value;

    vec![
        // --- A.2/A.7: records:create/4 and the canonical record encoding ------------------
        eval(
            "records/create_single_field",
            "records:create/4 encodes a one-field native record (the exported flag set)",
            record_bif,
            t2b,
            "records:create(m,r,[{a,1}],#{is_exported=>true})",
            "ok <<131,67,0,0,0,1,1,119,1,109,119,1,114,119,1,97,97,1>>",
            0,
        ),
        eval(
            "records/create_no_fields",
            "records:create/4 encodes a field-less native record",
            record_bif,
            t2b,
            "records:create(x,empty,[],#{is_exported=>false})",
            "ok <<131,67,0,0,0,0,0,119,1,120,119,5,101,109,112,116,121>>",
            0,
        ),
        eval(
            "records/create_field_order_is_normalised",
            "records:create/4 stores the fields in ascending-key order regardless of input order",
            record_bif,
            t2b,
            "records:create(m,r,[{b,2},{a,1}],#{is_exported=>true})",
            "ok <<131,67,0,0,0,2,1,119,1,109,119,1,114,119,1,98,119,1,97,97,2,97,1>>",
            0,
        ),
        eval(
            "records/create_compound_values",
            "records:create/4 encodes list and tuple field values",
            record_bif,
            t2b,
            "records:create(m, r, [{a, [1,2]}, {b, {3,4}}], #{is_exported=>true})",
            "ok <<131,67,0,0,0,2,1,119,1,109,119,1,114,119,1,97,119,1,98,107,0,2,1,2,104,2,97,3,97,4>>",
            0,
        ),
        eval(
            "records/create_duplicate_key_is_badarg",
            "records:create/4 raises badarg for a duplicated field key",
            error_semantics,
            t2b,
            "records:create(m, r, [{a,1},{a,2}], #{is_exported=>true})",
            "exception error badarg",
            1,
        ),
        eval(
            "records/create_bad_option_is_badarg",
            "records:create/4 raises badarg when the option map has no valid is_exported value",
            error_semantics,
            t2b,
            "records:create(m, r, [{a,1}], #{is_exported=>yes})",
            "exception error badarg",
            1,
        ),
        // --- A.2/A.7: records:update/4 ----------------------------------------------------
        eval(
            "records/update_single_field",
            "records:update/4 replaces one field and keeps the other",
            record_bif,
            t2b,
            "records:update(records:create(m,r,[{a,1},{b,2}],#{is_exported=>true}), m, r, #{a=>9})",
            "ok <<131,67,0,0,0,2,1,119,1,109,119,1,114,119,1,97,119,1,98,97,9,97,2>>",
            0,
        ),
        eval(
            "records/update_two_fields",
            "records:update/4 replaces two fields of a three-field record",
            record_bif,
            t2b,
            "records:update(records:create(m,r,[{a,1},{b,2},{c,3}],#{is_exported=>false}), m, r, #{a=>9, c=>7})",
            "ok <<131,67,0,0,0,3,0,119,1,109,119,1,114,119,1,97,119,1,98,119,1,99,97,9,97,2,97,7>>",
            0,
        ),
        eval(
            "records/update_wildcard_module_name",
            "records:update/4 accepts the '_' wildcard for the module and name",
            record_bif,
            t2b,
            "records:update(records:create(m,r,[{a,1},{b,2}],#{is_exported=>true}), '_', '_', #{b=>7})",
            "ok <<131,67,0,0,0,2,1,119,1,109,119,1,114,119,1,97,119,1,98,97,1,97,7>>",
            0,
        ),
        eval(
            "records/update_empty_map_is_identity",
            "records:update/4 returns the record unchanged for an empty update map",
            record_bif,
            t2b,
            "records:update(records:create(m,r,[{a,1},{b,2}],#{is_exported=>true}), m, r, #{})",
            "ok <<131,67,0,0,0,2,1,119,1,109,119,1,114,119,1,97,119,1,98,97,1,97,2>>",
            0,
        ),
        // --- A.7: the records:* introspection surface -------------------------------------
        eval(
            "records/get_name",
            "records:get_name/1 returns the record definition's name atom",
            introspection,
            val,
            "records:get_name(records:create(m,r,[{a,1}],#{is_exported=>true}))",
            "ok r",
            0,
        ),
        eval(
            "records/get_module",
            "records:get_module/1 returns the record definition's module atom",
            introspection,
            val,
            "records:get_module(records:create(m,r,[{a,1}],#{is_exported=>true}))",
            "ok m",
            0,
        ),
        eval(
            "records/get_field_names",
            "records:get_field_names/1 returns the field names in declaration order",
            introspection,
            val,
            "records:get_field_names(records:create(m,r,[{a,1},{b,2},{c,3}],#{is_exported=>false}))",
            "ok [a,b,c]",
            0,
        ),
        eval(
            "records/get_field_names_empty",
            "records:get_field_names/1 of a field-less record is the empty list",
            introspection,
            val,
            "records:get_field_names(records:create(x,empty,[],#{is_exported=>false}))",
            "ok []",
            0,
        ),
        eval(
            "records/get_field",
            "records:get/2 reads a field value from a native record",
            introspection,
            val,
            "records:get(a, records:create(m,r,[{a,1},{b,2}],#{is_exported=>false}))",
            "ok 1",
            0,
        ),
        eval(
            "records/get_compound_field",
            "records:get/2 returns a compound (list) field value",
            introspection,
            val,
            "records:get(a, records:create(m, r, [{a, [1,2]}, {b, {3,4}}], #{is_exported=>true}))",
            "ok [1,2]",
            0,
        ),
        eval(
            "records/is_exported_true",
            "records:is_exported/1 reports an exported record as the atom true",
            introspection,
            val,
            "records:is_exported(records:create(m,r,[{a,1}],#{is_exported=>true}))",
            "ok true",
            0,
        ),
        eval(
            "records/is_exported_false",
            "records:is_exported/1 reports an unexported record as the atom false",
            introspection,
            val,
            "records:is_exported(records:create(m,r,[{a,1}],#{is_exported=>false}))",
            "ok false",
            0,
        ),
        eval(
            "records/is_exported_non_record_is_badrecord",
            "records:is_exported/1 raises the structured {badrecord,Term} for a non-record",
            error_semantics,
            val,
            "records:is_exported({x,y})",
            "exception error {badrecord,{x,y}}",
            1,
        ),
        eval(
            "records/create_bad_options_is_badmap",
            "records:create/4 raises the structured {badmap,Term} for a non-map options argument",
            error_semantics,
            val,
            "records:create(m, r, [], badopts)",
            "exception error {badmap,badopts}",
            1,
        ),
        eval(
            "records/get_updated_field",
            "records:get/2 observes the value records:update/4 wrote",
            introspection,
            val,
            "records:get(a, records:update(records:create(m,r,[{a,1},{b,2}],#{is_exported=>true}), m, r, #{a=>9}))",
            "ok 9",
            0,
        ),
        eval(
            "records/get_absent_field_is_badarg",
            "records:get/2 raises badarg for a key naming no field",
            error_semantics,
            val,
            "records:get(zz, records:create(m,r,[{a,1}],#{is_exported=>true}))",
            "exception error badarg",
            1,
        ),
        eval(
            "records/get_name_non_record_is_badarg",
            "records:get_name/1 raises badarg for a non-record argument",
            error_semantics,
            val,
            "records:get_name(5)",
            "exception error badarg",
            1,
        ),
        // --- A.7: records:get_definition/2 -------------------------------------------------
        eval(
            "records/get_definition_non_atom_module_is_badarg",
            "records:get_definition/2 raises badarg for a non-atom module",
            introspection,
            val,
            "records:get_definition(1, 2)",
            "exception error badarg",
            1,
        ),
        eval(
            "records/get_definition_non_atom_name_is_badarg",
            "records:get_definition/2 raises badarg for a non-atom name",
            introspection,
            val,
            "records:get_definition(m, 2)",
            "exception error badarg",
            1,
        ),
        // `records:get_definition/2` is served by the runtime's `RecordSource` bridge (the
        // loaded module's `Recs`-chunk definitions reached through the code server), so a
        // record no loaded module defines reproduces the oracle's plain `error:badarg`. The
        // golden is the oracle's own answer.
        eval(
            "records/get_definition_unknown_record",
            "records:get_definition/2 of a record no loaded module defines",
            introspection,
            val,
            "records:get_definition(m, r)",
            "exception error badarg",
            1,
        ),
        eval(
            "records/get_definition_unloaded_module",
            "records:get_definition/2 raises badarg for an atom naming a module the node has not loaded",
            introspection,
            val,
            "records:get_definition(no_such_module_is_loaded, r)",
            "exception error badarg",
            1,
        ),
        eval(
            "records/get_definition_loaded_module_unknown_record",
            "records:get_definition/2 raises badarg for a loaded module (lists) that declares no such native record",
            introspection,
            val,
            "records:get_definition(lists, no_such_record_declared)",
            "exception error badarg",
            1,
        ),
        // --- A.2/A.1/A.7: the native-record opcodes, executed by a compiled module --------
        module_case(
            "records/module_native_record_get_field",
            "get_record_field reads the first field of a native record under both runtimes",
            "fld_a",
            "1",
        ),
        module_case(
            "records/module_native_record_get_second_field",
            "get_record_field reads the second field of a native record under both runtimes",
            "fld_b",
            "2",
        ),
        module_case(
            "records/module_native_record_field_sum",
            "two get_record_field reads of a native record sum under both runtimes",
            "total",
            "3",
        ),
        module_case(
            "records/module_native_record_update",
            "a one-field record update (put_record) agrees under both runtimes",
            "upd_a",
            "9",
        ),
        module_case(
            "records/module_native_record_update_two_fields",
            "a two-field record update (put_record + get_record_elements) agrees under both runtimes",
            "upd_total",
            "17",
        ),
        module_case(
            "records/module_native_record_guard",
            "the is_record/1 native-record guard (is_any_native_record) agrees under both runtimes",
            "guard",
            "1",
        ),
        module_case(
            "records/module_native_record_match",
            "a #rec{} match (get_record_elements) destructures a native record under both runtimes",
            "match_scale",
            "12",
        ),
        module_case(
            "records/module_native_record_external_get_field",
            "get_record_field reads a field of an external record (an {nr_ext,ext} record id) under both runtimes",
            "ext_fld",
            "7",
        ),
        module_case(
            "records/module_native_record_external_accessible_match",
            "an #nr_ext:ext{} match of an external record (is_record_accessible + get_record_elements) agrees under both runtimes",
            "ext_acc",
            "78",
        ),
        module_case(
            "records/module_native_record_external_update",
            "an external record update (is_record_accessible + put_record + get_record_field) agrees under both runtimes",
            "ext_upd",
            "5",
        ),
        // `records:get_definition/2` consults the running module's own record definitions
        // (for `nr_native`, the `-record #rec{}` it declares) through the runtime's
        // `RecordSource` bridge; the BIF returns the `{Name, FieldNames}` pair, so
        // `def_fields/0` reports the field count. The golden is the oracle's own answer.
        module_case(
            "records/module_get_definition_declared_record",
            "records:get_definition/2 of a record the running module declares",
            "def_fields",
            "2",
        ),
        // `put_record`'s create form with the running module's **own** atom identity (no export
        // requirement): `#rec{a = 1, b = 2}` is a compile-time literal, so the opcode is
        // `i_create_local_native_record`. The golden sums both fields (1 + 2).
        module_case(
            "records/module_native_record_create_local",
            "a compile-time #rec{a = 1, b = 2} literal (the create form, local atom identity) agrees",
            "create_local",
            "3",
        ),
        // --- A.1/A.7: the create form for a *foreign* `{Module,Name}` record id -------------
        // Each case needs `recdef` (or `recbad`) in the code path, so all four are
        // `ModuleEnv` cases over the same compiled module (see `foreign_module_case`).
        foreign_module_case(
            "records/module_foreign_record_create_field",
            "a create-form record id {recdef,rec} resolves a foreign module's definition, then get_record_field reads it",
            "mk_field",
            "1",
        ),
        foreign_module_case(
            "records/module_foreign_record_create_total",
            "both fields of the create-form foreign record sum (get_record_field twice)",
            "mk_total",
            "3",
        ),
        foreign_module_case(
            "records/module_foreign_record_create_definition",
            "the create-form foreign record carries the resolved definition: module, name and declaration-order fields",
            "mk_definition",
            "1",
        ),
        foreign_module_case(
            "records/module_foreign_record_create_not_exported",
            "the create form raises {badrecord,{recbad,rec}} for a loaded owner whose record is not exported",
            "mk_not_exported",
            "1",
        ),        // --- widened: the create/update encoding and the structured refusal reasons --------
        eval(
            "records/create_three_fields",
            "records:create/4 encodes a three-field native record in ascending-key order",
            record_bif,
            t2b,
            "records:create(m,r,[{a,1},{b,2},{c,3}],#{is_exported=>true})",
            "ok <<131,67,0,0,0,3,1,119,1,109,119,1,114,119,1,97,119,1,98,119,1,99,97,1,97,2,97,3>>",
            0,
        ),
        eval(
            "records/get_field_names_declaration_order",
            "records:get_field_names/1 returns the field names in declaration order, not sorted",
            introspection,
            val,
            "records:get_field_names(records:create(m,r,[{z,1},{a,2}],#{is_exported=>false}))",
            "ok [z,a]",
            0,
        ),
        eval(
            "records/get_module_of_a_named_module",
            "records:get_module/1 reports the module the record definition was created for",
            introspection,
            val,
            "records:get_module(records:create(records_mod,r,[],#{is_exported=>false}))",
            "ok records_mod",
            0,
        ),
        eval(
            "records/update_unknown_field_is_badfield",
            "records:update/4 raises the structured {badfield,{{Module,Name},Key}} for an unknown key",
            error_semantics,
            val,
            "records:update(records:create(m,r,[{a,1},{b,2}],#{is_exported=>true}), m, r, #{zz=>1})",
            "exception error {badfield,{{m,r},zz}}",
            1,
        ),
        eval(
            "records/update_wrong_module_is_badrecord",
            "records:update/4 raises {badrecord,Name} when the module does not match",
            error_semantics,
            val,
            "records:update(records:create(m,r,[{a,1}],#{is_exported=>true}), other, r, #{a=>2})",
            "exception error {badrecord,r}",
            1,
        ),
        eval(
            "records/update_wrong_name_is_badrecord",
            "records:update/4 raises {badrecord,Name} when the name does not match",
            error_semantics,
            val,
            "records:update(records:create(m,r,[{a,1}],#{is_exported=>true}), m, other, #{a=>2})",
            "exception error {badrecord,other}",
            1,
        ),
        eval(
            "records/create_missing_is_exported_is_badarg",
            "records:create/4 raises badarg when the options map has no is_exported key",
            error_semantics,
            val,
            "records:create(m,r,[{a,1}],#{})",
            "exception error badarg",
            1,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        group, FOREIGN_RECORD_SOURCE, NATIVE_RECORD_SOURCE, RECORD_DEFINER_SOURCE,
        RECORD_UNEXPORTED_SOURCE,
    };
    use crate::corpus::{CaseKind, Contract, Probe, DIR_PLACEHOLDER};
    use std::collections::HashSet;

    #[test]
    fn every_case_declares_a_native_record_contract_and_a_well_formed_golden() {
        let cases = group();
        assert!(!cases.is_empty());
        let mut ids = HashSet::new();
        for case in &cases {
            assert!(ids.insert(case.id), "duplicate id {}", case.id);
            assert!(case.id.starts_with("records/"), "id {} group", case.id);
            assert!(!case.description.is_empty(), "{} description", case.id);
            assert!(!case.contracts.is_empty(), "{} contracts", case.id);
            assert!(
                case.contracts.iter().all(|c| matches!(
                    c,
                    Contract::BeamAsm | Contract::LanguageSemantics | Contract::OtpApps
                )),
                "{} claims a contract it does not evidence",
                case.id
            );
            let golden = case.golden.stdout.expect("a golden line");
            assert!(
                !golden.is_empty() && !golden.ends_with('\n'),
                "{} golden",
                case.id
            );
            match &case.kind {
                CaseKind::Eval { probe, .. } => {
                    assert!(
                        golden.starts_with("ok ") || golden.starts_with("exception "),
                        "{}: eval golden shape {golden:?}",
                        case.id
                    );
                    if golden.starts_with("exception ") {
                        assert_ne!(case.golden.exit_code, 0, "{} exit code", case.id);
                    } else {
                        assert_eq!(case.golden.exit_code, 0, "{} exit code", case.id);
                        // A record value is compared by canonical ETF bytes (A.2).
                        if golden.contains("<<131,67,") {
                            assert_eq!(*probe, Probe::TermToBinary, "{} probe", case.id);
                        }
                    }
                }
                CaseKind::Module(run) => {
                    assert_eq!(case.golden.exit_code, 0, "{} exit code", case.id);
                    assert_eq!(case.kind_label(), "module-run");
                    assert_eq!(run.source, NATIVE_RECORD_SOURCE, "{} source", case.id);
                    assert_eq!(run.module, "nr_native", "{} module", case.id);
                    assert_eq!(run.arity as usize, run.args.len(), "{} arity", case.id);
                    assert!(
                        golden
                            .trim_start_matches('-')
                            .chars()
                            .all(|c| c.is_ascii_digit()),
                        "{}: a module golden is an integer line, got {golden:?}",
                        case.id
                    );
                }
                CaseKind::ModuleEnv { run, extra, env } => {
                    // The group's only environment-variable cases: the create form's `foreign`
                    // record id, whose owner modules reach the code path through `ERL_AFLAGS`
                    // alone.
                    assert_eq!(case.golden.exit_code, 0, "{} exit code", case.id);
                    assert_eq!(case.kind_label(), "module-run", "{}", case.id);
                    assert_eq!(run.source, FOREIGN_RECORD_SOURCE, "{} source", case.id);
                    assert_eq!(run.module, "nr_foreign", "{} module", case.id);
                    assert_eq!(run.arity as usize, run.args.len(), "{} arity", case.id);
                    let owners: Vec<&str> = extra.iter().map(|helper| helper.module).collect();
                    assert_eq!(owners, ["recdef", "recbad"], "{} owners", case.id);
                    assert_eq!(env.len(), 1, "{} env", case.id);
                    assert_eq!(env[0].0, "ERL_AFLAGS", "{} env", case.id);
                    assert!(
                        env[0].1.contains(DIR_PLACEHOLDER),
                        "{}: the compiled dir must reach the code path through the env, got {:?}",
                        case.id,
                        env[0].1
                    );
                    assert!(
                        golden
                            .trim_start_matches('-')
                            .chars()
                            .all(|c| c.is_ascii_digit()),
                        "{}: a module golden is an integer line, got {golden:?}",
                        case.id
                    );
                }
                CaseKind::EvalEnv { .. } | CaseKind::Command(_) => {
                    panic!(
                        "{}: the records group declares no eval-env or command cases",
                        case.id
                    )
                }
            }
        }
    }
    #[test]
    fn the_group_covers_the_create_update_and_get_definition_bifs_and_the_opcode_module() {
        let cases = group();
        for needle in [
            "records:create(",
            "records:update(",
            "records:get_definition(",
            "records:is_exported(",
        ] {
            assert!(
                cases
                    .iter()
                    .any(|c| c.expr().is_some_and(|e| e.contains(needle))),
                "no eval case exercises {needle}"
            );
        }

        let functions: Vec<&str> = cases
            .iter()
            .filter_map(|c| match &c.kind {
                CaseKind::Module(run) | CaseKind::ModuleEnv { run, .. } => Some(run.function),
                CaseKind::Eval { .. } | CaseKind::EvalEnv { .. } | CaseKind::Command(_) => None,
            })
            .collect();
        // Field access, update, guard, match and the external-record paths all execute the
        // native-record opcodes; `def_fields` reaches the get_definition BIF from a module;
        // `create_local` is the create form with an own-record atom identity, and the
        // `mk_*` functions the create form with a `{Module,Name}` foreign identity.
        for expected in [
            "fld_a",
            "fld_b",
            "total",
            "upd_a",
            "upd_total",
            "guard",
            "match_scale",
            "ext_fld",
            "ext_acc",
            "ext_upd",
            "def_fields",
            "create_local",
            "mk_field",
            "mk_total",
            "mk_definition",
            "mk_not_exported",
        ] {
            assert!(
                functions.contains(&expected),
                "no module case runs {expected}"
            );
        }
        // `records/get_definition_unknown_record` raises the oracle's plain `badarg` (a record
        // no loaded module defines); the runtime reproduces it through its record-definition
        // lookup.
        assert!(cases
            .iter()
            .any(|c| c.id == "records/get_definition_unknown_record"
                && c.golden.stdout == Some("exception error badarg")));
    }

    #[test]
    fn the_embedded_module_uses_native_record_syntax() {
        assert!(NATIVE_RECORD_SOURCE.contains("-module(nr_native)."));
        assert!(NATIVE_RECORD_SOURCE.contains("-record #rec{a, b}."));
        assert!(NATIVE_RECORD_SOURCE.contains("R#rec.a"));
        assert!(NATIVE_RECORD_SOURCE.contains("R#rec{a = 9}"));
        assert!(NATIVE_RECORD_SOURCE.contains("#rec{a = A, b = B} = R"));
        // The create form with the running module's **own** record identity: a compile-time
        // literal (`put_record` create form, `Src` is `nil`), not the `records:create/4` BIF.
        assert!(NATIVE_RECORD_SOURCE
            .contains("create_local() -> R = #rec{a = 1, b = 2}, R#rec.a + R#rec.b."));
    }

    #[test]
    fn the_foreign_create_form_module_names_another_modules_record_id() {
        // `#recdef:rec{…}` (an explicit `#Module:Name{}` literal) is what makes the compiler
        // emit the `{recdef, rec}` identity **pair** — the create form the engine cannot
        // resolve from the running module's own `Recs` chunk. The field list comes from the
        // module's own local `-record #rec{a, b}` declaration.
        assert!(FOREIGN_RECORD_SOURCE.contains("-module(nr_foreign)."));
        assert!(FOREIGN_RECORD_SOURCE.contains("-record #rec{a, b}."));
        assert!(FOREIGN_RECORD_SOURCE.contains("#recdef:rec{a = 1, b = 2}"));
        assert!(FOREIGN_RECORD_SOURCE.contains("#recbad:rec{a = 1, b = 2}"));
        // The owner is loaded explicitly: the oracle's create form does not load an absent
        // owner (see the module docs), so both runtimes see a *loaded* foreign module.
        assert!(FOREIGN_RECORD_SOURCE.contains("code:ensure_loaded(recdef)"));
        assert!(FOREIGN_RECORD_SOURCE.contains("code:ensure_loaded(recbad)"));
        // The two owners differ in exactly one way: `recdef` exports its record.
        assert!(RECORD_DEFINER_SOURCE.contains("-export_record([rec])."));
        assert!(!RECORD_UNEXPORTED_SOURCE.contains("-export_record"));
        // Both declare the same record, so the only difference the create form can observe is
        // the export flag.
        assert!(RECORD_DEFINER_SOURCE.contains("-record #rec{a, b}."));
        assert!(RECORD_UNEXPORTED_SOURCE.contains("-record #rec{a, b}."));
        assert!(group().iter().any(|c| matches!(
            &c.kind,
            CaseKind::ModuleEnv { run, .. } if run.function == "mk_not_exported"
        )));
    }
    #[test]
    fn the_embedded_module_reaches_the_external_record_and_the_get_definition_bif() {
        // `is_record_accessible` (190) needs an external record: the module imports the
        // record from another module and uses `#nr_ext:ext{}` syntax on it.
        assert!(NATIVE_RECORD_SOURCE.contains("-import_record(nr_ext, [ext])."));
        assert!(NATIVE_RECORD_SOURCE.contains("R#nr_ext:ext.a"));
        assert!(NATIVE_RECORD_SOURCE.contains("#nr_ext:ext{a = A, b = B} = R"));
        assert!(NATIVE_RECORD_SOURCE.contains("R#nr_ext:ext{a = 5}"));
        // The positive `records:get_definition/2` path is reached from inside the module
        // (the BIF only consults the code server table of a *loaded* module).
        assert!(NATIVE_RECORD_SOURCE.contains("records:get_definition(nr_native, rec)"));
        assert!(group().iter().any(|c| matches!(
            &c.kind,
            CaseKind::Module(run) if run.function == "def_fields"
        )));
    }

    #[test]
    fn the_is_exported_boolean_and_the_structured_badrecord_badmap_reasons_are_pinned() {
        // The Rust runtime used to answer `records:is_exported/1` with the wrong atom
        // (`linux`/`unix`); it now agrees, and these cases hold that agreement.
        let golden = |id: &str| {
            group()
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no case {id}"))
                .golden
                .stdout
        };
        assert_eq!(golden("records/is_exported_true"), Some("ok true"));
        assert_eq!(golden("records/is_exported_false"), Some("ok false"));
        assert_eq!(
            golden("records/is_exported_non_record_is_badrecord"),
            Some("exception error {badrecord,{x,y}}")
        );
        assert_eq!(
            golden("records/create_bad_options_is_badmap"),
            Some("exception error {badmap,badopts}")
        );
    }
}
