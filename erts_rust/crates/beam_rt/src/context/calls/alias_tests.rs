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

//! The `ets` **table-alias witness**'s host half ([`RuntimeContext::call_registered`]): the
//! aliased copy-out, its **ordering** relative to the pool rewind, and its non-applicability of
//! the `graph_holds_reference` exemption.
//!
//! Every case here is written so that removing the aliased arm — or moving it after
//! `heap.reset_to(mark)`, or letting the reference exemption apply to it — makes it **fail**:
//!
//! * [`an_ets_result_survives_the_tables_own_storage_being_recycled`] reads a container, then
//!   deletes the key and inserts an **equal-size** object (which the `TermStore`'s free lists hand
//!   the same run back to) and asserts the value read earlier is unchanged. A skipped copy leaves
//!   the read value pointing at the recycled run, so it would read the *new* object.
//! * [`an_aliased_container_is_copied_before_the_pool_is_rewound`] pins the **ordering** through
//!   the copy site's own `(mark, used)` witness: `used > mark` can only be recorded while the
//!   pool words the `ets` helper built (the list spine, or a `select`'s `{Objects, Cont}` tuple)
//!   are still live — a tail that rewound first would record `used == mark`.
//! * [`an_aliased_immediate_is_never_copied_out`] pins the other side of the rule (an aliased
//!   result that names no table word is not copied) **and** that the witness really fired
//!   ([`notes::alias_marks`]), so "no copy" cannot be an artefact of an uninstalled witness.
//! * [`an_aliased_reference_result_is_copied_and_stays_identical`] is the exemption's case: the
//!   reference inside an `ets` read result is copied with the graph, and `=:=` still holds.
//!
//! The read shapes themselves are cross-checked against the system `erl` by
//! [`ets_read_shapes_agree_with_the_oracle`] (a missing toolchain is a loud skip, never a silent
//! pass). `beam_ets` exposes no `TermStore` address range, so the structural pins are the
//! counters and the `(mark, used)` witness rather than an address-membership test.

use super::*;
use crate::config::RuntimeConfig;
use crate::context::test_support::{resolve_oracle, run_oracle_probe_exact, scratch_dir};

/// A runtime with the default configuration (the `-eval` route: no process table, so the
/// aliased copy lands in the runtime's long-lived `-eval` store).
fn runtime() -> crate::runtime::Runtime {
    crate::runtime::Runtime::new(RuntimeConfig::default()).expect("the default runtime")
}

/// Evaluates `expr` and renders the result the way the oracle's `~p` would, so a case can assert
/// a whole term with one string comparison.
fn eval_printed(rt: &mut crate::runtime::Runtime, expr: &str) -> String {
    let term = rt.eval(expr).expect("the -eval program runs");
    rt.format_term(term, None)
        .expect("~p renders the result")
        .trim_end()
        .to_string()
}

/// **A read result is a deep copy: it survives the table's own storage being recycled.**
///
/// The aliased arm hands the table's `TermStore` words back to the copier, and `TermStore::free`
/// returns an object's runs to the store's **free lists**, from which a later `copy` of an
/// equal-size object takes them again. So a result that were returned **un-copied** would still
/// be *readable* — no crash, no dangling pointer the allocator notices — and would show the
/// **re-inserted** object's contents. This shape is what makes the copy load-bearing rather than
/// merely tidy.
#[test]
fn an_ets_result_survives_the_tables_own_storage_being_recycled() {
    let mut rt = runtime();
    let printed = eval_printed(
        &mut rt,
        "begin T = ets:new(alias_churn, [set]), \
         ets:insert(T, {k, {1,2,3}}), \
         L = ets:lookup(T, k), \
         ets:delete(T, k), \
         ets:insert(T, {k, {9,9,9}}), \
         L end",
    );
    assert_eq!(
        printed, "[{k,{1,2,3}}]",
        "the value read before the delete is the copy, not the table's own (now recycled) words \
         — an un-copied result would read the re-inserted `{{9,9,9}}`"
    );
}

