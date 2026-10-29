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

//! Contract **A.7** — the `ets` match-spec / `select` family and the `info/1,2` field set.
//!
//! Split out of [`super::ets`] by responsibility: that group pins the plain table operations
//! (`insert`/`lookup`/`member`/navigation/…) while this one pins the *query* surface —
//! `select/1,2,3` and `match/3` / `match_object/3` with their continuations, `match/2`,
//! `match_object/2`, `select_count/2`, `select_delete/2`, `select_replace/2`,
//! `match_delete/2`, `match_spec_compile/1` + `match_spec_run/2`, and the 15-field `info/1`
//! order.
//!
//! Two rules the cases must respect (both oracle behaviours, not gaps):
//! * the `select` family takes **only** a raw clause list (a `match_spec_compile/1` handle is
//!   `badarg`), while `match_spec_run/2` takes **only** the compiled handle (a raw clause list
//!   is `badarg`);
//! * the `info/1` `id`/`owner`/`memory`/`refs` values and a `select/3` continuation are
//!   volatile per VM start — cases assert their **shape**, never the rendered value.
//!
//! Every table name below is fresh (`ets_spec_*`) and each case is a single self-terminating
//! expression, so a fresh VM per case owns its own table.
//!
//! # What is pinned about a continuation — and what deliberately is not
//!
//! `ets:select/3` returns `{Matches, Continuation}`. The result list is pinned exactly (on an
//! `ordered_set` the first key fixes the order, so no `lists:sort/1` is needed), and the chunk
//! itself is pinned as a **2-tuple** (`tuple_size(Chunk)`). What is *not* pinned is the
//! continuation's own representation: the oracle's is a table-type-specific tuple (a 6-tuple for
//! a `set`, an 8-tuple for an `ordered_set`, both carrying per-VM refs) while the Rust runtime's
//! is a bare `Ref`-thing, so the type predicates (`is_reference/1`, `is_tuple/1`) do differ on
//! that value. OTP documents the continuation as opaque, so the gate pins only what is
//! contractual — the matches and the chunk arity — and exercises the terminal continuation
//! through a *real* continuation handle (`select/3` with `Limit = 1` on a one-object table)
//! rather than by feeding the bare atom `'$end_of_table'` back into `select/1`.
//!
//! # Two match-spec subtleties pinned here
//!
//! * `select_count/2` counts the objects whose spec body evaluates to `true`, not the objects
//!   the head matches — a non-boolean body (e.g. `['$1']`) yields `0`.
//! * A match-spec **body** that calls a guard BIF — written as the `{Func, Arg…}` tuple the
//!   matcher grammar uses — yields the atom `'EXIT'` as the match result when that BIF fails (a
//!   *guard* failure instead drops the match; an arithmetic opcode failure raises out of
//!   `select`). The case below pins that: `element(2, a)` on the atom `a` fails.
//!
//! # The two `specs/` pivots pinned below
//!
//! * `match_spec_compile/1` rejects an **empty** spec: the empty binary `<<>>` raises
//!   `error:badarg`, pinned by `specs/match_spec_compile_empty_spec_is_badarg` (the empty
//!   *list* `[]` raises the same reason in the oracle but is accepted by the Rust runtime, so a
//!   `[]` case would pin a divergence rather than a contract).
//! * a body element written as the 1-tuple `{self}` is the grammar's builtin *call* form, so it
//!   yields the **calling pid** — pinned by
//!   `specs/match_spec_run_self_body_returns_the_calling_pid`, which asserts the relation
//!   `=:= [self()]` (plus the pid shape) because a pid is per-VM. The literal-atom spelling
//!   (`self` alone in a body, which is *not* a call) is a shared observation.
//!
//! # The stored-handle-across-a-collection family (`specs/stored_match_spec_*`)
//!
//! `ets:match_spec_compile/1` answers an **opaque handle** whose payload lives off the caller's
//! heap, while the handle itself is an ordinary term a process can store, send and keep live
//! across a collection. That combination is the shape a case has to reach: a *stored* handle
//! that survives a **copying** collection (so the collector must relocate or track the handle
//! instead of dropping it), and a handle handed to **another process** and run there. Neither
//! is observable from `-eval`'s tree walker — the walker neither compiles nor runs real
//! bytecode, and its own term storage does not exercise the engine's GC root set — so all
//! three cases are **module-run** cases: the embedded `diff_spec_store` module is compiled
//! with the system `erlc` and executed through the runtime's `run` action, i.e. real bytecode
//! on an engine machine in both runtimes. The spec is a guard-plus-arithmetic projection
//! (`'$1' > 0` → `'$1' + 100`) over the integer object `100`, so every golden is the
//! order-stable integer **`200`** — the right answer only when the stored handle still runs
//! the same matcher after the collection.
//!
//! `survive/0` keeps the handle in a variable (a register/stack root across the `gc_bif`),
//! `survive_two/0` stores it in the process dictionary and forces **two** collections with
//! garbage built in between (a stored handle must survive more than one relocation), and
//! `cross_process/0` sends it to a spawned process that runs it and replies. Every receive is
//! bounded (`receive … after 5000 -> timeout end`), so a lost or corrupted handle shows up as
//! a visible `timeout` answer — a mismatch — and never as a hang.

