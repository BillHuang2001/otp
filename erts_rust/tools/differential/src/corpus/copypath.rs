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

//! Contract **A.1** (language semantics) — the **process copy boundary's cons-chain fast path**.
//!
//! A message between two Erlang processes is *copied* into the receiving process's heap, so the
//! copy engine (`../../crates/beam_message/src/copy.rs`) owns a defining language-semantics
//! contract: the term the receiver observes is **byte-identical** to the term the sender held,
//! whatever interior the copy chooses.
//!
//! One such interior is `copy_cons_chain`: instead of one worklist round trip per cell, a whole
//! `cdr` chain is written by a single flat pass, bounded by the destination's **remaining words**
//! (`CopyTarget::remaining_words`), and a fix-up is recorded on the per-thread worklist only for a
//! child that is not shared — so off-heap/refcounted children are relocated afterwards, while the
//! immediates and the chain's own cells are laid down inline. Until this group existed no corpus
//! case reached a long cons chain through the copy boundary: the fast path was pinned only by the
//! crate's unit tests.
//!
//! Every case therefore:
//!
//! * builds the chain in the `-eval` process, **sends it to a spawned process**, and has that
//!   process **echo it back**, comparing `term_to_binary/1` of the echoed graph against
//!   `term_to_binary/1` of the original. The echo leg is not decoration: the receiver's own state
//!   is never an observable, so `P ! Payload` alone pins only that the send did not raise — a copy
//!   that dropped or corrupted the chain would still answer `ok` (see the harness `CONTEXT.md`).
//!   The echoed graph is the observable, and it has crossed the boundary **twice**;
//! * answers the boolean `term_to_binary(Echo) =:= term_to_binary(L)`, so the golden is the
//!   oracle's canonical image of `true` (an integer, an atom, a float or a nested list inside the
//!   chain cannot leak into the golden — only the equality of the two images can). A copy that
//!   lost a field answers `false` (a different line), and a message that never arrives answers
//!   `timeout` (again a different line): a failure is always **visible**, never a silent pass;
//! * keeps the payload small enough for a smoke-level corpus — `term_to_binary/1` of the largest
//!   chain (400 distinct 4 KiB reference-counted binaries, 1.6 MB of payload) stays under a few MB,
//!   and the whole group adds a couple of seconds per runtime.
//!
//! The five chains are deliberately different *interiors*:
//!
//! * a 20 000-element integer list — the plain flat pass, all immediates;
//! * a 200 000-element integer list — the chain cannot fit the receiver's remaining words, so the
//!   pass must stop at the bound and continue through the refill/chunk path;
//! * 400 conses each holding a **distinct** 4 KiB reference-counted binary — the fix-up worklist
//!   has to relocate every off-heap binary and leave its bytes intact;
//! * a 10 000-cons **improper** list whose tail is an atom — the chain walk must stop at a
//!   non-cons tail instead of reading it as a cell;
//! * 5 000 conses of `{I, I * 1.5, <<"bin">>, atom, [I, I + 1]}` — immediates, a fresh heap float
//!   and a nested cons chain interleaved in one element, so the flat pass and the fix-up path
//!   alternate.
//!
//! The probe wrapper binds `V`, `C` and `R`, so no expression may bind those names (the echoed
//! message is bound as `Echo`), and every `receive` here carries an `after` arm.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// The canonical image of `true` under [`Probe::TermToBinary`]: every case in this group answers
/// the boolean `term_to_binary(Echo) =:= term_to_binary(L)`, so `true` is the only agreeing line.
/// (The measured oracle line — see the module tests — is
/// `ok <<131,119,4,116,114,117,101>>`.)
const TRUE_GOLDEN: &str = "ok <<131,119,4,116,114,117,101>>";

/// One **A.1** case: `expr` builds a cons chain, sends it to a spawned process, echoes it back and
/// answers whether the echoed graph's canonical image equals the original's.
fn case(id: &'static str, description: &'static str, expr: &'static str) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::LanguageSemantics],
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::TermToBinary,
        },
        golden: Golden {
            stdout: Some(TRUE_GOLDEN),
            exit_code: 0,
        },
    }
}