/// **The alias witness fires and the aliased container is copied, before the pool is rewound.**
///
/// Three assertions, one per fact the tail owns:
///
/// * `marks >= 1` — the `BifContext`'s witness was **installed** and `beam_bifs`' `copy_out_list`
///   really handed back the table's roots (without the wiring this is `0`);
/// * `copies >= 1`, `refusals == 0` — the tail took the aliased branch and the copy succeeded;
/// * `used > mark` — at the instant the copier took the term, the pooled BIF heap held the words
///   the `ets` helper had built on it. `heap.reset_to(mark)` sets `used == mark`, so a tail that
///   rewound first records the opposite and this assertion fails.
#[test]
fn an_aliased_container_is_copied_before_the_pool_is_rewound() {
    let mut rt = runtime();
    notes::reset();
    let printed = eval_printed(
        &mut rt,
        "begin T = ets:new(alias_copy, [set]), \
         ets:insert(T, {k, lists:seq(1, 8)}), \
         ets:lookup(T, k) end",
    );
    assert_eq!(printed, "[{k,[1,2,3,4,5,6,7,8]}]");
    assert!(
        notes::alias_marks() >= 1,
        "the `ets` table-alias witness was installed and fired (got {} marks)",
        notes::alias_marks()
    );
    let (copies, refusals) = notes::alias_snapshot();
    assert!(copies >= 1, "the aliased container was copied out");
    assert_eq!(refusals, 0, "the copy succeeded");
    let (mark, used) = notes::alias_copy_pool();
    assert!(
        used > mark,
        "the copier saw the pool words at [{mark}..{used}) of the pooled BIF heap: the copy runs \
         BEFORE `heap.reset_to(mark)`, which would have left `used == mark`"
    );
}

/// **The mixed shape is the one the ordering exists for**: `ets:select/3` with a limit answers
/// `{Objects, Continuation}` — table-owned objects inside a tuple and continuation the helper
/// built on the **pool**. Copied before the rewind, all three parts survive.
#[test]
fn a_chunked_select_result_is_copied_out_whole() {
    let mut rt = runtime();
    notes::reset();
    let printed = eval_printed(
        &mut rt,
        "begin T = ets:new(alias_chunk, [ordered_set]), \
         ets:insert(T, [{1, one}, {2, two}, {3, three}]), \
         {Objs, Cont} = ets:select(T, [{{'$1', '$2'}, [], ['$_']}], 2), \
         {Objs, Cont =:= '$end_of_table', ets:select(Cont)} end",
    );
    assert_eq!(
        printed, "{[{1,one},{2,two}],false,{[{3,three}],'$end_of_table'}}",
        "the chunk, its continuation and the next chunk all read back intact"
    );
    let (copies, refusals) = notes::alias_snapshot();
    assert!(
        copies >= 1 && refusals == 0,
        "the `{{Objects, Cont}}` tuple was copied out (copies={copies}, refusals={refusals})"
    );
    let (mark, used) = notes::alias_copy_pool();
    assert!(used > mark, "the pool-built tuple/spine was live at the copy");
}

/// **An aliased result that names no table word is not copied** — and the witness still fired, so
/// the missing copy is the *immediate* rule, not an uninstalled witness.
///
/// `ets:lookup_element/3` of a small int and `ets:first/1` of an empty table both answer
/// immediates (`42`, `'$end_of_table'`); `beam_bifs`' `copy_out` sets the cell for them too.
#[test]
fn an_aliased_immediate_is_never_copied_out() {
    let mut rt = runtime();
    notes::reset();
    let element = eval_printed(
        &mut rt,
        "begin T = ets:new(alias_imm, [set]), ets:insert(T, {k, 42}), \
         ets:lookup_element(T, k, 2) end",
    );
    assert_eq!(element, "42");
    let first = eval_printed(
        &mut rt,
        "begin T = ets:new(alias_empty, [set]), ets:first(T) end",
    );
    assert_eq!(first, "'$end_of_table'", "an empty table's first key");
    assert!(
        notes::alias_marks() >= 1,
        "the `ets` table-alias witness was installed and fired for the element read (got {} \
         marks); an empty table's `ets:first/1` answers `'$end_of_table'` without reaching the \
         copy helper at all",
        notes::alias_marks()
    );
    let (copies, refusals) = notes::alias_snapshot();
    assert_eq!(
        (copies, refusals),
        (0, 0),
        "an aliased immediate names no table word, so nothing is copied (and nothing is refused)"
    );
}

