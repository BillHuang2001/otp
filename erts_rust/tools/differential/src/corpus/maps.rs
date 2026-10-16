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

//! Contract **A.7** — the `maps` fun-taking BIFs over a map *or* a `maps` iterator.
//!
//! `maps:fold/3`, `maps:map/2`, `maps:filter/2` and `maps:foreach/2` all take a
//! `MapOrIter` argument: either a map, or a `maps` iterator (`maps:iterator/1,2`).
//! Their second/third argument is documented as `MapOrIter`, and the same walk must
//! answer identically for the two shapes.
//!
//! Three rules the cases must respect (all oracle behaviours, not gaps):
//!
//! * the **default** iterator (`maps:iterator/1`, the `[0 | Map]` form) walks a map's
//!   *internal* order, and so does a bare map walk — both are unspecified, so a case
//!   pins them only through `lists:sort/1` (the sorted pair list) and through `=:=`
//!   identities, never in walk order;
//! * a multi-key map's `~w` rendering and its `term_to_binary/1` image are equally
//!   order-unstable, so a produced map is classified into its **sorted** pair list
//!   (`maps:to_list/1` + `lists:sort/1`) before it is pinned;
//! * `maps:iterator/2`'s custom order is a **2-arity comparison fun**
//!   `fun((Key, Key) -> boolean())` (the `lists:sort/2` comparator), *not* the
//!   key-returning shape `fun({Key, Value}) -> term()` — the latter is the oracle's
//!   plain `badarg` (`maps.erl:1070-1073`), and so is a wrong-arity fun for any of the
//!   four BIFs.
//!
//! The walk of an *ordered* iterator (`ordered` / `reversed` / a custom comparator) is
//! fully deterministic (it is the term order of a sorted key list), so those results are
//! pinned in walk order.
//!
//! Case 3 pins the structured `error:{badmap, MapOrIter}` contract: an argument that is
//! neither a map nor a walkable iterator is reported against the **original** argument,
//! while a bad *fun* stays the plain `error:badarg`.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.7 `maps` fun-taking `MapOrIter` cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "maps/fun_bifs_walk_an_iterator_and_a_map_alike",
            description: "fold/map/filter/foreach answer identically over a map and over each \
                          maps:iterator/2 order, plus maps:next/1's look-ahead and terminal none",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `~w` of the default-iterator walk and of a bare map walk is unspecified, so
                // `FoldMap`/`FoldIter`/`ForeachMap`/`ForeachIter` are pinned **sorted**, and the
                // map-valued results are classified into their sorted pair list. `FoldOrd`,
                // `FoldRev` and `FoldCust` walk an ordered iterator, whose order is deterministic.
                //
                // `M = #{a => 3, b => 1, c => 2}`: `ordered` ascends by key, `reversed` descends,
                // and the custom comparator orders by the *value* (b=1, c=2, a=3) — a different
                // order from both, so the custom-order route is really exercised.
                expr: "begin M = #{a => 3, b => 1, c => 2}, SF = fun(L) -> lists:sort(L) end, \
                       F = fun(K,V,A) -> [{K,V}|A] end, \
                       Pair = fun(Mp) -> SF(maps:to_list(Mp)) end, \
                       FoldMap = SF(maps:fold(F, [], M)), \
                       FoldIter = SF(maps:fold(F, [], maps:iterator(M))), \
                       FoldOrd = maps:fold(F, [], maps:iterator(M, ordered)), \
                       FoldRev = maps:fold(F, [], maps:iterator(M, reversed)), \
                       FoldCust = maps:fold(F, [], \
                           maps:iterator(M, fun(A,B) -> maps:get(A,M) =< maps:get(B,M) end)), \
                       MapMap = Pair(maps:map(fun(_K,V) -> V*2 end, M)), \
                       MapIter = Pair(maps:map(fun(_K,V) -> V*2 end, maps:iterator(M))), \
                       FiltMap = Pair(maps:filter(fun(_K,V) -> V > 1 end, M)), \
                       FiltIter = Pair(maps:filter(fun(_K,V) -> V > 1 end, maps:iterator(M))), \
                       ForeachMap = begin put(maps_fun_bif_acc, []), \
                           maps:foreach(fun(K,V) -> \
                               put(maps_fun_bif_acc, [{K,V}|get(maps_fun_bif_acc)]) end, M), \
                           SF(get(maps_fun_bif_acc)) end, \
                       ForeachIter = begin put(maps_fun_bif_acc, []), \
                           maps:foreach(fun(K,V) -> \
                               put(maps_fun_bif_acc, [{K,V}|get(maps_fun_bif_acc)]) end, \
                               maps:iterator(M)), \
                           SF(get(maps_fun_bif_acc)) end, \
                       {NK1,NV1,I1} = maps:next(maps:iterator(M, ordered)), \
                       {NK2,NV2,I2} = maps:next(I1), \
                       {NK3,NV3,I3} = maps:next(I2), \
                       NextTerm = maps:next(I3), \
                       {FoldMap, FoldIter =:= FoldMap, FoldOrd, FoldRev, FoldCust, MapMap, \
                        MapIter =:= MapMap, FiltMap, FiltIter =:= FiltMap, ForeachMap, \
                        ForeachIter =:= ForeachMap, {NK1,NV1,NK2,NV2,NK3,NV3}, NextTerm} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {[{a,3},{b,1},{c,2}],true,[{c,2},{b,1},{a,3}],[{a,3},{b,1},{c,2}],\
                     [{a,3},{c,2},{b,1}],[{a,6},{b,2},{c,4}],true,[{a,3},{c,2}],true,\
                     [{a,3},{b,1},{c,2}],true,{a,3,b,1,c,2},none}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/iterator_walk_of_a_hamt",
            description: "a >32-key HAMT walks to its terminal none via repeated maps:next/1, and \
                          its ordered/reversed walks agree with the sorted key list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // 40 keys crosses the flat→HAMT `MAP_SMALL_MAP_LIMIT` chunk boundary, so the
                // `maps:next/1` chain is chunked and the walk must continue through the tail.
                // `Last/2` returns the iterator a fully drained walk terminates on — the atom
                // `none` — so the terminal sentinel is pinned explicitly rather than inferred
                // from the count.
                expr: "begin M = maps:from_list([{I,I} || I <- lists:seq(1,40)]), \
                       Step = fun(Rec, I, Acc) -> case maps:next(I) of \
                           none -> lists:reverse(Acc); \
                           {K,V,I2} -> Rec(Rec, I2, [{K,V}|Acc]) end end, \
                       Last = fun(Rec, I) -> case maps:next(I) of \
                           none -> I; {_,_,I2} -> Rec(Rec, I2) end end, \
                       L = Step(Step, maps:iterator(M), []), \
                       LO = Step(Step, maps:iterator(M, ordered), []), \
                       LR = Step(Step, maps:iterator(M, reversed), []), \
                       Tail = Last(Last, maps:iterator(M)), \
                       FoldC = maps:fold(fun(_K,_V,A) -> A + 1 end, 0, maps:iterator(M)), \
                       {maps:size(M), length(L), FoldC, lists:sort([K || {K,_} <- L]), \
                        lists:sort([V || {_,V} <- L]), [K || {K,_} <- LO], \
                        LR =:= lists:reverse(LO), Tail} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {40,40,40,[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,\
                     25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40],\
                     [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,\
                     29,30,31,32,33,34,35,36,37,38,39,40],\
                     [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,\
                     29,30,31,32,33,34,35,36,37,38,39,40],true,none}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/fun_bif_bad_map_or_iter_is_badmap",
            description: "a non-map, non-iterator MapOrIter argument is error:{badmap, Arg} for \
                          the four fun-taking BIFs, while a bad fun stays error:badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Every shape tried in the `MapOrIter` position — `5`, `[]`, the improper list
                // `[foo|Map]` and `[[a|b]|Map]` — is reported as `{badmap, <original argument>}`,
                // never as an unwrapped `badarg`, and never against a sub-iterator. The last row
                // shows the other half of the contract: a wrong-arity fun is the plain `badarg`.
                expr: "begin M = #{a => 1}, FoldF = fun(_,_,Acc) -> Acc end, \
                       MapF = fun(_K,V) -> V end, FiltF = fun(_K,V) -> V > 0 end, \
                       ForeF = fun(_K,_V) -> ok end, \
                       A1 = try maps:fold(FoldF, 0, 5) catch error:X1 -> X1 end, \
                       A2 = try maps:map(MapF, 5) catch error:X2 -> X2 end, \
                       A3 = try maps:filter(FiltF, 5) catch error:X3 -> X3 end, \
                       A4 = try maps:foreach(ForeF, 5) catch error:X4 -> X4 end, \
                       B1 = try maps:fold(FoldF, 0, []) catch error:X5 -> X5 end, \
                       B2 = try maps:map(MapF, []) catch error:X6 -> X6 end, \
                       B3 = try maps:filter(FiltF, []) catch error:X7 -> X7 end, \
                       B4 = try maps:foreach(ForeF, []) catch error:X8 -> X8 end, \
                       C1 = try maps:fold(FoldF, 0, [foo|M]) catch error:X9 -> X9 end, \
                       C2 = try maps:map(MapF, [foo|M]) catch error:X10 -> X10 end, \
                       C3 = try maps:filter(FiltF, [foo|M]) catch error:X11 -> X11 end, \
                       C4 = try maps:foreach(ForeF, [foo|M]) catch error:X12 -> X12 end, \
                       D1 = try maps:fold(FoldF, 0, [[a|b]|M]) catch error:X13 -> X13 end, \
                       D2 = try maps:map(MapF, [[a|b]|M]) catch error:X14 -> X14 end, \
                       D3 = try maps:filter(FiltF, [[a|b]|M]) catch error:X15 -> X15 end, \
                       D4 = try maps:foreach(ForeF, [[a|b]|M]) catch error:X16 -> X16 end, \
                       E1 = try maps:fold(fun(X) -> X end, 0, #{a => 1}) \
                           catch error:X17 -> X17 end, \
                       E2 = try maps:map(fun(X) -> X end, #{a => 1}) catch error:X18 -> X18 end, \
                       E3 = try maps:filter(fun(X) -> X end, #{a => 1}) catch error:X19 -> X19 end, \
                       E4 = try maps:foreach(fun(X) -> X end, #{a => 1}) catch error:X20 -> X20 end, \
                       {{A1,A2,A3,A4},{B1,B2,B3,B4},{C1,C2,C3,C4},{D1,D2,D3,D4},{E1,E2,E3,E4}} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {{{badmap,5},{badmap,5},{badmap,5},{badmap,5}},\
                     {{badmap,[]},{badmap,[]},{badmap,[]},{badmap,[]}},\
                     {{badmap,[foo|#{a => 1}]},{badmap,[foo|#{a => 1}]},\
                     {badmap,[foo|#{a => 1}]},{badmap,[foo|#{a => 1}]}},\
                     {{badmap,[[a|b]|#{a => 1}]},{badmap,[[a|b]|#{a => 1}]},\
                     {badmap,[[a|b]|#{a => 1}]},{badmap,[[a|b]|#{a => 1}]}},\
                     {badarg,badarg,badarg,badarg}}",
                ),
                exit_code: 0,
            },
        },
    ]
}