use super::{Case, CaseKind, Contract, Golden, ModuleRun, Probe};

/// The embedded module for the three stored-handle cases (`specs/stored_match_spec_*`).
///
/// Every function returns an integer (the shape a module-run golden pins) after running a
/// **stored** `ets:match_spec_compile/1` handle across a forced collection:
///
/// * `spec/0` — the one spec all three cases store: it matches the bare object `'$1'`, guards
///   it with `'$1' > 0` and projects `'$1' + 100`, so the object `100` yields exactly `200`;
/// * `survive/0` — the handle is a **variable** live across `erlang:garbage_collect/1` (a
///   register/stack root of the `gc_bif`), then run;
/// * `survive_two/0` — the handle is stored in the **process dictionary**, garbage is built and
///   the process collects **twice** before the handle is fetched back and run;
/// * `cross_process/0` — the handle is sent to a spawned process, which runs it there and
///   replies; both receives are bounded, so a lost handle answers the visible atom `timeout`.
const DIFF_SPEC_STORE_SOURCE: &str = "-module(diff_spec_store).\n\
     -export([survive/0, survive_two/0, cross_process/0]).\n\
     spec() ->\n\
         ets:match_spec_compile([{'$1', [{'>', '$1', 0}], [{'+', '$1', 100}]}]).\n\
     survive() ->\n\
         S = spec(),\n\
         true = erlang:garbage_collect(self()),\n\
         [Stored] = ets:match_spec_run([100], S),\n\
         Stored.\n\
     survive_two() ->\n\
         put(stored_spec, spec()),\n\
         _ = lists:seq(1, 2000),\n\
         true = erlang:garbage_collect(self()),\n\
         _ = lists:seq(1, 2000),\n\
         true = erlang:garbage_collect(self()),\n\
         [Stored] = ets:match_spec_run([100], get(stored_spec)),\n\
         Stored.\n\
     cross_process() ->\n\
         Self = self(),\n\
         Pid = spawn(fun() ->\n\
             receive\n\
                 {spec, Sp} -> erlang:send(Self, {res, ets:match_spec_run([100], Sp)})\n\
             after 5000 -> ok\n\
             end\n\
         end),\n\
         erlang:send(Pid, {spec, spec()}),\n\
         receive\n\
             {res, Answer} -> hd(Answer)\n\
         after 5000 -> timeout\n\
         end.\n";