/// **The reference-holding case: the copy applies, and identity still holds.**
///
/// The `graph_holds_reference` exemption is deliberately NOT consulted for an aliased result — its
/// words are the table's, not the pool's — so a reference inside an `ets` read result is copied
/// with the rest of the graph. `beam_term::compare::cmp_refs` orders/comps internal references by
/// their **payload words**, so the copy is `=:=` the original and this shape keeps agreeing with
/// the oracle. (`erlang:cancel_timer/1` on such a reference is a *separate*, pre-existing
/// `-eval`-route timer-store residual, recorded in `../CONTEXT.md`'s *Known Issues* — it is not
/// weakened, and not papered over, by the copy.)
#[test]
fn an_aliased_reference_result_is_copied_and_stays_identical() {
    let mut rt = runtime();
    notes::reset();
    let printed = eval_printed(
        &mut rt,
        "begin T = ets:new(alias_ref, [set]), \
         R = make_ref(), \
         ets:insert(T, {k, R}), \
         R2 = element(2, hd(ets:lookup(T, k))), \
         {R =:= R2, R2 =/= make_ref()} end",
    );
    assert_eq!(
        printed, "{true,true}",
        "the reference read back out of the table is the one that was inserted"
    );
    let (copies, refusals) = notes::alias_snapshot();
    assert!(
        copies >= 1 && refusals == 0,
        "a reference-holding aliased container IS copied (copies={copies}, refusals={refusals})"
    );
}

/// The `ets` read shapes, cross-checked against the system `erl`: the same program evaluated by
/// this runtime and by the oracle must print the same term, and mutating a returned term must not
/// reach the table.
///
/// A missing `erl` is a loud skip (never a silent pass); the probe always ends in `init stop`, so
/// it terminates.
#[test]
fn ets_read_shapes_agree_with_the_oracle() {
    let Some(oracle) = resolve_oracle() else {
        crate::context::test_support::skip_loudly(
            "ets table-alias read shapes",
            "no `erl` on $PATH and no executable $ERTS_ORACLE",
        );
        return;
    };
    let dir = scratch_dir("ets_alias_shapes");
    // One `ordered_set` so every shape's order is exact on both runtimes.
    let setup = "T = ets:new(alias_shapes, [ordered_set]), \
                 ets:insert(T, [{a, 1}, {b, [2,3]}, {c, {4,5}}]), ";
    let tails = [
        "ets:lookup(T, b)",
        "ets:lookup(T, nokey)",
        "ets:tab2list(T)",
        "ets:lookup_element(T, c, 2)",
        "ets:lookup_element(T, c, 1)",
        "{ets:first(T), ets:next(T, a), ets:prev(T, c)}",
        "lists:sort(ets:select(T, [{{'$1', '$2'}, [], [{{'$1', '$2'}}]}]))",
        "ets:select(T, [{{'$1', '$2'}, [], ['$_']}])",
    ];
    let mut rt = runtime();
    for tail in &tails {
        let ours = eval_printed(&mut rt, &format!("begin {setup}{tail} end"));
        let oracle_line = run_oracle_probe_exact(
            &oracle,
            dir.path(),
            &format!("{setup}io:format(\"~p~n\", [{tail}])"),
        );
        assert_eq!(
            oracle_line,
            vec![ours.clone()],
            "the oracle and this runtime must print the same read result for `{tail}`"
        );
    }
    // The deep-copy half: a `setelement/3` on the returned object leaves the table alone.
    let ours = eval_printed(
        &mut rt,
        &format!(
            "begin {setup}M = setelement(2, hd(ets:lookup(T, b)), [9,9,9]), {{M, ets:lookup(T, b)}} end"
        ),
    );
    let oracle_line = run_oracle_probe_exact(
        &oracle,
        dir.path(),
        &format!(
            "{setup}io:format(\"~p~n\", [{{setelement(2, hd(ets:lookup(T, b)), [9,9,9]), ets:lookup(T, b)}}])"
        ),
    );
    assert_eq!(oracle_line, vec![ours], "a mutated read term never reaches the table");
}