/// The A.1 process-copy-boundary cases. Every one of these is verified `AGREE`.
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "copypath/long_int_cons_chain_round_trips",
            "a 20 000-element integer cons chain survives the send/echo copy round trip byte-identically",
            "begin L = lists:seq(1, 20000), \
             Me = self(), P = spawn(fun() -> receive M -> Me ! M end end), P ! L, \
             receive Echo -> term_to_binary(Echo) =:= term_to_binary(L) \
             after 30000 -> timeout end end",
        ),
        case(
            "copypath/cons_chain_longer_than_the_target_heap_round_trips",
            "a 200 000-element cons chain, longer than the receiver's remaining heap words, \
             survives the round trip byte-identically",
            "begin L = lists:seq(1, 200000), \
             Me = self(), P = spawn(fun() -> receive M -> Me ! M end end), P ! L, \
             receive Echo -> term_to_binary(Echo) =:= term_to_binary(L) \
             after 30000 -> timeout end end",
        ),
        case(
            "copypath/cons_chain_of_refc_binaries_round_trips",
            "400 conses of distinct 4 KiB reference-counted binaries keep every byte across the \
             send/echo round trip",
            "begin L = [<<I:32, 0:((4096-4)*8)>> || I <- lists:seq(1, 400)], \
             Me = self(), P = spawn(fun() -> receive M -> Me ! M end end), P ! L, \
             receive Echo -> term_to_binary(Echo) =:= term_to_binary(L) \
             after 30000 -> timeout end end",
        ),
        case(
            "copypath/improper_cons_chain_with_atom_tail_round_trips",
            "a 10 000-cons improper list whose tail is an atom survives the send/echo round trip \
             byte-identically",
            "begin L = lists:foldr(fun(E, Acc) -> [E | Acc] end, tail, lists:seq(1, 10000)), \
             Me = self(), P = spawn(fun() -> receive M -> Me ! M end end), P ! L, \
             receive Echo -> term_to_binary(Echo) =:= term_to_binary(L) \
             after 30000 -> timeout end end",
        ),
        case(
            "copypath/cons_chain_of_mixed_nested_elements_round_trips",
            "5 000 conses of {integer, float, binary, atom, nested list} survive the send/echo \
             round trip byte-identically",
            "begin L = [{I, I * 1.5, <<\"bin\">>, atom, [I, I + 1]} || I <- lists:seq(1, 5000)], \
             Me = self(), P = spawn(fun() -> receive M -> Me ! M end end), P ! L, \
             receive Echo -> term_to_binary(Echo) =:= term_to_binary(L) \
             after 30000 -> timeout end end",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five case ids, in the order [`group`] emits them.
    const IDS: &[&str] = &[
        "copypath/long_int_cons_chain_round_trips",
        "copypath/cons_chain_longer_than_the_target_heap_round_trips",
        "copypath/cons_chain_of_refc_binaries_round_trips",
        "copypath/improper_cons_chain_with_atom_tail_round_trips",
        "copypath/cons_chain_of_mixed_nested_elements_round_trips",
    ];

    fn case<'a>(cases: &'a [Case], id: &str) -> &'a Case {
        cases
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the copypath group"))
    }

    #[test]
    fn the_group_is_exactly_the_declared_cases() {
        let cases = group();
        assert_eq!(cases.len(), IDS.len());
        for (case, id) in cases.iter().zip(IDS) {
            assert_eq!(case.id, *id, "the group order is the declared order");
            assert!(
                case.id.starts_with("copypath/") && case.id.len() > "copypath/".len(),
                "case id {} is not copypath/<name>",
                case.id
            );
            assert!(!case.description.is_empty(), "case {} has no description", case.id);
        }
    }

    /// Every case pins the **oracle's own measured line**: the canonical image of `true`, with a
    /// clean exit. `true` is only reachable when the echoed graph's bytes equal the original's, so
    /// a dropped, truncated or corrupted copy (or a message that never arrives) cannot agree.
    #[test]
    fn every_golden_is_the_measured_oracle_line() {
        assert_eq!(TRUE_GOLDEN, "ok <<131,119,4,116,114,117,101>>");
        let cases = group();
        for id in IDS {
            let case = case(cases.as_slice(), id);
            assert_eq!(case.golden.stdout, Some(TRUE_GOLDEN), "case {id}");
            assert_eq!(case.golden.exit_code, 0, "case {id}: the oracle exits 0");
        }
    }

    #[test]
    fn every_case_is_a_language_semantics_eval_case() {
        let cases = group();
        let cases = cases.as_slice();
        for id in IDS {
            let case = case(cases, id);
            assert!(
                matches!(
                    case.kind,
                    CaseKind::Eval {
                        probe: Probe::TermToBinary,
                        ..
                    }
                ),
                "case {id} must be an Eval case with the canonical term_to_binary probe"
            );
            assert!(case.extra_argv().is_empty(), "case {id} takes no extra argv");
            assert!(case.env().is_empty(), "case {id} sets no environment");
            assert_eq!(case.contracts, &[Contract::LanguageSemantics], "case {id} evidences A.1");
        }
    }

    /// The shape that makes the group pin the copy boundary at all: `expr` must **spawn** a
    /// process, **send** the chain to it, **echo** the received term back, compare the two
    /// canonical images, and do all of it under a **bounded** `receive` — a non-delivery must
    /// answer `timeout` (a visible line), never block the case.
    #[test]
    fn every_expression_echoes_a_spawned_process_under_a_bounded_receive() {
        for case in group() {
            let expr = case.expr().expect("an Eval case carries an expr");
            assert!(
                expr.contains("spawn(fun() -> receive M -> Me ! M end end)"),
                "case {} must echo the message back from a spawned process: {expr:?}",
                case.id
            );
            assert!(expr.contains("P ! L"), "case {} must send the chain: {expr:?}", case.id);
            assert!(
                expr.contains("receive Echo -> term_to_binary(Echo) =:= term_to_binary(L)"),
                "case {} must compare the echoed graph with the original: {expr:?}",
                case.id
            );
            assert_eq!(
                expr.matches("term_to_binary(").count(),
                2,
                "case {}: the two canonical images are the only encodings: {expr:?}",
                case.id
            );
            assert!(
                expr.contains(" after 30000 -> timeout end"),
                "case {} must bound its receive so a non-delivery is visible: {expr:?}",
                case.id
            );
            assert!(
                !expr.contains('\n'),
                "case {}: the expression is one line, as the -eval route requires: {expr:?}",
                case.id
            );
        }
    }

    /// The probe wrapper binds `V`, `C` and `R` (`try (EXPR) of V -> … catch C:R -> …`), so an
    /// expression that binds one of those names makes the **oracle** fail to compile the wrapped
    /// probe — a guaranteed, silent false mismatch.
    #[test]
    fn no_expression_binds_a_probe_wrapper_variable() {
        for case in group() {
            let expr = case.expr().expect("an Eval case carries an expr");
            let names: Vec<&str> = expr
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .filter(|token| !token.is_empty())
                .collect();
            for bound in ["V", "C", "R"] {
                assert!(
                    !names.contains(&bound),
                    "case {} binds `{bound}`, which the probe wrapper already binds",
                    case.id
                );
            }
        }
    }

    /// Each case carries the payload its id and description promise, so the group cannot silently
    /// degenerate into five copies of one shape (and a shape cannot drift without this failing).
    #[test]
    fn each_case_carries_its_declared_payload() {
        let cases = group();
        let cases = cases.as_slice();
        for (id, marker) in [
            ("copypath/long_int_cons_chain_round_trips", "lists:seq(1, 20000)"),
            (
                "copypath/cons_chain_longer_than_the_target_heap_round_trips",
                "lists:seq(1, 200000)",
            ),
            (
                "copypath/cons_chain_of_refc_binaries_round_trips",
                "<<I:32, 0:((4096-4)*8)>>",
            ),
            (
                "copypath/improper_cons_chain_with_atom_tail_round_trips",
                "lists:foldr(fun(E, Acc) -> [E | Acc] end, tail, lists:seq(1, 10000))",
            ),
            (
                "copypath/cons_chain_of_mixed_nested_elements_round_trips",
                "{I, I * 1.5, <<\"bin\">>, atom, [I, I + 1]}",
            ),
        ] {
            let expr = case(cases, id).expr().expect("an Eval case carries an expr");
            assert!(expr.contains(marker), "case {id} must carry {marker:?}: {expr:?}");
        }
        // The five shapes are distinct, so no case is a renamed duplicate of another.
        let mut exprs: Vec<&str> = cases.iter().filter_map(Case::expr).collect();
        assert_eq!(exprs.len(), IDS.len());
        exprs.sort_unstable();
        let before = exprs.len();
        exprs.dedup();
        assert_eq!(exprs.len(), before, "two cases carry the same expression");
    }

    /// A repeated expression would make this group look like coverage while pinning another
    /// group's shape a second time, so no case here may duplicate an expression elsewhere in the
    /// corpus (and none of the sibling groups pins a cons chain through the copy boundary).
    #[test]
    fn no_expression_duplicates_another_corpus_case() {
        let mine: Vec<&str> = group().iter().filter_map(Case::expr).collect();
        for other in crate::corpus::cases() {
            if other.id.starts_with("copypath/") {
                continue;
            }
            if let Some(expr) = other.expr() {
                assert!(
                    !mine.contains(&expr),
                    "case {} duplicates an expression of this group",
                    other.id
                );
            }
        }
    }
}
