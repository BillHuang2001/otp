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

use super::{Case, CaseKind, Contract, Golden, Probe};

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
    ]
}
