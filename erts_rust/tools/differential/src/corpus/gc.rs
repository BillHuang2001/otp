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

//! Contracts **A.1** (language semantics) / **A.7** (BIF surface) — the per-process
//! **garbage collector** observed through the values it must preserve.
//!
//! The collector is an internal component: nothing about it is directly observable, and the
//! quantities it exposes (`erlang:process_info/2`'s byte totals, `minor_gcs`, the heap-size
//! limits) are *per-runtime internal counters*, not oracle values — the oracle answers a
//! different number on every VM start. What **is** oracle-observable is the collector's
//! defining contract: it must be **value-transparent**. These cases therefore:
//!
//! * build a term, force a collection with `erlang:garbage_collect/1` (or stress the
//!   allocator until the runtime collects on its own), then observe the **surviving value**
//!   (`lists:sum/1`, `hd/1`, a `=:=` against a reconstructed value, `element/2`, the canonical
//!   `term_to_binary/1` image) — a collector that dropped, duplicated or corrupted a live term
//!   answers a different value, which is exactly the kind of divergence this group exists to
//!   catch;
//! * pin the **shape** of the `erlang:process_info/2` fields the collector owns
//!   (`memory`'s integer, `garbage_collection`'s documented key set) — never their values;
//! * pin the return value (`true`) of every `erlang:garbage_collect` arity — `/0`, `/1` and the
//!   `/2` option form (`[]`, `{type, minor}`, `{type, major}`) — and the `/2` form's
//!   `error:badarg` refusal of an unknown `type` — contract A.7.
//!
//! Every golden is the oracle's **single-line** stdout under the default [`Probe::TermToBinary`]
//! (the canonical term image, so an atom-keyed map's `~w` key order cannot leak into a golden).
//! The module-run cases (`gc/module_*`) drive the same properties through **compiled bytecode**
//! and the runtime's `run` action, where allocation and collection are driven by the engine
//! rather than by the `-eval` tree-walker; a module-run golden must be an integer.
//!
//! # Deliberately NOT pinned here
//!
//! See `GC_COMPARABILITY.md` next to this crate for the full rationale. In short: the
//! `process_info(self(), memory)` **byte total**, the `garbage_collection` **counters/limits**
//! (`minor_gcs`, `min_heap_size`, `fullsweep_after`, …) and the `max_heap_size` **map image**
//! (its atom keys encode in a different order in the Rust runtime — a genuine divergence, not
//! a gap) are internal per-runtime quantities, and `erlang:process_info/2`'s `binary` item is
//! an honest Rust capability gap (exit `3`) that must not enter the corpus until it agrees.
use super::{Case, CaseKind, Contract, Golden, ModuleRun, Probe};

/// The embedded module compiled for the module-run cases.
///
/// Every function returns an **integer** (the shape a module-run golden pins), forces a
/// collection with `erlang:garbage_collect(self())` and returns an observation of the
/// surviving term, so the golden is stable in both runtimes:
///
/// * `churn/1` — allocate and drop a fresh 50-element list per round, so the collector runs
///   many times before the accumulated figure is returned;
/// * `survive_sum/0` — a 10 000-element list's sum across a forced collection;
/// * `rebuild_equal/1` — `1` iff a built range still compares equal to a rebuilt one;
/// * `deep_shape/0` — `1` iff a nested tuple/list/binary still compares equal to its literal;
/// * `big_sum/0` — a 200 000-element range (a bignum result) across a forced collection.
const DIFF_GC_SOURCE: &str = "-module(diff_gc).\n\
     -export([churn/1, survive_sum/0, rebuild_equal/1, deep_shape/0, big_sum/0]).\n\
     churn(N) -> churn(N, 0).\n\
     churn(0, Acc) -> Acc;\n\
     churn(N, Acc) -> churn(N - 1, Acc + length(lists:seq(1, 50))).\n\
     survive_sum() ->\n\
         L = lists:seq(1, 10000),\n\
         _ = erlang:garbage_collect(self()),\n\
         lists:sum(L).\n\
     rebuild_equal(N) ->\n\
         L = lists:seq(1, N),\n\
         _ = erlang:garbage_collect(self()),\n\
         case L =:= lists:seq(1, N) of\n\
             true -> 1;\n\
             false -> 0\n\
         end.\n\
     deep_shape() ->\n\
         T = {a, [b, {c, 3}], <<\"xy\">>},\n\
         _ = erlang:garbage_collect(self()),\n\
         case T =:= {a, [b, {c, 3}], <<\"xy\">>} of\n\
             true -> 1;\n\
             false -> 0\n\
         end.\n\
     big_sum() ->\n\
         L = lists:seq(1, 200000),\n\
         _ = erlang:garbage_collect(self()),\n\
         lists:sum(L).\n";

