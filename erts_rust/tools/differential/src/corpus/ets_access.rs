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

//! Contract **A.7** — the ETS access-control matrix (`public` / `protected` / `private`
//! × owner / non-owner), observed from Erlang.
//!
//! A spawned helper process is used as the **non-owner**: it performs the access and
//! reports the outcome back over a mailbox (`(the {res, X} pattern)`), because a table is
//! owned by the process that created it, so a same-process access cannot exercise the
//! owner/non-owner distinction. The helper blocks in its `receive` after replying (the
//! table is owned by the main process, so its lifetime is unaffected) and the matrix cell
//! is asserted on the reply.
//!
//! # `ets:info/1,2` is deliberately ungated
//!
//! Unlike the read/write operations, the documented `info` surface is **not** subject to
//! the access mode — a non-owner may inspect a `private` table's metadata. That is pinned
//! here (as a shape: the size is an integer, the `info/1` result is a proplist).
//!
//! # The *denied* cells pin only the refusal, never the reason's shape
//!
//! The Rust runtime enforces the access mode both at `ets:new/2` (so `ets:info(T, protection)`
//! agrees) and on a later access by a non-owner: a non-owner `ets:lookup/2` of a `private`
//! table and a non-owner `ets:insert/2` of a `protected`/`private` table both raise. The
//! **reason** differs though — the oracle raises a stacktrace-carrying `error:{badarg,
//! Stacktrace}`, the Rust runtime a bare `error:badarg` — and the stacktrace frames are
//! volatile, so the denial half of the matrix is a handoff to `beam_ets`/`beam_bifs` rather
//! than a pinned case. The allowed half is covered end to end (owner + non-owner reads and
//! non-owner writes) so a regression in the enforcing path shows up as a failure.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One A.7 ETS-access case: evaluate `expr` under both runtimes, pin the oracle's rendering.
fn case(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::OtpApps],
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.7 ETS access-control cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- the owner reading its own (empty) tables: every mode admits the owner --------
        case(
            "ets_access/owner_reads_private_table",
            "the owner's ets:lookup/2 of its own private table is [] (an empty table)",
            "begin T = ets:new(ets_acc_owner_priv, [private]), Res = ets:lookup(T, k), \
             ets:delete(T), Res end",
            "ok []",
        ),
        case(
            "ets_access/owner_reads_protected_table",
            "the owner's ets:lookup/2 of its own protected table is [] (an empty table)",
            "begin T = ets:new(ets_acc_owner_prot, [protected]), Res = ets:lookup(T, k), \
             ets:delete(T), Res end",
            "ok []",
        ),
        case(
            "ets_access/owner_reads_public_table",
            "the owner's ets:lookup/2 of its own public table is [] (an empty table)",
            "begin T = ets:new(ets_acc_owner_pub, [public]), Res = ets:lookup(T, k), \
             ets:delete(T), Res end",
            "ok []",
        ),
        // --- the owner writing its own tables: every mode admits the owner ----------------
        case(
            "ets_access/owner_writes_private_table",
            "the owner's ets:insert/2 + lookup of a private table round trips",
            "begin T = ets:new(ets_acc_ow_priv, [private]), true = ets:insert(T, {k, 1}), \
             Res = ets:lookup(T, k), ets:delete(T), Res end",
            "ok [{k,1}]",
        ),
        case(
            "ets_access/owner_writes_protected_table",
            "the owner's ets:insert/2 + lookup of a protected table round trips",
            "begin T = ets:new(ets_acc_ow_prot, [protected]), true = ets:insert(T, {k, 1}), \
             Res = ets:lookup(T, k), ets:delete(T), Res end",
            "ok [{k,1}]",
        ),
        case(
            "ets_access/owner_writes_public_table",
            "the owner's ets:insert/2 + lookup of a public table round trips",
            "begin T = ets:new(ets_acc_ow_pub, [public]), true = ets:insert(T, {k, 1}), \
             Res = ets:lookup(T, k), ets:delete(T), Res end",
            "ok [{k,1}]",
        ),
        // --- a NON-owner reading: `public` and `protected` admit any reader ---------------
        case(
            "ets_access/non_owner_reads_public_table",
            "a non-owner's ets:lookup/2 of a public table sees the owner's entry",
            "begin T = ets:new(ets_acc_nr_pub, [public]), ets:insert(T, {k, 1}), \
             H = spawn(fun() -> receive {go, From} -> \
                 From ! {res, ets:lookup(T, k)} end end), \
             H ! {go, self()}, \
             X = receive {res, Z} -> Z after 2000 -> timeout end, \
             ets:delete(T), X end",
            "ok [{k,1}]",
        ),
        case(
            "ets_access/non_owner_reads_protected_table",
            "a non-owner's ets:lookup/2 of a protected table sees the owner's entry",
            "begin T = ets:new(ets_acc_nr_prot, [protected]), ets:insert(T, {k, 1}), \
             H = spawn(fun() -> receive {go, From} -> \
                 From ! {res, ets:lookup(T, k)} end end), \
             H ! {go, self()}, \
             X = receive {res, Z} -> Z after 2000 -> timeout end, \
             ets:delete(T), X end",
            "ok [{k,1}]",
        ),
        // --- a NON-owner writing: `public` admits any writer -----------------------------
        case(
            "ets_access/non_owner_writes_public_table",
            "a non-owner's ets:insert/2 of a public table succeeds and is visible to the owner",
            "begin T = ets:new(ets_acc_nw_pub, [public]), \
             H = spawn(fun() -> receive {go, From} -> \
                 Out = try ets:insert(T, {k, 2}) \
                     catch Cls:Reason -> {Cls, Reason} end, \
                 From ! {res, Out} end end), \
             H ! {go, self()}, \
             X = receive {res, Z} -> Z after 2000 -> timeout end, \
             Y = ets:lookup(T, k), ets:delete(T), {X, Y} end",
            "ok {true,[{k,2}]}",
        ),
        // --- ets:info/1,2 is deliberately ungated ----------------------------------------
        case(
            "ets_access/non_owner_info_size_is_integer",
            "a non-owner's ets:info(T, size) of a private table is allowed (an integer)",
            "begin T = ets:new(ets_acc_info_size, [private]), \
             H = spawn(fun() -> receive {go, From} -> \
                 From ! {res, is_integer(ets:info(T, size))} end end), \
             H ! {go, self()}, \
             X = receive {res, Z} -> Z after 2000 -> timeout end, \
             ets:delete(T), X end",
            "ok true",
        ),
        case(
            "ets_access/non_owner_info_is_a_proplist",
            "a non-owner's ets:info/1 of a private table is allowed (a proplist)",
            "begin T = ets:new(ets_acc_info_all, [private]), \
             H = spawn(fun() -> receive {go, From} -> \
                 From ! {res, is_list(ets:info(T)) andalso is_integer(ets:info(T, size))} \
                 end end), \
             H ! {go, self()}, \
             X = receive {res, Z} -> Z after 2000 -> timeout end, \
             ets:delete(T), X end",
            "ok true",
        ),
        // --- the configured mode is observable -------------------------------------------
        case(
            "ets_access/protection_reflects_the_creation_option",
            "ets:info/2 reports the protection mode each table was created with",
            "begin T1 = ets:new(ets_acc_a, [private]), T2 = ets:new(ets_acc_b, [protected]), \
             T3 = ets:new(ets_acc_c, [public]), \
             Res = {ets:info(T1, protection), ets:info(T2, protection), \
             ets:info(T3, protection)}, \
             ets:delete(T1), ets:delete(T2), ets:delete(T3), Res end",
            "ok {private,protected,public}",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner-only cells (a table read/written by its creating process).
    const OWNER_CASES: &[&str] = &[
        "ets_access/owner_reads_private_table",
        "ets_access/owner_reads_protected_table",
        "ets_access/owner_reads_public_table",
        "ets_access/owner_writes_private_table",
        "ets_access/owner_writes_protected_table",
        "ets_access/owner_writes_public_table",
        "ets_access/protection_reflects_the_creation_option",
    ];

    /// The cells probed from a spawned non-owner process.
    const NON_OWNER_CASES: &[&str] = &[
        "ets_access/non_owner_reads_public_table",
        "ets_access/non_owner_reads_protected_table",
        "ets_access/non_owner_writes_public_table",
        "ets_access/non_owner_info_size_is_integer",
        "ets_access/non_owner_info_is_a_proplist",
    ];

    fn case<'a>(cases: &'a [Case], id: &str) -> &'a Case {
        cases
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the ets_access group"))
    }

    #[test]
    fn every_case_is_a_plain_a7_eval_case_with_no_extra_argv() {
        let cases = group();
        let cases = cases.as_slice();
        for id in OWNER_CASES.iter().chain(NON_OWNER_CASES) {
            let case = case(cases, id);
            assert_eq!(case.contracts, &[Contract::OtpApps], "case {id}");
            assert!(
                matches!(case.kind, CaseKind::Eval { .. }),
                "case {id} must be an Eval case"
            );
            assert!(case.extra_argv().is_empty(), "case {id} takes no extra argv");
            assert!(case.env().is_empty(), "case {id} sets no environment");
        }
        assert_eq!(cases.len(), OWNER_CASES.len() + NON_OWNER_CASES.len());
    }

    #[test]
    fn the_non_owner_cases_use_a_spawned_helper_and_a_mailbox_round_trip() {
        let cases = group();
        let cases = cases.as_slice();
        for id in NON_OWNER_CASES {
            let expr = case(cases, id).expr().expect("an Eval case has an expr");
            assert!(
                expr.contains("spawn(fun() -> receive {go, From} ->"),
                "case {id}: the non-owner must be a spawned helper: {expr:?}"
            );
            assert!(
                expr.contains("From ! {res, ") && expr.contains("receive {res, Z} ->"),
                "case {id}: the helper must report over a mailbox: {expr:?}"
            );
            assert!(
                expr.contains("after 2000 -> timeout end"),
                "case {id}: the mailbox round trip must be bounded: {expr:?}"
            );
        }
    }

    #[test]
    fn the_cases_never_pin_a_table_identifier_or_a_pid() {
        for case in group() {
            let golden = case.golden.stdout.expect("every case asserts stdout");
            assert!(
                !golden.contains("<0.") && !golden.contains("#Ref<"),
                "case {}: the golden pins a volatile identifier: {golden:?}",
                case.id
            );
            assert!(
                !golden.contains("ets_acc"),
                "case {}: the golden pins a table name: {golden:?}",
                case.id
            );
        }
    }

    #[test]
    fn the_matrix_covers_all_three_access_modes_and_both_read_and_write() {
        let cases = group();
        let cases = cases.as_slice();
        let exprs: String = cases
            .iter()
            .filter_map(|c| c.expr())
            .collect::<Vec<_>>()
            .join("\n");
        for needle in [
            "[private]",
            "[protected]",
            "[public]",
            "ets:lookup(T, k)",
            "ets:insert(T, {k, 2})",
            "ets:info(T1, protection)",
            "ets:info(T, size)",
        ] {
            assert!(exprs.contains(needle), "the matrix never exercises {needle:?}");
        }
    }
}
