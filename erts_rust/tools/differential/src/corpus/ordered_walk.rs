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

//! Contract **A.1** — the Erlang *term-order* walk contract over the `ordered_set` interior
//! (`ets:first/1`/`ets:next/2` and `ets:last/1`/`ets:prev/2` across the `ORDER` = 32 key
//! B-tree node boundary, including the `'$end_of_table'` terminal and the absent-key anchor).
//!
//! The reference `ordered_set` is a `ca` B-tree whose nodes hold `ORDER = 32` keys, so a table
//! with more than `2 * ORDER = 64` keys spans several levels; these cases use 100 keys, so a
//! walk crosses *and* descends splits, and the mutations are applied at keys that sit on their
//! own node boundary (key `5`/`96` mid-walk; keys `33`/`32` at the first split).
//!
//! The pinned contract is the documented **term order** (`number < atom < reference < fun <
//! port < pid < tuple < map < nil < list < bitstring`), not the interior shape:
//!
//! * a forward/backward walk visits every surviving key exactly once, in term order, even when
//!   keys are deleted and inserted while the walk is in flight (`ets:next/2`/`ets:prev/2` are
//!   documented to be safe under concurrent modification — a key inserted below the cursor is
//!   reached, one already passed over is not);
//! * an **absent** key acts purely as a *range anchor*: `ets:next/2` yields the smallest present
//!   key greater than it and `ets:prev/2` the largest present key smaller than it, on either
//!   side of a split and for both an integer and a different term type (an atom sorts above
//!   every integer, so it clamps to the table's ends);
//! * the terminal sentinel of both directions is the atom `'$end_of_table'`.
//!
//! Every table name is fresh per case (`ordered_walk_*`) and each case is a single
//! self-terminating expression, so a fresh VM per case owns its own table.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.1 `ordered_set` term-order walk cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "ordered_walk/ascending_chain_crosses_a_split_under_mutation",
            description: "ets:first/1 + ets:next/2 over a >64-key ordered_set whose keys are \
                          deleted and inserted mid-walk visits the survivors once, ascending",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                // 100 keys = >2*ORDER, so the walk crosses a node split. At key 5 (before the
                // deletions are reached) the not-yet-visited keys 50..59 are deleted and the new
                // keys 200..204 are inserted: the deleted range must be skipped, and the inserted
                // keys — below no cursor yet passed — must still be visited after 100.
                expr: "begin T = ets:new(ordered_walk_asc, [ordered_set]), \
                       ets:insert(T, [{I,I} || I <- lists:seq(1,100)]), \
                       Step = fun(Rec, K, Acc) -> case K of \
                           '$end_of_table' -> lists:reverse(Acc); \
                           _ -> _ = case K of \
                               5 -> [ets:delete(T, X) || X <- lists:seq(50,59)], \
                                    ets:insert(T, [{J,J} || J <- lists:seq(200,204)]); \
                               _ -> ok end, \
                                Rec(Rec, ets:next(T, K), [K|Acc]) end end, \
                       Visited = Step(Step, ets:first(T), []), \
                       {length(Visited), Visited} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {95,[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,\
                     27,28,29,30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,60,61,\
                     62,63,64,65,66,67,68,69,70,71,72,73,74,75,76,77,78,79,80,81,82,83,84,85,86,\
                     87,88,89,90,91,92,93,94,95,96,97,98,99,100,200,201,202,203,204]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "ordered_walk/descending_chain_crosses_a_split_under_mutation",
            description: "ets:last/1 + ets:prev/2 over a >64-key ordered_set whose keys are \
                          deleted and inserted mid-walk visits the survivors once, descending",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                // The mirror image: walking down from 100, at key 96 the not-yet-visited keys
                // 52..61 are deleted and -5..-1 are inserted — below the cursor, so they must be
                // visited after key 1, in descending order.
                expr: "begin T = ets:new(ordered_walk_desc, [ordered_set]), \
                       ets:insert(T, [{I,I} || I <- lists:seq(1,100)]), \
                       Step = fun(Rec, K, Acc) -> case K of \
                           '$end_of_table' -> lists:reverse(Acc); \
                           _ -> _ = case K of \
                               96 -> [ets:delete(T, X) || X <- lists:seq(52,61)], \
                                     ets:insert(T, [{J,J} || J <- lists:seq(-5,-1)]); \
                               _ -> ok end, \
                                Rec(Rec, ets:prev(T, K), [K|Acc]) end end, \
                       Visited = Step(Step, ets:last(T), []), \
                       {length(Visited), Visited} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {95,[100,99,98,97,96,95,94,93,92,91,90,89,88,87,86,85,84,83,82,81,80,79,78,\
                     77,76,75,74,73,72,71,70,69,68,67,66,65,64,63,62,51,50,49,48,47,46,45,44,43,\
                     42,41,40,39,38,37,36,35,34,33,32,31,30,29,28,27,26,25,24,23,22,21,20,19,18,\
                     17,16,15,14,13,12,11,10,9,8,7,6,5,4,3,2,1,-1,-2,-3,-4,-5]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "ordered_walk/absent_key_anchor_is_a_range_bound_across_the_split",
            description: "an absent key anchors ets:next/2 / ets:prev/2 to the neighbouring present \
                          keys on both sides of the split, and clamps to '$end_of_table' at the ends",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                // Keys 10, 33 and 70 are deleted: 10 and 70 lie in different nodes, and 33 is
                // exactly the first key of the node after the `ORDER = 32` split. Each must behave
                // as a pure range bound. `Edges` pins the four extreme/outside anchors (0 and
                // -1000 below the minimum, 101 and 1000 above the maximum, plus the two present
                // boundary keys 100 and 1), `Other` pins a non-integer (atom) anchor — atoms sort
                // above every integer, so it clamps to the top — and `Present` pins that a present
                // key still yields its neighbours, not itself.
                expr: "begin T = ets:new(ordered_walk_anchor, [ordered_set]), \
                       ets:insert(T, [{I,I} || I <- lists:seq(1,100)]), \
                       [ets:delete(T, D) || D <- [10,33,70]], \
                       Abs = [{ets:next(T, K), ets:prev(T, K)} || K <- [10,33,70]], \
                       Edges = {ets:next(T, 0), ets:prev(T, 101), ets:next(T, 1000), \
                                ets:prev(T, -1000), ets:next(T, 100), ets:prev(T, 1)}, \
                       Other = {ets:next(T, zz), ets:prev(T, zz)}, \
                       Present = {ets:next(T, 50), ets:prev(T, 50), ets:next(T, 32), \
                                  ets:prev(T, 34)}, \
                       {Abs, Edges, Other, Present} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {[{11,9},{34,32},{71,69}],\
                     {1,100,'$end_of_table','$end_of_table','$end_of_table','$end_of_table'},\
                     {'$end_of_table',100},{51,49,34,32}}",
                ),
                exit_code: 0,
            },
        },
    ]
}
