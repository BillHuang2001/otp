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

//! Contract **A.7** (with **A.1** for the term-ordered navigation cases) — ETS tables.
//!
//! ETS is the in-memory table store every OTP application leans on, and its `ets:*` BIFs are
//! part of the OTP application contract (A.7). The cases below pin the **surface the Rust
//! runtime actually serves**, all through the *value* probe (`~w`), which is a rendering a
//! runtime must reproduce exactly:
//!
//! * **table creation** — `ets:new/2` returns a table handle (a reference) for the default
//!   options and for `set`; two handles are distinct;
//! * **mutation + lookup** — `insert/2`, `insert_new/2`, `lookup/2`, `lookup_element/3,4`,
//!   `member/2`, `delete/1,2`, `delete_object/2`, `delete_all_objects/1`, `take/2`,
//!   `update_counter/3,4`, `update_element/3`;
//! * **table types** — `set` (key replacement), `bag` (dedup of identical objects),
//!   `duplicate_bag` (keeps duplicates), `ordered_set` (term order);
//! * **navigation** — `first/1`/`next/2`/`last/1`/`prev/2` over an `ordered_set` (the
//!   `'$end_of_table'` sentinel for an empty table, and term-ordered walks otherwise — the
//!   A.1 half of this group); and
//! * **introspection** — the stable `info/2` items (`size`, `type`, `protection`, `keypos`,
//!   `name`, `named_table`, `compressed`, `id`-is-a-reference, `memory`-is-an-integer) plus
//!   `badarg` for an unknown item.
//!
//! Every case here is one line of setup + one observation (a fresh VM per case, so the table
//! lives and dies with it), and each is verified `agree` against the live oracle. Multi-key
//! maps are never rendered and no volatile value (a table id, a pid, a memory figure) is
//! asserted exactly — only its shape.
//!
//! # The match-spec / `select` family lives in the [`super::ets_spec`] group
//!
//! The *query* surface — `select/1,2,3`, `match/2,3`, `match_object/2,3`, `select_count/2`,
//! `select_delete/2`, `select_replace/2`, `match_delete/2`,
//! `match_spec_compile/1` + `match_spec_run/2`, and the `info/1,2` field set — is served by the
//! Rust runtime and pinned (all `agree`) by the sibling [`super::ets_spec`] group, which was
//! split out of this file by responsibility (see that module's docs for the query surface and
//! the two continuation/match-spec subtleties it pins).
//!
//! The `ets:*` BIFs still outside the runtime's served surface — `slot/2`,
//! `select_reverse/1,2,3`, `is_compiled_ms/1`, `internal_delete_all/2` — raise a plain
//! `error:undef`, an *observation* rather than a capability-gap signal, so a case for them
//! would be a `mismatch` and none is pinned. **Handoff to the runtime nodes** (`beam_bifs` /
//! `beam_ets`).
//!
//! # Other verified divergences found while authoring — deliberately NOT pinned
//!
//! Each produces a *plain Erlang value/reason* (not a capability-gap signal), so a case would
//! be a `mismatch`; they are handoffs to the runtime nodes and are recorded here instead:
//!
//! * **`named_table` lookup by name** — `ets:new(ets_tab,[named_table,set])` then
//!   `ets:lookup(ets_tab, a)` is `[{a,1}]` in the oracle but `badarg` in the Rust runtime
//!   (the name registry is not consulted).
//! * **`ets:info/1,2` on a deleted table** — `ets:delete(T), ets:info(T)` is `undefined` in the
//!   oracle but `badarg` in the Rust runtime.
//! * **`ets:info(T, owner)`** — the oracle returns a pid, the Rust runtime a non-pid
//!   (`is_pid(ets:info(T, owner))` is `true` vs `false`).
//! * **`ets:first/last` on a key-less table**: covered above by the `'$end_of_table'` cases; the
//!   `next/2`/`prev/2` forms raise `badarg` for an absent key in **both** runtimes (agreement),
//!   so only the present-key walks are pinned.
//!
//! **Never edit this file from a context tree that has not rebuilt `differential`**: the corpus
//! is compiled into the tool, so a golden change only takes effect after a rebuild.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.7 ETS cases: the `ets:*` BIF surface the Rust runtime serves.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- table creation / the handle ---
        Case {
            id: "ets/new_returns_reference",
            description: "ets:new/2 returns a table handle (a reference) for a set table",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "is_reference(ets:new(ets_tab_new, [set]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/new_default_options_is_reference",
            description: "ets:new/2 with no options still yields a reference handle (defaults apply)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_def, []), is_reference(T) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/new_handles_are_distinct",
            description: "two ets:new/2 calls yield two distinct table handles",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T1 = ets:new(ets_tab_h1, [set]), T2 = ets:new(ets_tab_h2, [set]), T1 =/= T2 end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // --- insert / lookup round trips ---
        Case {
            id: "ets/insert_lookup_roundtrip",
            description: "insert/2 then lookup/2 returns the stored object",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_rt, [set]), ets:insert(T, {a,1}), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/insert_returns_true",
            description: "insert/2 returns true",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_inst, [set]), ets:insert(T, {a,1}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/insert_list_of_objects",
            description: "insert/2 accepts a list of objects and stores each one",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_list, [set]), ets:insert(T, [{a,1},{b,2}]), ets:info(T, size) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/lookup_missing_key_is_empty",
            description: "lookup/2 of an absent key returns the empty list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_miss, [set]), ets:lookup(T, missing) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/insert_new_new_key_is_true",
            description: "insert_new/2 on an absent key stores it and returns true",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_in1, [set]), ets:insert_new(T, {a,1}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/insert_new_existing_key_is_false",
            description: "insert_new/2 on a present key stores nothing and returns false",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_in2, [set]), ets:insert(T, {a,1}), ets:insert_new(T, {a,2}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/insert_replaces_set_key",
            description: "in a set table a second insert/2 for the same key replaces the object",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_rep, [set]), ets:insert(T, {a,1}), ets:insert(T, {a,2}), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,2}]"),
                exit_code: 0,
            },
        },
        // --- lookup_element ---
        Case {
            id: "ets/lookup_element_pos",
            description: "lookup_element/3 returns the element at the requested position",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_le, [set]), ets:insert(T, {a,1}), ets:lookup_element(T, a, 2) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/lookup_element_default",
            description: "lookup_element/4 returns the default for an absent key",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_led, [set]), ets:lookup_element(T, missing, 2, default) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok default"),
                exit_code: 0,
            },
        },
        // --- member ---
        Case {
            id: "ets/member_present",
            description: "member/2 is true for a stored key",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_mem1, [set]), ets:insert(T, {a,1}), ets:member(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/member_absent",
            description: "member/2 is false for an absent key",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_mem2, [set]), ets:insert(T, {a,1}), ets:member(T, b) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        // --- ordered_set ordering + navigation (A.1 term ordering) ---
        Case {
            id: "ets/tab2list_ordered_set_sorted",
            description: "tab2list/1 of an ordered_set is in ascending term order",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_t2l, [ordered_set]), ets:insert(T, [{2,x},{1,y},{3,z}]), ets:tab2list(T) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{1,y},{2,x},{3,z}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/first_ordered_set",
            description: "first/1 returns the smallest key of an ordered_set",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_first, [ordered_set]), ets:insert(T, [{2,x},{1,y},{3,z}]), ets:first(T) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/next_ordered_set",
            description: "next/2 walks to the successor key of an ordered_set",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_next, [ordered_set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), ets:next(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok b"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/last_ordered_set",
            description: "last/1 returns the largest key of an ordered_set",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_last, [ordered_set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), ets:last(T) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok b"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/prev_ordered_set",
            description: "prev/2 walks to the predecessor key of an ordered_set",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_prev, [ordered_set]), ets:insert(T, {a,1}), ets:insert(T, {b,2}), ets:prev(T, b) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok a"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/first_empty_table_end_of_table",
            description: "first/1 of an empty table is the '$end_of_table' sentinel",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "ets:first(ets:new(ets_tab_empty, [set]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok '$end_of_table'"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/last_empty_table_end_of_table",
            description: "last/1 of an empty table is the '$end_of_table' sentinel",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_emptyl, [set]), ets:last(T) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok '$end_of_table'"),
                exit_code: 0,
            },
        },
        // --- delete / take ---
        Case {
            id: "ets/delete_key",
            description: "delete/2 removes a single key from the table",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_del, [set]), ets:insert(T, {a,1}), ets:delete(T, a), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/delete_table_returns_true",
            description: "delete/1 removes the whole table and returns true",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_delt, [set]), ets:delete(T) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/delete_all_objects",
            description: "delete_all_objects/1 empties the table but keeps the table",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_dao, [set]), ets:insert(T, {a,1}), ets:delete_all_objects(T), ets:info(T, size) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 0"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/delete_object_removes_match",
            description: "delete_object/2 removes the exactly matching object",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_dobj, [set]), ets:insert(T, {a,1}), ets:delete_object(T, {a,1}), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/delete_object_keeps_mismatch",
            description: "delete_object/2 leaves the object when the value does not match",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_dobj2, [set]), ets:insert(T, {a,1}), ets:delete_object(T, {a,2}), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/take_returns_and_removes",
            description: "take/2 returns the stored object",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_take, [set]), ets:insert(T, {a,1}), ets:take(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/take_removes_object",
            description: "take/2 deletes the object it returns",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_take2, [set]), ets:insert(T, {a,1}), ets:take(T, a), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        // --- counters + element updates ---
        Case {
            id: "ets/update_counter_existing",
            description: "update_counter/3 increments a present counter and returns the new value",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_uc, [set]), ets:insert(T, {k, 5}), ets:update_counter(T, k, 1) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 6"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/update_counter_pos",
            description: "update_counter/3 with a {Pos,Incr} spec increments element Pos",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_uc2, [set]), ets:insert(T, {a,1}), ets:update_counter(T, a, {2, 4}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 5"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/update_element_replaces",
            description: "update_element/3 replaces the element at the requested position",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_ue, [set]), ets:insert(T, {a,1}), ets:update_element(T, a, {2, 9}), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,9}]"),
                exit_code: 0,
            },
        },
        // --- info/2: only the VM-stable items ---
        Case {
            id: "ets/info_size",
            description: "info/2 size counts the stored objects",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_size, [set]), ets:insert(T, {a,1}), ets:info(T, size) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_type_set",
            description: "info/2 type reports a set table",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_tset, [set]), ets:info(T, type) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok set"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_type_ordered_set",
            description: "info/2 type reports an ordered_set table",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_toset, [ordered_set]), ets:info(T, type) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok ordered_set"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_protection_public",
            description: "info/2 protection reports the requested public access",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_pub, [set, public]), ets:info(T, protection) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok public"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_protection_default",
            description: "info/2 protection defaults to protected",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_prot, [set]), ets:info(T, protection) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok protected"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_keypos_default",
            description: "info/2 keypos defaults to 1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_kp1, [set]), ets:info(T, keypos) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_keypos_option",
            description: "info/2 keypos reports the {keypos, N} option",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_kp2, [{keypos,2}]), ets:info(T, keypos) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_name",
            description: "info/2 name reports the atom passed to ets:new/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_name, [set]), ets:info(T, name) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok ets_tab_name"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_named_table_false",
            description: "info/2 named_table is false when the option was not given",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_named, [set]), ets:info(T, named_table) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_id_is_reference",
            description: "info/2 id is a table reference",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_id, [set]), is_reference(ets:info(T, id)) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_compressed_false",
            description: "info/2 compressed is false by default",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_comp, [set]), ets:info(T, compressed) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_memory_is_integer",
            description: "info/2 memory is an integer (its exact figure is VM-dependent)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_mem, [set]), is_integer(ets:info(T, memory)) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_unknown_item_badarg",
            description: "info/2 with an unknown item raises badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_badi, [set]), ets:info(T, bogus_item) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- table types ---
        Case {
            id: "ets/bag_dedups_identical_object",
            description: "a bag table stores an identical object only once",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_bag, [bag]), ets:insert(T, {a,1}), ets:insert(T, {a,1}), lists:sort(ets:lookup(T, a)) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/bag_keeps_distinct_objects",
            description: "a bag table keeps objects that share a key but differ",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_bag2, [bag]), ets:insert(T, {a,1}), ets:insert(T, {a,2}), lists:sort(ets:lookup(T, a)) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1},{a,2}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/duplicate_bag_keeps_identical",
            description: "a duplicate_bag table keeps identical objects",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_dbag, [duplicate_bag]), ets:insert(T, {a,1}), ets:insert(T, {a,1}), length(ets:lookup(T, a)) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/keypos_option_end_to_end",
            description: "a {keypos,2} table keys on element 2 for insert and lookup",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_kpe, [{keypos,2}]), ets:insert(T, {x, a, 1}), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{x,a,1}]"),
                exit_code: 0,
            },
        },
        // --- widened: the remaining table types and creation options `info/2` reflects ---
        Case {
            id: "ets/info_type_bag",
            description: "info/2 type reports a bag table",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_type_bag, [bag]), ets:info(T, type) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok bag"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_type_duplicate_bag",
            description: "info/2 type reports a duplicate_bag table",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_type_dbag, [duplicate_bag]), ets:info(T, type) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok duplicate_bag"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_compressed_true",
            description: "info/2 compressed is true when the {compressed, true} option was given",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_cmpr2, [set, compressed]), ets:info(T, compressed) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_read_concurrency_true",
            description: "info/2 read_concurrency is true when {read_concurrency, true} was given",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_rc, [set, {read_concurrency, true}]), \
                       ets:info(T, read_concurrency) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_write_concurrency_true",
            description: "info/2 write_concurrency is true when {write_concurrency, true} was given",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_wc, [set, {write_concurrency, true}]), \
                       ets:info(T, write_concurrency) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/info_named_table_true",
            description: "info/2 named_table is true when the named_table option was given",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_nt2, [named_table, set]), ets:info(T, named_table) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // --- widened: mutation / lookup round trips ---
        Case {
            id: "ets/keypos_three_end_to_end",
            description: "a {keypos,3} table keys on element 3 for insert and lookup",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_kp3, [{keypos,3}]), ets:insert(T, {x, y, 1}), \
                       ets:lookup(T, 1) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{x,y,1}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/insert_new_list_keeps_existing",
            description: "insert_new/2 over a list keeps the existing object and reports false",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_inlist, [set]), ets:insert(T, {a,1}), \
                       ets:insert_new(T, [{a,9}]), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/member_empty_table_is_false",
            description: "member/2 is false on a table that has never been written",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_mem0, [set]), ets:member(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/delete_all_objects_returns_true",
            description: "delete_all_objects/1 returns true",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_dao2, [set]), ets:insert(T, {a,1}), \
                       ets:delete_all_objects(T) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/duplicate_bag_delete_object_removes_all",
            description: "delete_object/2 in a duplicate_bag removes every identical object",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_dbd, [duplicate_bag]), ets:insert(T, {a,1}), \
                       ets:insert(T, {a,1}), ets:delete_object(T, {a,1}), \
                       length(ets:lookup(T, a)) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 0"),
                exit_code: 0,
            },
        },
        // --- widened: update_counter/4 (the default-object form) and the multi-spec list ---
        Case {
            id: "ets/update_counter4_default_insert",
            description: "update_counter/4 with a default object inserts and increments it",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_ucd, [set]), \
                       ets:update_counter(T, k, 1, {k, 100}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 101"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/update_counter4_pos_spec_default",
            description: "update_counter/4 with a {Pos,Incr} spec uses the default object's element",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_ucd2, [set]), \
                       ets:update_counter(T, k, {2, 1}, {k, 0}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/update_counter4_multi_spec_list",
            description: "update_counter/3 with a list of {Pos,Incr} specs returns the new values",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_ucd3, [set]), ets:insert(T, {k, 1, 1}), \
                       ets:update_counter(T, k, [{2,1},{3,1}]) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [2,2]"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/update_counter_missing_key_is_badarg",
            description: "update_counter/3 without a default raises badarg for an absent key",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_ucbad, [set]), ets:update_counter(T, k, 1) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- widened: update_element/3 (absent key, and a position list) ---
        Case {
            id: "ets/update_element_absent_key_is_false",
            description: "update_element/3 on an absent key stores nothing and returns false",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_ueabs, [set]), ets:insert(T, {a,1}), \
                       ets:update_element(T, b, {2, 1}) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "ets/update_element_list_of_positions",
            description: "update_element/3 accepts a list of {Pos,Value} pairs",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_uelist, [set]), ets:insert(T, {a,1,2}), \
                       ets:update_element(T, a, [{2,5},{3,6}]), ets:lookup(T, a) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,5,6}]"),
                exit_code: 0,
            },
        },
        // --- widened: the rejected argument shapes ---
        Case {
            id: "ets/insert_non_tuple_is_badarg",
            description: "insert/2 with a non-tuple object raises badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin T = ets:new(ets_tab_badins, [set]), ets:insert(T, not_a_tuple) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
    ]
}