/// One **A.1** case: `expr` builds/forces/stresses and observes a surviving value, pinned as
/// the oracle's canonical `term_to_binary/1` image.
fn semantics(
    id: &'static str,
    description: &'static str,
    expr: &'static str,
    golden: &'static str,
) -> Case {
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
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// One **A.7** case: `expr` observes the collector's own BIF surface, pinned as the oracle's
/// canonical `term_to_binary/1` image.
fn bifs(
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
            probe: Probe::TermToBinary,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// One **A.7** case whose wrapped expression *raises*: the probe prints its
/// `exception <class> <reason>` line and exits `1`, so the golden carries that line and the
/// non-zero exit code.
fn bifs_raising(
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
            probe: Probe::TermToBinary,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 1,
        },
    }
}

/// One module-run case: every function returns an integer (the golden), and each one forces a
/// collection inside `DIFF_GC_SOURCE`.
fn module_case(
    id: &'static str,
    description: &'static str,
    function: &'static str,
    arity: u32,
    args: &'static [i64],
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
            source: DIFF_GC_SOURCE,
            module: "diff_gc",
            function,
            arity,
            args,
            nif: None,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.1/A.7 garbage-collector cases. Every one of these is verified `AGREE`.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- A.1: post-collection term identity and equality --------------------------------
        semantics(
            "gc/large_list_sum_survives_collection",
            "the sum of a 10 000-element list is unchanged across a forced collection",
            "begin L = lists:seq(1, 10000), true = erlang:garbage_collect(self()), \
             lists:sum(L) end",
            "ok <<131,98,2,251,4,8>>",
        ),
        semantics(
            "gc/large_list_head_survives_collection",
            "hd/1 of a 10 000-element list is unchanged across a forced collection",
            "begin L = lists:seq(1, 10000), true = erlang:garbage_collect(self()), hd(L) end",
            "ok <<131,97,1>>",
        ),
        semantics(
            "gc/large_list_rebuild_equality",
            "a 10 000-element list still compares equal to a rebuilt one after a collection",
            "begin L = lists:seq(1, 10000), true = erlang:garbage_collect(self()), \
             L =:= lists:seq(1, 10000) end",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        semantics(
            "gc/shared_subterm_identity_survives_collection",
            "a subterm shared by two tuple elements is not duplicated disparately by a move",
            "begin L = lists:seq(1, 5000), T = {L, L}, true = erlang:garbage_collect(self()), \
             element(1, T) =:= element(2, T) andalso element(1, T) =:= lists:seq(1, 5000) end",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        semantics(
            "gc/int_keyed_flatmap_bytes_survive_collection",
            "a 16-key integer flatmap keeps its canonical MAP_EXT image across a collection",
            "begin M = maps:from_list([{I, I * 3} || I <- lists:seq(0, 15)]), \
             true = erlang:garbage_collect(self()), M end",
            "ok <<131,116,0,0,0,16,97,0,97,0,97,1,97,3,97,2,97,6,97,3,97,9,97,4,97,12,97,5,\
             97,15,97,6,97,18,97,7,97,21,97,8,97,24,97,9,97,27,97,10,97,30,97,11,97,33,97,12,\
             97,36,97,13,97,39,97,14,97,42,97,15,97,45>>",
        ),
        semantics(
            "gc/int_keyed_hamt_size_survives_collection",
            "a 40-key integer map (the HAMT interior) keeps its size across a collection",
            "begin M = maps:from_list([{I, I * 3} || I <- lists:seq(0, 39)]), \
             true = erlang:garbage_collect(self()), maps:size(M) end",
            "ok <<131,97,40>>",
        ),
        semantics(
            "gc/atom_keyed_map_lookup_survives_collection",
            "an atom-keyed map still answers a lookup after a collection",
            "begin M = #{alpha => 1, beta => 2, gamma => 3}, \
             true = erlang:garbage_collect(self()), maps:get(beta, M) end",
            "ok <<131,97,2>>",
        ),
        semantics(
            "gc/refc_binary_size_survives_collection",
            "an off-heap refc binary keeps its byte size across a collection",
            "begin B = list_to_binary(lists:seq(1, 200)), true = erlang:garbage_collect(self()), \
             byte_size(B) end",
            "ok <<131,97,200>>",
        ),
        semantics(
            "gc/sub_binary_bytes_survive_collection",
            "a sub-binary window (`binary:part/3`) keeps its bytes across a collection",
            "begin B = list_to_binary(lists:seq(1, 200)), S = binary:part(B, 10, 40), \
             true = erlang:garbage_collect(self()), binary:at(S, 0) end",
            "ok <<131,97,11>>",
        ),
        semantics(
            "gc/binary_identity_survives_collection",
            "the same off-heap binary held twice by a tuple stays one term across a collection",
            "begin B = list_to_binary(lists:seq(1, 200)), T = {B, B}, \
             true = erlang:garbage_collect(self()), element(1, T) =:= element(2, T) end",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        semantics(
            "gc/ref_identity_survives_collection",
            "a reference held twice by a tuple stays one term across a collection",
            "begin Ref = make_ref(), T = {Ref, Ref}, true = erlang:garbage_collect(self()), \
             element(1, T) =:= element(2, T) end",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        semantics(
            "gc/pid_identity_survives_collection",
            "a pid held twice by a tuple stays one term across a collection",
            "begin P = self(), T = {P, P}, true = erlang:garbage_collect(self()), \
             element(1, T) =:= element(2, T) end",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        semantics(
            "gc/fun_value_survives_collection",
            "a fun closure is still callable and returns the same value after a collection",
            "begin F = fun(X) -> X * 2 end, true = erlang:garbage_collect(self()), F(21) end",
            "ok <<131,97,42>>",
        ),
        semantics(
            "gc/deep_nested_structure_preserved",
            "a deeply nested tuple/list/map/bin term still compares equal to its literal after a collection",
            "begin T = {a, [b, {c, #{1 => <<\"x\">>}}]}, true = erlang:garbage_collect(self()), \
             T =:= {a, [b, {c, #{1 => <<\"x\">>}}]} end",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        // --- A.1: collector-stressing programs with a stable integer result -----------------
        semantics(
            "gc/alloc_discard_foldl_sum",
            "allocating and discarding 500 inner lists leaves the fold's total unchanged",
            "begin X = lists:foldl(fun(_, A) -> A + length(lists:duplicate(100, x)) end, 0, \
             lists:seq(1, 500)), X end",
            "ok <<131,98,0,0,195,80>>",
        ),
        semantics(
            "gc/comprehension_alloc_discard_sum",
            "2 000 comprehension rounds that each allocate and drop a 50-element list sum stably",
            "begin X = lists:sum([length(lists:seq(1, 50)) || _ <- lists:seq(1, 2000)]), X end",
            "ok <<131,98,0,1,134,160>>",
        ),
        semantics(
            "gc/named_fun_alloc_loop",
            "a self-recursive named fun that allocates per round returns the same accumulator",
            "begin F = fun Loop(0, Acc) -> Acc; \
             Loop(N, Acc) -> Loop(N - 1, Acc + length(lists:seq(1, 50))) end, F(2000, 0) end",
            "ok <<131,98,0,1,134,160>>",
        ),
        semantics(
            "gc/bignum_sum_after_collection",
            "a 200 000-element range summed to a bignum is unchanged across a forced collection",
            "begin L = lists:seq(1, 200000), S = lists:sum(L), \
             true = erlang:garbage_collect(self()), S end",
            "ok <<131,110,5,0,160,78,25,168,4>>",
        ),
        semantics(
            "gc/scheduled_process_big_list_sum",
            "a spawned process's 50 000-element sum arrives intact (the sender collected for it)",
            "begin Me = self(), spawn(fun() -> L = lists:seq(1, 50000), Me ! lists:sum(L) end), \
             receive X -> X after 2000 -> timeout end end",
            "ok <<131,98,74,129,222,40>>",
        ),
        // --- A.7: the collector's BIF surface ----------------------------------------------
        bifs(
            "gc/garbage_collect_self_returns_true",
            "erlang:garbage_collect/1 on the running process returns true",
            "erlang:garbage_collect(self())",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        bifs(
            "gc/garbage_collect_0_returns_true",
            "erlang:garbage_collect/0 (the caller) returns true",
            "erlang:garbage_collect()",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        bifs(
            "gc/process_info_memory_is_positive_integer",
            "process_info(self(), memory) is a positive integer (the byte total itself is volatile)",
            "is_integer(element(2, erlang:process_info(self(), memory))) andalso \
             element(2, erlang:process_info(self(), memory)) > 0",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        bifs(
            "gc/process_info_memory_key_is_memory",
            "process_info(self(), memory) is tagged by its own key",
            "element(1, erlang:process_info(self(), memory))",
            "ok <<131,119,6,109,101,109,111,114,121>>",
        ),
        bifs(
            "gc/process_info_gc_has_min_heap_size",
            "the process_info(self(), garbage_collection) proplist carries min_heap_size",
            "lists:keymember(min_heap_size, 1, \
             element(2, erlang:process_info(self(), garbage_collection)))",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        bifs(
            "gc/process_info_gc_key_set_is_stable",
            "the garbage_collection proplist's key set is exactly the documented five keys",
            "lists:sort([K || {K, _} <- element(2, erlang:process_info(self(), \
             garbage_collection))])",
            "ok <<131,108,0,0,0,5,119,15,102,117,108,108,115,119,101,101,112,95,97,102,116,\
             101,114,119,13,109,97,120,95,104,101,97,112,95,115,105,122,101,119,18,109,105,110,\
             95,98,105,110,95,118,104,101,97,112,95,115,105,122,101,119,13,109,105,110,95,104,\
             101,97,112,95,115,105,122,101,119,9,109,105,110,111,114,95,103,99,115,106>>",
        ),
        bifs(
            "gc/process_info_gc_max_heap_size_is_a_map",
            "the garbage_collection proplist's max_heap_size field is an option map",
            "is_map(element(2, lists:keyfind(max_heap_size, 1, \
             element(2, erlang:process_info(self(), garbage_collection)))))",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        bifs(
            "gc/process_info_gc_min_heap_size_is_integer",
            "the garbage_collection proplist's min_heap_size field is an integer (its value is volatile)",
            "is_integer(element(2, lists:keyfind(min_heap_size, 1, \
             element(2, erlang:process_info(self(), garbage_collection)))))",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        // --- A.7: the two-argument erlang:garbage_collect/2 option form ---------------------
        bifs(
            "gc/garbage_collect_2_empty_options_returns_true",
            "erlang:garbage_collect/2 with an empty option list returns true",
            "erlang:garbage_collect(self(), [])",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        bifs(
            "gc/garbage_collect_2_minor_returns_true",
            "erlang:garbage_collect/2 with a `{type, minor}` option returns true",
            "erlang:garbage_collect(self(), [{type, minor}])",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        bifs(
            "gc/garbage_collect_2_major_returns_true",
            "erlang:garbage_collect/2 with a `{type, major}` option returns true",
            "erlang:garbage_collect(self(), [{type, major}])",
            "ok <<131,119,4,116,114,117,101>>",
        ),
        bifs_raising(
            "gc/garbage_collect_2_unknown_type_is_badarg",
            "erlang:garbage_collect/2 with an unknown `type` raises error:badarg",
            "erlang:garbage_collect(self(), [{type, bad}])",
            "exception error badarg",
        ),
        // --- A.1: the /2 option form is value-transparent ----------------------------------
        semantics(
            "gc/garbage_collect_2_major_list_sum_survives_collection",
            "a 10 000-element list's sum is unchanged across a forced major collection",
            "begin L = lists:seq(1, 10000), true = erlang:garbage_collect(self(), [{type, major}]), \
             lists:sum(L) end",
            "ok <<131,98,2,251,4,8>>",
        ),
        semantics(
            "gc/garbage_collect_2_minor_binary_size_survives_collection",
            "an off-heap refc binary keeps its byte size across a forced minor collection",
            "begin B = list_to_binary(lists:seq(1, 200)), true = erlang:garbage_collect(self(), \
             [{type, minor}]), byte_size(B) end",
            "ok <<131,97,200>>",
        ),
        // --- compiled bytecode through the `run` action (the engine-driven path) ------------
        module_case(
            "gc/module_churn_alloc_discard",
            "a compiled churn loop that allocates and drops a list per round returns the same total",
            "churn",
            1,
            &[200],
            "10000",
        ),
        module_case(
            "gc/module_survive_sum",
            "a compiled 10 000-element list sums identically across a forced collection",
            "survive_sum",
            0,
            &[],
            "50005000",
        ),
        module_case(
            "gc/module_rebuild_equal",
            "a compiled range still compares equal to a rebuilt one after a forced collection",
            "rebuild_equal",
            1,
            &[5000],
            "1",
        ),
        module_case(
            "gc/module_deep_shape",
            "a compiled nested tuple/list/binary term still compares equal to its literal after a collection",
            "deep_shape",
            0,
            &[],
            "1",
        ),
        module_case(
            "gc/module_big_sum",
            "a compiled 200 000-element range summed to a bignum is unchanged across a collection",
            "big_sum",
            0,
            &[],
            "20000100000",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The A.1 post-collection value/identity cases.
    const VALUE_CASES: &[&str] = &[
        "gc/large_list_sum_survives_collection",
        "gc/large_list_head_survives_collection",
        "gc/large_list_rebuild_equality",
        "gc/shared_subterm_identity_survives_collection",
        "gc/int_keyed_flatmap_bytes_survive_collection",
        "gc/int_keyed_hamt_size_survives_collection",
        "gc/atom_keyed_map_lookup_survives_collection",
        "gc/refc_binary_size_survives_collection",
        "gc/sub_binary_bytes_survive_collection",
        "gc/binary_identity_survives_collection",
        "gc/ref_identity_survives_collection",
        "gc/pid_identity_survives_collection",
        "gc/fun_value_survives_collection",
        "gc/deep_nested_structure_preserved",
    ];

    /// The A.1 collector-stressing programs.
    const STRESS_CASES: &[&str] = &[
        "gc/alloc_discard_foldl_sum",
        "gc/comprehension_alloc_discard_sum",
        "gc/named_fun_alloc_loop",
        "gc/bignum_sum_after_collection",
        "gc/scheduled_process_big_list_sum",
    ];

    /// The A.7 collector-BIF cases.
    const BIF_CASES: &[&str] = &[
        "gc/garbage_collect_self_returns_true",
        "gc/garbage_collect_0_returns_true",
        "gc/process_info_memory_is_positive_integer",
        "gc/process_info_memory_key_is_memory",
        "gc/process_info_gc_has_min_heap_size",
        "gc/process_info_gc_key_set_is_stable",
        "gc/process_info_gc_max_heap_size_is_a_map",
        "gc/process_info_gc_min_heap_size_is_integer",
    ];

    /// The compiled-bytecode (`run` action) cases.
    const MODULE_CASES: &[&str] = &[
        "gc/module_churn_alloc_discard",
        "gc/module_survive_sum",
        "gc/module_rebuild_equal",
        "gc/module_deep_shape",
        "gc/module_big_sum",
    ];

    /// The A.7 `erlang:garbage_collect/2` option-form cases. Kept in their own list because
    /// one of them asserts a raise (exit `1`), unlike [`BIF_CASES`].
    const GC2_CASES: &[&str] = &[
        "gc/garbage_collect_2_empty_options_returns_true",
        "gc/garbage_collect_2_minor_returns_true",
        "gc/garbage_collect_2_major_returns_true",
        "gc/garbage_collect_2_unknown_type_is_badarg",
    ];

    /// The A.1 post-collection value cases that drive the collection through the `/2` form.
    const GC2_VALUE_CASES: &[&str] = &[
        "gc/garbage_collect_2_major_list_sum_survives_collection",
        "gc/garbage_collect_2_minor_binary_size_survives_collection",
    ];

    fn case<'a>(cases: &'a [Case], id: &str) -> &'a Case {
        cases
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("case {id} is missing from the gc group"))
    }

    #[test]
    fn the_group_is_exactly_the_declared_cases() {
        assert_eq!(
            group().len(),
            VALUE_CASES.len()
                + STRESS_CASES.len()
                + BIF_CASES.len()
                + GC2_CASES.len()
                + GC2_VALUE_CASES.len()
                + MODULE_CASES.len()
        );
        // Every id is `gc/<name>`.
        for case in group() {
            assert!(
                case.id.starts_with("gc/") && case.id.len() > "gc/".len(),
                "case id {} is not gc/<name>",
                case.id
            );
            assert!(!case.description.is_empty(), "case {} has no description", case.id);
        }
    }

    #[test]
    fn every_eval_case_is_a_term_to_binary_probe_with_no_extra_argv_or_env() {
        let cases = group();
        let cases = cases.as_slice();
        for id in VALUE_CASES.iter().chain(STRESS_CASES).chain(BIF_CASES) {
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
            assert_eq!(case.golden.exit_code, 0, "case {id} expects a clean exit");
        }
        // The measured-value cases evidence A.1; the BIF cases evidence A.7.
        for id in VALUE_CASES.iter().chain(STRESS_CASES) {
            assert_eq!(case(cases, id).contracts, &[Contract::LanguageSemantics], "case {id}");
        }
        for id in BIF_CASES {
            assert_eq!(case(cases, id).contracts, &[Contract::OtpApps], "case {id}");
        }
    }

    #[test]
    fn the_value_and_stress_cases_all_force_or_stress_a_collection() {
        let cases = group();
        let cases = cases.as_slice();
        for id in VALUE_CASES {
            let expr = case(cases, id).expr().expect("an Eval case has an expr");
            assert!(
                expr.contains("erlang:garbage_collect(self())"),
                "case {id} must force a collection: {expr:?}"
            );
        }
        for id in STRESS_CASES {
            let expr = case(cases, id).expr().expect("an Eval case has an expr");
            assert!(
                expr.contains("lists:") || expr.contains("spawn("),
                "case {id} must exercise the allocator: {expr:?}"
            );
        }
    }

    #[test]
    fn goldens_never_pin_a_volatile_pid_reference_or_heap_size() {
        for case in group() {
            let golden = case.golden.stdout.expect("every case asserts stdout");
            for needle in ["<0.", "#Ref<", "#Port<", "#Fun<"] {
                assert!(
                    !golden.contains(needle),
                    "case {}: the golden pins an unstable live term: {golden:?}",
                    case.id
                );
            }
            // A golden is a single line (the probe prints exactly one and halts).
            assert!(
                !golden.contains('\n'),
                "case {}: the golden must be one line: {golden:?}",
                case.id
            );
        }
    }

    #[test]
    fn the_bif_cases_pin_the_garbage_collect_return_value_and_process_info_shapes() {
        let cases = group();
        let cases = cases.as_slice();
        for id in [
            "gc/garbage_collect_self_returns_true",
            "gc/garbage_collect_0_returns_true",
        ] {
            let case = case(cases, id);
            assert_eq!(
                case.golden.stdout,
                Some("ok <<131,119,4,116,114,117,101>>"),
                "case {id} pins `true`"
            );
            assert_eq!(
                case.expr(),
                Some(if id.ends_with("0_returns_true") {
                    "erlang:garbage_collect()"
                } else {
                    "erlang:garbage_collect(self())"
                }),
                "case {id}"
            );
        }
        // Every `garbage_collection` case reads the proplist through `process_info/2`
        // and never renders one of its (volatile) values.
        for id in [
            "gc/process_info_gc_has_min_heap_size",
            "gc/process_info_gc_key_set_is_stable",
            "gc/process_info_gc_max_heap_size_is_a_map",
            "gc/process_info_gc_min_heap_size_is_integer",
        ] {
            let expr = case(cases, id).expr().expect("an Eval case has an expr");
            assert!(
                expr.contains("erlang:process_info(self(), garbage_collection)"),
                "case {id} must read the garbage_collection proplist: {expr:?}"
            );
        }
        assert_eq!(
            find_golden(cases, "gc/process_info_memory_key_is_memory"),
            "ok <<131,119,6,109,101,109,111,114,121>>",
            "the memory field's key is pinned by name"
        );
    }

    fn find_golden(cases: &[Case], id: &str) -> &'static str {
        case(cases, id)
            .golden
            .stdout
            .expect("every case asserts stdout")
    }

    #[test]
    fn the_garbage_collect_2_cases_pin_the_two_argument_option_form() {
        let cases = group();
        let cases = cases.as_slice();
        for id in GC2_CASES {
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
            assert_eq!(case.contracts, &[Contract::OtpApps], "case {id}");
            let expr = case.expr().expect("an Eval case has an expr");
            assert!(
                expr.starts_with("erlang:garbage_collect(self(), "),
                "case {id} must drive the two-argument form: {expr:?}"
            );
        }
        // The three valid option forms return `true` and exit cleanly.
        for id in [
            "gc/garbage_collect_2_empty_options_returns_true",
            "gc/garbage_collect_2_minor_returns_true",
            "gc/garbage_collect_2_major_returns_true",
        ] {
            let case = case(cases, id);
            assert_eq!(find_golden(cases, id), "ok <<131,119,4,116,114,117,101>>", "case {id}");
            assert_eq!(case.golden.exit_code, 0, "case {id}");
        }
        // An unknown `type` is a raised `error:badarg`, i.e. an `exception` line and exit 1.
        let badarg = case(cases, "gc/garbage_collect_2_unknown_type_is_badarg");
        assert_eq!(badarg.golden.stdout, Some("exception error badarg"));
        assert_eq!(badarg.golden.exit_code, 1, "the raised badarg exits 1");

        // The `/2` value cases evidence A.1 and must actually force the collection through it.
        for id in GC2_VALUE_CASES {
            let case = case(cases, id);
            assert_eq!(case.contracts, &[Contract::LanguageSemantics], "case {id}");
            assert_eq!(case.golden.exit_code, 0, "case {id}");
            let expr = case.expr().expect("an Eval case has an expr");
            assert!(
                expr.contains("erlang:garbage_collect(self(), [{type,"),
                "case {id} must force a collection through the /2 form: {expr:?}"
            );
        }
    }
    #[test]
    fn the_module_cases_are_integer_returning_run_cases_over_one_module() {
        let cases = group();
        let cases = cases.as_slice();
        for id in MODULE_CASES {
            let case = case(cases, id);
            let CaseKind::Module(run) = &case.kind else {
                panic!("case {id} must be a module-run case");
            };
            assert_eq!(run.module, "diff_gc", "case {id}");
            assert_eq!(run.source, DIFF_GC_SOURCE, "case {id}");
            assert_eq!(run.nif, None, "case {id}");
            assert_eq!(run.arity, run.args.len() as u32, "case {id}");
            assert!(
                case.contracts.contains(&Contract::BeamAsm)
                    && case.contracts.contains(&Contract::LanguageSemantics)
                    && case.contracts.contains(&Contract::OtpApps),
                "case {id} must evidence .beam + A.1 + A.7"
            );
            // The module source must force a collection in the applied function.
            assert!(
                run.source.contains("erlang:garbage_collect(self())"),
                "case {id}: the compiled module must force a collection"
            );
            // A module-run golden is an integer line (the `run` action prints `~w`).
            let golden = case.golden.stdout.expect("every case asserts stdout");
            assert!(
                golden.parse::<i64>().is_ok(),
                "case {id}: a module-run golden must be an integer, got {golden:?}"
            );
        }
    }
}