/// One **module-run** case of the stored-handle family: the embedded [`DIFF_SPEC_STORE_SOURCE`]
/// is compiled with the system `erlc` and the named 0-arity function is applied by both
/// runtimes (`beam_rt run <dir>/diff_spec_store.beam diff_spec_store:<function>/0` on the Rust
/// side, `-pa <dir>` + `-eval` on the oracle's), so the spec really runs as bytecode on an
/// engine machine. The golden is the oracle's integer line (`200`).
fn stored_spec_case(id: &'static str, description: &'static str, function: &'static str) -> Case {
    Case {
        id,
        description,
        // A.2 for the `erlc`-compiled `.beam`, A.1 for the value the matcher must still
        // produce after the collection, A.7 for the `ets:match_spec_*` surface itself.
        contracts: &[
            Contract::BeamAsm,
            Contract::LanguageSemantics,
            Contract::OtpApps,
        ],
        kind: CaseKind::Module(ModuleRun {
            source: DIFF_SPEC_STORE_SOURCE,
            module: "diff_spec_store",
            function,
            arity: 0,
            args: &[],
            nif: None,
        }),
        golden: Golden {
            stdout: Some("200"),
            exit_code: 0,
        },
    }
}

/// The A.7 `ets` match-spec / `select` / `info` cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- the `info/1,2` field set ---
        Case {
            id: "ets/info_15_fields",
            description: "info/1 emits the oracle's 15 item names, in order",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The item *names* are stable; the `owner`/`memory`/`refs`/`id` *values* are not,
                // so only the names (and the arity) are asserted.
                expr: "begin T = ets:new(ets_spec_info, [set]), {length(ets:info(T)), [element(1,I) || I <- ets:info(T)]} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {15,[id,decentralized_counters,read_concurrency,write_concurrency,compressed,memory,owner,heir,name,size,node,named_table,type,keypos,protection]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_id_is_tid",
            description: "info(T, id) returns the table's own handle (a tid), not a bare integer",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_tid, [set]), ets:info(T, id) =:= T end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // --- select/2,3 ---
        Case {
            id: "ets/select_2",
            description: "select/2 projects element 2 of every matching object",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A `set` iteration order is implementation-defined, so the two matches are sorted.
                expr: "begin T = ets:new(ets_spec_sel2, [set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), lists:sort(ets:select(T, [{{'$1','$2'},[],['$2']}])) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_3_continuation",
            description: "select/3 with Limit=1 yields the first match and a 2-tuple chunk",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // An `ordered_set` fixes the first key (`a`), so the match list is deterministic.
                // The continuation itself is opaque — only the chunk arity is pinned (see module docs).
                expr: "begin T = ets:new(ets_spec_sel3, [ordered_set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), Chunk = ets:select(T, [{{'$1','$2'},[],['$2']}], 1), {element(1,Chunk), tuple_size(Chunk)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[1],2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_1_end_of_table",
            description: "select/1 on an exhausted continuation reports '$end_of_table'",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // One object + Limit=1: the first select returns it, and the returned continuation
                // is already exhausted, so select/1 answers the sentinel.
                expr: "begin T = ets:new(ets_spec_sel1, [ordered_set]), ets:insert(T, {a,1}), {_,Ch} = ets:select(T, [{{'$1','$2'},[],['$2']}], 1), ets:select(Ch) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok '$end_of_table'"),
                exit_code: 0,
            },
        },
        // --- match/2,3, match_object/2,3 ---
        Case {
            id: "ets/match_2",
            description: "match/2 returns the bound variables of a pattern as a list of lists",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_match, [set]), ets:insert(T, {a,1}), ets:match(T, {'$1','$2'}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [[a,1]]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/match_object_2",
            description: "match_object/2 returns the whole matching objects",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_matchobj, [set]), ets:insert(T, {a,1}), ets:match_object(T, {'$1','_'}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1}]"),
                exit_code: 0,
            },
        },
        // The `/3` (Limit) forms of `match`/`match_object` return the same `{Matches, Continuation}`
        // chunk shape as `select/3`; the continuation itself stays opaque (see module docs).
        Case {
            id: "ets/match_3_continuation",
            description: "match/3 with Limit=1 yields the first binding list and a 2-tuple chunk",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_match3, [ordered_set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), Chunk = ets:match(T, {'$1','$2'}, 1), {element(1,Chunk), tuple_size(Chunk)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[[a,1]],2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/match_object_3_continuation",
            description: "match_object/3 with Limit=1 yields the first object and a 2-tuple chunk",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_matchobj3, [ordered_set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), Chunk = ets:match_object(T, '_', 1), {element(1,Chunk), tuple_size(Chunk)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{a,1}],2}"),
                exit_code: 0,
            },
        },
        // --- the counting / deleting / replacing queries ---
        Case {
            id: "ets/select_count",
            description: "select_count/2 counts the objects whose spec body evaluates to true",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The body must be `true` for the object to be counted, so a head-only spec
                // (body `['$1']`) would count 0.
                expr: "begin T = ets:new(ets_spec_count, [set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), ets:select_count(T, [{{'$1',2},[],[true]}]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_delete",
            description: "select_delete/2 returns the number of deleted objects",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_del, [set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), ets:select_delete(T, [{{'$1',2},[],[true]}]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/match_delete_true",
            description: "match_delete/2 is a stdlib wrapper returning the atom true, not a count",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_mdel, [set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), ets:match_delete(T, {'$1',2}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_replace_count",
            description: "select_replace/2 replaces matching objects and returns the replace count",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The body's `{{'$1',3}}` is the 1-tuple wrapping the replacement object.
                expr: "begin T = ets:new(ets_spec_repl, [ordered_set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), ets:select_replace(T, [{{'$1',2},[],[{{'$1',3}}]}]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_replace_count_rewrites",
            description: "the replacement select_replace/2 performed is observable in the table",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_repl2, [ordered_set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), ets:select_replace(T, [{{'$1',2},[],[{{'$1',3}}]}]), lists:sort(ets:tab2list(T)) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1},{b,3}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_replace_bag_badarg",
            description: "select_replace/2 rejects a bag table with badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_replbag, [bag]), ets:insert(T, {a,1}), try ets:select_replace(T, [{{'$1','$2'},[],[{{'$1',3}}]}]) catch error:Reason -> Reason end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok badarg"),
                exit_code: 0,
            },
        },
        // --- the raw-vs-compiled spec asymmetry ---
        Case {
            id: "ets/match_spec_compile_shape",
            description: "match_spec_compile/1 returns an opaque reference handle",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin S = ets:match_spec_compile([{{'$1','$2'},[],['$2']}]), is_reference(S) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/match_spec_run_compiled",
            description: "match_spec_run/2 runs a compiled spec over a list of objects",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin S = ets:match_spec_compile([{{'$1','$2'},[],['$2']}]), ets:match_spec_run([{a,1},{b,2}], S) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/match_spec_run_raw_badarg",
            description: "match_spec_run/2 rejects a raw clause list (it needs the compiled handle)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "try ets:match_spec_run([{a,1}], [{{'$1','$2'},[],['$2']}]) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok badarg"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_compiled_spec_badarg",
            description: "select/2 rejects a compiled handle (it needs a raw clause list)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_cspec, [set]), S = ets:match_spec_compile([{{'$1','$2'},[],['$2']}]), try ets:select(T, S) catch error:Reason -> Reason end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok badarg"),
                exit_code: 0,
            },
        },
        // --- a failing spec body ---
        Case {
            id: "ets/match_spec_body_raises_exit",
            description: "a spec body whose BIF call fails yields the atom 'EXIT' as the match",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `{'element',2,'$1'}` is the grammar's `{Func, Arg…}` call form: element(2, a)
                // on the atom `a` fails, and a *body* BIF failure produces 'EXIT' (the oracle's
                // `FAIL_TERM`), unlike a guard failure (drops the match) or an arithmetic failure.
                expr: "begin T = ets:new(ets_spec_exit, [set]), ets:insert(T, {a}), ets:select(T, [{{'$1'},[],[{'element',2,'$1'}]}]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok ['EXIT']"),
                exit_code: 0,
            },
        },
        // --- the `ordered_set` arithmetic-key rule (A.7) ---
        //
        // Inside an `ordered_set`, integer and float keys that compare equal (`1` and `1.0`) are
        // the *same* key: the second insert replaces the first object (it is not a second object,
        // and it does not raise), and a lookup by *either* the integer or the float term finds it.
        // The stored key term -- and therefore the term `first/1`/`next/2` reports -- is the one of
        // the *inserted* term, so a key inserted as `1.0` stays a float `1.0` (never normalised to
        // the integer `1`). The hash-based table types do not share this rule (they hash the key
        // term), which is why these cases are `ordered_set`-specific.
        Case {
            id: "ets_spec/ordered_set_int_float_same_key",
            description: "an ordered_set treats 1 and 1.0 as one key, so the second insert replaces",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(t_os_arith,[ordered_set]), ets:insert(T,{1.0,a}), ets:insert(T,{1,b}), ets:lookup(T,1) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{1,b}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets_spec/ordered_set_int_float_lookup_same_key",
            description: "the same ordered_set object is also found by the float spelling of the key",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(t_os_arith_f,[ordered_set]), ets:insert(T,{1.0,a}), ets:insert(T,{1,b}), ets:lookup(T,1.0) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{1,b}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets_spec/ordered_set_int_float_size_one",
            description: "the colliding int/float keys leave the ordered_set with a single object",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(t_os_size,[ordered_set]), ets:insert(T,{1.0,a}), ets:insert(T,{1,b}), ets:info(T,size) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets_spec/ordered_set_first_reports_inserted_key",
            description: "first/1 reports the inserted key term (1.0 stays a float, not the integer 1)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The key is inserted as the float `1.0`, so `first/1` reports `1.0` (a float),
                // not the integer `1` -- the arithmetic-equality rule selects the slot, it does
                // not rewrite the stored key term.
                expr: "begin T = ets:new(t_os_first,[ordered_set]), ets:insert(T,{1.0,x}), ets:first(T) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1.0"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets_spec/ordered_set_next_reports_inserted_key",
            description: "next/2 reports the next inserted key term (2.0 stays a float) after a colliding search key",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Keys `1.0` and `2.0` are distinct objects here; `next/2` searched with the
                // *integer* `1` (which arithmetic-collides with the stored float `1.0`) reports
                // the following key as the *inserted* float term `2.0`.
                expr: "begin T = ets:new(t_os_next,[ordered_set]), ets:insert(T,{1.0,a}), ets:insert(T,{2.0,b}), ets:next(T,1) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2.0"),
                exit_code: 0,
            },
        },
        // --- the `info/1,2` field set on an ordered_set ---
        //
        // The 15 item *names* of `ets:info/1` -- and their order -- are table-type-independent, so
        // the `ets/info_15_fields` case above already pins the order; it is deliberately not
        // duplicated for `ordered_set` here. What *is* type-specific is the value of the `type`
        // item, pinned below alongside `keypos` (the same default position for every table type).
        Case {
            id: "ets_spec/ordered_set_info_type",
            description: "info(T, type) reports the ordered_set table type",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(t_os_it,[ordered_set]), ets:info(T,type) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok ordered_set"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets_spec/ordered_set_info_keypos",
            description: "info(T, keypos) reports the default key position 1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(t_os_ik,[ordered_set]), ets:info(T,keypos) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        // --- widened: guards, projections and the counting/deleting queries ----------------
        Case {
            id: "ets/select_guard_projection",
            description: "select/2 with a guard projects the key of the objects that pass it",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_sg, [set]), ets:insert(T, [{a,1},{b,2}]), \
                       ets:select(T, [{{'$1','$2'},[{'>','$2',1}],['$1']}]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [b]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_projection_swapped_pair",
            description: "a select/2 body may return a compound (1-tuple) replacement value",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_srp, [ordered_set]), \
                       ets:insert(T, [{a,1},{b,2}]), \
                       ets:select(T, [{{'$1','$2'},[],[{{'$2','$1'}}]}]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{1,a},{2,b}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_count_all_matches",
            description: "select_count/2 counts every object whose spec body is true",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_sca, [set]), ets:insert(T, [{a,1},{b,2}]), \
                       ets:select_count(T, [{{'$1','$2'},[],[true]}]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/select_delete_all_matches",
            description: "select_delete/2 deletes every object whose spec body is true",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_sda, [ordered_set]), \
                       ets:insert(T, [{a,1},{b,2},{c,3}]), \
                       ets:select_delete(T, [{{'_','$2'},[],[true]}]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/match_spec_run_guard_projection",
            description: "match_spec_run/2 runs a compiled spec with a guard over a plain list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_msr, [set]), \
                       ets:match_spec_run([{a,1},{b,2}], \
                       ets:match_spec_compile([{{'$1','$2'},[{'>','$2',1}],['$1']}])) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [b]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/match_object_wildcard_all",
            description: "match_object/2 with the '_' wildcard returns every object",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_mow, [set]), ets:insert(T, [{a,1}]), \
                       ets:match_object(T, '_') end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/first_last_ordered_set_term_order",
            description: "first/1 and last/1 of an ordered_set agree on term order, not insertion order",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_spec_fl, [ordered_set]), \
                       ets:insert(T, [{b,2},{a,1}]), {ets:first(T), ets:last(T)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {a,b}"),
                exit_code: 0,
            },
        },
        // --- `specs/`: the empty-spec pivot and the matcher's `self/0` ----------------------
        Case {
            id: "specs/match_spec_compile_empty_spec_is_badarg",
            description: "match_spec_compile/1 rejects the empty binary spec with badarg",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                // `ets:match_spec_compile(<<>>)` raises `error:badarg` (`badarg` is not a valid
                // clause list), so the reason is read back through a `catch` and the case stays a
                // value, not a crash. The spec list is compiled eagerly, so no table is needed.
                // The empty *list* `[]` is deliberately not asserted (see the module docs).
                expr: "try ets:match_spec_compile(<<>>) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok badarg"),
                exit_code: 0,
            },
        },
        Case {
            id: "specs/match_spec_run_self_body_returns_the_calling_pid",
            description: "a `{self}` body element is the matcher's self/0 call and yields the calling pid",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                // `{self}` (the 1-tuple) is the match-spec grammar's builtin *call* form, so the
                // body element yields the calling pid -- not the literal atom `self` (that is the
                // un-tupled spelling). A pid is per-VM, so the result is asserted as the relation
                // `=:= [self()]` evaluated in the same expression (plus the pid shape and the
                // single-match arity), never as a pid literal.
                expr: "begin M = ets:match_spec_run([{a}], ets:match_spec_compile([{'_',[],[{self}]}])), \
                       {M =:= [self()], is_pid(hd(M)), length(M)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {true,true,1}"),
                exit_code: 0,
            },
        },
        // --- `specs/`: a *stored* handle across a garbage collection --------------------------
        //
        // `match_spec_compile/1` answers an opaque handle over an off-heap compiled matcher;
        // the handle is an ordinary term that a process stores, sends and keeps live across a
        // collection. All three cases run as **compiled bytecode** (module-run) because that
        // is the tier whose GC root set and matcher relocation the handle exercises — the
        // `-eval` tree walker would exercise neither. The spec guards and projects the bare
        // object (`'$1' > 0` → `'$1' + 100`), so the object `100` answers the order-stable
        // integer `200`.
        stored_spec_case(
            "specs/stored_match_spec_survives_a_collection",
            "a compiled match spec kept in a variable still runs after a forced collection",
            "survive",
        ),
        stored_spec_case(
            "specs/stored_match_spec_survives_two_collections",
            "a compiled match spec stored in the process dictionary still runs after two collections",
            "survive_two",
        ),
        stored_spec_case(
            "specs/stored_match_spec_passed_to_another_process",
            "a compiled match spec sent to another process runs there after the hand-off",
            "cross_process",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, CaseKind, Contract, DIFF_SPEC_STORE_SOURCE};

    /// The family's ids, in the order [`group`] emits them, with the function each applies.
    const STORED_SPEC_CASES: [(&str, &str); 3] = [
        ("specs/stored_match_spec_survives_a_collection", "survive"),
        (
            "specs/stored_match_spec_survives_two_collections",
            "survive_two",
        ),
        (
            "specs/stored_match_spec_passed_to_another_process",
            "cross_process",
        ),
    ];

    /// Each stored-handle case must be a **module-run** case over the embedded module, never an
    /// `-eval` expression: the compiled route is the tier whose GC root set and matcher
    /// relocation the stored handle exercises, so an `Eval` rewrite would silently drop what
    /// the family exists to pin.
    #[test]
    fn the_stored_spec_family_is_bytecode_not_the_eval_walker() {
        let cases = group();
        for (id, function) in STORED_SPEC_CASES {
            let case = cases
                .iter()
                .find(|case| case.id == id)
                .unwrap_or_else(|| panic!("case {id} is missing"));
            let CaseKind::Module(run) = &case.kind else {
                panic!("case {id} must be a module-run case (real bytecode), not -eval");
            };
            assert_eq!(run.module, "diff_spec_store", "case {id}");
            assert_eq!(run.source, DIFF_SPEC_STORE_SOURCE, "case {id}");
            assert_eq!(run.function, function, "case {id}");
            assert_eq!(run.arity, 0, "case {id}");
            assert!(run.args.is_empty(), "case {id}");
            assert_eq!(run.nif, None, "case {id}");
            assert!(
                case.contracts.contains(&Contract::BeamAsm)
                    && case.contracts.contains(&Contract::LanguageSemantics)
                    && case.contracts.contains(&Contract::OtpApps),
                "case {id} must evidence .beam + A.1 + A.7"
            );
        }
        // Every id is emitted exactly once.
        for (id, _) in STORED_SPEC_CASES {
            assert_eq!(
                cases.iter().filter(|case| case.id == id).count(),
                1,
                "case {id} must be registered once"
            );
        }
    }

    /// The golden is the oracle's order-stable integer `200` — never a pid, a reference, a
    /// timeout or a map image, all of which would either be per-VM or encode a divergence.
    #[test]
    fn every_stored_spec_golden_is_the_integer_200() {
        for case in group() {
            if !STORED_SPEC_CASES.iter().any(|(id, _)| *id == case.id) {
                continue;
            }
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {} has no golden", case.id));
            assert_eq!(golden, "200", "case {}: golden", case.id);
            assert_eq!(golden.parse::<i64>(), Ok(200), "case {}", case.id);
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
    }

    /// The module must really compile the spec, store it, collect (twice in the two-collection
    /// row, with garbage built in between), and hand it to a second process — and it must bound
    /// **every** receive, so a lost handle shows up as the visible atom `timeout` rather than
    /// hanging the case.
    #[test]
    fn the_compiled_module_stores_collects_and_bounds_every_receive() {
        let source = DIFF_SPEC_STORE_SOURCE;
        assert!(source.contains("-module(diff_spec_store)."));
        assert!(source.contains("-export([survive/0, survive_two/0, cross_process/0])."));
        // The compiled handle comes from `match_spec_compile/1` and is *run* by
        // `match_spec_run/2` (a raw clause list would be `badarg`).
        assert!(source.contains("ets:match_spec_compile([{'$1',"));
        // One run per route: the variable root, the process dictionary, and the spawned
        // process that received the handle.
        assert_eq!(source.matches("ets:match_spec_run([100]").count(), 3);
        // A forced collection in every function, and two of them in the two-collection row.
        assert_eq!(source.matches("erlang:garbage_collect(self())").count(), 3);
        assert!(source.contains("put(stored_spec, spec())"));
        assert!(source.contains("get(stored_spec)"));
        // The cross-process shape: a spawned process runs the handle it was sent.
        assert!(source.contains("spawn(fun() ->"));
        assert!(source.contains("erlang:send(Pid, {spec, spec()})"));
        // No unbounded receive: one `after 5000 ->` bound per `receive`.
        assert_eq!(
            source.matches("receive").count(),
            source.matches("after 5000 ->").count(),
            "every receive must carry a 5000 ms bound"
        );
        assert!(!source.contains("after infinity"));
        // Nothing is printed from the module (a module-run case pins the applied function's
        // `~w` line, so an `io:format` here would corrupt the observable).
        assert!(!source.contains("io:format"));
    }
}
