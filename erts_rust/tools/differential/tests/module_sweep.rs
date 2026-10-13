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

//! A **real-system-module execution sweep** through the differential harness.
//!
//! Where `src/corpus/mexec.rs` registers a small, curated set of real-`stdlib`/`kernel`
//! execution cases in the shipped corpus (so `differential suite` reports them), this
//! file is the **wide** evidence: a larger list of `(module, function, int-args)` entries
//! — deliberately bigger than the corpus group — that each exercise a **distinct real
//! OTP module** end to end under **both** runtimes, driven through
//! [`differential::harness`].
//!
//! # What the sweep proves
//!
//! For every entry a tiny wrapper module is generated (`f(A1, …) -> <the real call>`) and
//! compiled with the system `erlc`; the **oracle** applies it through `erl -pa … -eval …`
//! and the **Rust runtime** through `beam_rt run <file>.beam …`. The wrapper's call is a
//! cross-module call into the *real* library, so the Rust side must actually load and
//! execute the real system module from the code path. The two observable results
//! (stdout + exit status) are compared by the harness.
//!
//! # Honesty contract
//!
//! * **Every entry must be `Agree` or `Unsupported` — never `Mismatch`.** A mismatch is a
//!   real divergence between the runtimes and fails the test loudly; a candidate that
//!   diverges is a *finding*, not something to add with a weakened expectation.
//! * An entry the Rust runtime cannot run is `Unsupported` (its documented capability-gap
//!   signal), reported honestly and counted — never a fabricated pass.
//! * A missing oracle **skips loudly** (`SKIP: …`, never a silent pass); a missing `erlc`
//!   does the same, since the wrappers cannot be compiled without it.
//!
//! The generated wrappers are grouped by the real module they exercise, so the sweep
//! compiles one wrapper per module (not one per entry) while every entry still gets its
//! own argv and its own comparison.

use std::collections::BTreeMap;

use differential::harness::{run_case_in, RuntimeContext};
use differential::module_build;
use differential::runtime::resolve_oracle;
use differential::{Case, CaseKind, Contract, Golden, ModuleRun, Verdict};

mod common;

/// One sweep entry: a real system module + function invoked with integer arguments.
struct Entry {
    /// The real system module exercised (also the generated wrapper's group key).
    module: &'static str,
    /// `module:function/arity` — the human label printed per entry.
    spec: &'static str,
    /// The integer arguments handed to the wrapper function (and so to the real call).
    args: &'static [i64],
    /// The wrapper body, referencing the parameters as `A1`, `A2`, …; it makes the real
    /// cross-module call (and any list/tuple the real function needs).
    body: &'static str,
}

/// The contracts every sweep entry evidences: a compiled `.beam` executes (A.2) and a
/// real OTP application module is loaded and run (A.7).
const CONTRACTS: &[Contract] = &[Contract::BeamAsm, Contract::OtpApps];

/// The sweep: **277 entries across 35 real modules** — `array`, `base64`, `binary`,
/// `calendar`, `code`, `digraph`, `digraph_utils`, `erl_anno`, `erl_error`, `erl_internal`,
/// `erl_parse`, `erl_pp`, `erl_scan`, `ets`, `filelib`, `filename`, `gb_sets`, `gb_trees`,
/// `io_lib`, `io_lib_pretty`, `lists`, `maps`, `math`, `orddict`, `ordsets`, `os`,
/// `proplists`, `queue`, `rand`, `re`, `sets`, `sofs`, `string`, `unicode` and
/// `uri_string`.
///
/// Every entry is verified against the current `beam_rt`: all but two `Agree`, and the two
/// that do not are honest, counted `Unsupported` capability gaps (`filelib:is_dir/1` /
/// `filelib:is_file/1`, whose `read_file_info/1` seam the runtime does not serve). The two
/// `re:*` entries (`re:version/0`, `re:split/2`) now `Agree` — the `re` family is fully
/// implemented — and are additionally gate-checked by the A.7 `re` corpus group
/// (`src/corpus/re.rs`).
///
/// A candidate whose observable would be volatile (a `~w` of a multi-key map, a pid/ref/port,
/// or an unseeded clock/`rand` value) is deliberately absent. Two shapes that *used* to produce
/// a differing observable — a zero-length binary returned by `base64:encode/1` and handed
/// straight to `io:format`, and `erl_error:format_exception/3` with a heap-allocated reason —
/// are now fixed, and are pinned as corpus cases (`modules/bif_base64_encode_empty_size` and the
/// `modules/erl_error_format_exception_*` group), so no sweep entry needs to avoid them.
const SWEEP: &[Entry] = &[
    // --- array -------------------------------------------------------------------
    Entry { module: "array", spec: "array:size/1", args: &[0], body: "array:size(array:new())" },
    Entry { module: "array", spec: "array:size/1", args: &[5], body: "array:size(array:set(1, A1, array:new()))" },
    Entry { module: "array", spec: "array:to_list/1", args: &[5], body: "length(array:to_list(array:from_list([1, A1, 3])))" },
    Entry { module: "array", spec: "array:foldr/3", args: &[5], body: "array:foldr(fun(_I, X, Acc) -> Acc * 10 + X end, 0, array:from_list([1, 2]))" },
    Entry { module: "array", spec: "array:size/1", args: &[5], body: "array:size(array:from_orddict([{1, A1}, {2, 2}]))" },
    Entry { module: "array", spec: "array:sparse_to_list/1", args: &[5], body: "length(array:sparse_to_list(array:from_list([1, A1])))" },
    Entry { module: "array", spec: "array:size/1", args: &[5], body: "array:size(array:concat(array:from_list([1]), array:from_list([A1])))" },
    Entry { module: "array", spec: "array:size/1", args: &[5], body: "array:size(array:prepend(0, array:from_list([A1]))) =:= 2" },
    // --- base64 ------------------------------------------------------------------
    Entry { module: "base64", spec: "base64:encode/1", args: &[7], body: "base64:encode(<<A1, 2, 3>>)" },
    Entry { module: "base64", spec: "base64:mime_decode/1", args: &[7], body: "base64:mime_decode(base64:encode(<<A1, 2>>))" },
    Entry { module: "base64", spec: "base64:encode_to_string/1", args: &[7], body: "base64:encode_to_string(<<A1, 2>>)" },
    Entry { module: "base64", spec: "base64:decode_to_string/1", args: &[7], body: "base64:decode_to_string(base64:encode(<<A1, 2>>))" },
    Entry { module: "base64", spec: "base64:mime_decode/1", args: &[0], body: "base64:mime_decode(<<\"aGVs\\r\\nbG8=\">>)" },
    // --- binary ------------------------------------------------------------------
    Entry { module: "binary", spec: "binary:part/3", args: &[4], body: "binary:part(<<1, 2, 3, A1, 5>>, 1, 2)" },
    Entry { module: "binary", spec: "binary:last/1", args: &[7], body: "binary:last(<<A1, 2, 3>>)" },
    Entry { module: "binary", spec: "binary:copy/2", args: &[7], body: "byte_size(binary:copy(<<A1, 2>>, 3))" },
    Entry { module: "binary", spec: "binary:bin_to_list/2", args: &[7], body: "binary:bin_to_list(<<A1, 2, 3>>, {1, 2})" },
    Entry { module: "binary", spec: "binary:decode_unsigned/1", args: &[7], body: "binary:decode_unsigned(<<0, A1>>)" },
    Entry { module: "binary", spec: "binary:decode_unsigned/2", args: &[7], body: "binary:decode_unsigned(<<A1, 2>>, little)" },
    Entry { module: "binary", spec: "binary:matches/2", args: &[0], body: "binary:matches(<<1, 2, 3, 2>>, <<2>>)" },
    Entry { module: "binary", spec: "binary:replace/3", args: &[0], body: "binary:replace(<<1, 2, 3>>, <<2>>, <<9>>)" },
    Entry { module: "binary", spec: "binary:longest_common_prefix/1", args: &[0], body: "binary:longest_common_prefix([<<1, 2, 3>>, <<1, 2, 4>>])" },
    Entry { module: "binary", spec: "binary:referenced_byte_size/1", args: &[7], body: "binary:referenced_byte_size(<<A1>>)" },
    Entry { module: "binary", spec: "binary:encode_hex/2", args: &[7], body: "binary:encode_hex(<<A1>>, lowercase)" },
    Entry { module: "binary", spec: "binary:join/2", args: &[0], body: "binary:join([<<1, 2>>, <<3>>], <<0>>)" },
    Entry { module: "binary", spec: "binary:split/3", args: &[0], body: "binary:split(<<1, 2, 3, 2>>, <<2>>, [trim])" },
    Entry { module: "binary", spec: "binary:matches/2", args: &[0], body: "binary:matches(<<1, 2, 1>>, <<1>>)" },
    Entry { module: "binary", spec: "binary:at/2", args: &[0], body: "binary:at(<<1, 2, 3>>, 0)" },
    // --- calendar ----------------------------------------------------------------
    Entry { module: "calendar", spec: "calendar:gregorian_days_to_date/1", args: &[730485], body: "calendar:gregorian_days_to_date(A1)" },
    Entry { module: "calendar", spec: "calendar:datetime_to_gregorian_seconds/1", args: &[2024, 2, 29], body: "calendar:datetime_to_gregorian_seconds({{A1, A2, A3}, {12, 0, 0}})" },
    Entry { module: "calendar", spec: "calendar:seconds_to_daystime/1", args: &[90061], body: "calendar:seconds_to_daystime(A1)" },
    Entry { module: "calendar", spec: "calendar:time_difference/2", args: &[1, 0, 0], body: "calendar:time_difference({{2000, 1, 2}, {A1, A2, A3}}, {{2000, 1, 1}, {0, 0, 0}})" },
    Entry { module: "calendar", spec: "calendar:is_leap_year/1", args: &[2024], body: "calendar:is_leap_year(A1)" },
    Entry { module: "calendar", spec: "calendar:last_day_of_the_month/2", args: &[2024], body: "calendar:last_day_of_the_month(A1, 2)" },
    Entry { module: "calendar", spec: "calendar:time_difference/2", args: &[5, 0, 0], body: "calendar:time_difference({{2000, 1, 1}, {A1, A2, A3}}, {{2000, 1, 1}, {0, 0, 0}})" },
    // --- code --------------------------------------------------------------------
    Entry { module: "code", spec: "code:objfile_extension/0", args: &[0], body: "code:objfile_extension()" },
    Entry { module: "code", spec: "code:which/1", args: &[0], body: "code:which(lists)" },
    Entry { module: "code", spec: "code:lib_dir/1", args: &[0], body: "code:lib_dir(erts)" },
    Entry { module: "code", spec: "code:get_path/0", args: &[0], body: "is_list(code:get_path())" },
    // --- digraph -----------------------------------------------------------------
    Entry { module: "digraph", spec: "digraph:no_vertices/1", args: &[1], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:no_vertices(D) end" },
    Entry { module: "digraph", spec: "digraph:out_degree/2", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), digraph:out_degree(D, A1) end" },
    Entry { module: "digraph", spec: "digraph:out_neighbours/2", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), lists:sort(digraph:out_neighbours(D, A1)) end" },
    Entry { module: "digraph", spec: "digraph:no_vertices/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), digraph:del_vertex(D, A1), digraph:no_vertices(D) end" },
    Entry { module: "digraph", spec: "digraph:sink_vertices/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), lists:sort(digraph:sink_vertices(D)) end" },
    Entry { module: "digraph", spec: "digraph:edges/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), lists:sort(digraph:edges(D)) end" },
    Entry { module: "digraph", spec: "digraph:edges/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2, e1), lists:sort(digraph:edges(D)) end" },
    Entry { module: "digraph", spec: "digraph:no_edges/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), digraph:del_edges(D, digraph:edges(D)), digraph:no_edges(D) end" },
    Entry { module: "digraph", spec: "digraph:vertex/2", args: &[1], body: "begin D = digraph:new(), digraph:add_vertex(D, A1, l), element(2, digraph:vertex(D, A1)) end" },
    // --- digraph_utils -----------------------------------------------------------
    Entry { module: "digraph_utils", spec: "digraph_utils:is_acyclic/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), digraph_utils:is_acyclic(D) end" },
    Entry { module: "digraph_utils", spec: "digraph_utils:topsort/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), digraph_utils:topsort(D) end" },
    Entry { module: "digraph_utils", spec: "digraph_utils:preorder/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), length(digraph_utils:preorder(D)) end" },
    Entry { module: "digraph_utils", spec: "digraph_utils:cyclic_strong_components/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), length(digraph_utils:cyclic_strong_components(D)) end" },
    Entry { module: "digraph_utils", spec: "digraph_utils:postorder/1", args: &[1, 2], body: "begin D = digraph:new(), digraph:add_vertex(D, A1), digraph:add_vertex(D, A2), digraph:add_edge(D, A1, A2), length(digraph_utils:postorder(D)) end" },
    // --- erl_anno ----------------------------------------------------------------
    Entry { module: "erl_anno", spec: "erl_anno:line/1", args: &[5], body: "erl_anno:line(erl_anno:new(A1))" },
    Entry { module: "erl_anno", spec: "erl_anno:column/1", args: &[5], body: "erl_anno:column(erl_anno:new(A1))" },
    Entry { module: "erl_anno", spec: "erl_anno:is_anno/1", args: &[5], body: "erl_anno:is_anno(erl_anno:new(A1))" },
    Entry { module: "erl_anno", spec: "erl_anno:is_anno/1", args: &[5], body: "erl_anno:is_anno(A1)" },
    Entry { module: "erl_anno", spec: "erl_anno:to_term/1", args: &[5], body: "erl_anno:to_term(erl_anno:new(A1))" },
    // --- erl_error ---------------------------------------------------------------
    Entry { module: "erl_error", spec: "erl_error:format_exception/3", args: &[0], body: "length(lists:flatten(erl_error:format_exception(error, badarg, [])))" },
    Entry { module: "erl_error", spec: "erl_error:format_exception/3", args: &[0], body: "lists:flatten(erl_error:format_exception(error, badarg, []))" },
    // --- erl_internal ------------------------------------------------------------
    Entry { module: "erl_internal", spec: "erl_internal:bif/3", args: &[0], body: "erl_internal:bif(erlang, length, 1)" },
    Entry { module: "erl_internal", spec: "erl_internal:arith_op/2", args: &[0], body: "erl_internal:arith_op('+', 2)" },
    Entry { module: "erl_internal", spec: "erl_internal:type_test/2", args: &[0], body: "erl_internal:type_test(is_atom, 1)" },
    Entry { module: "erl_internal", spec: "erl_internal:comp_op/2", args: &[0], body: "erl_internal:comp_op('=:=', 2)" },
    Entry { module: "erl_internal", spec: "erl_internal:send_op/2", args: &[0], body: "erl_internal:send_op('!', 2)" },
    Entry { module: "erl_internal", spec: "erl_internal:guard_bif/2", args: &[0], body: "erl_internal:guard_bif(is_atom, 1)" },
    // --- erl_parse ---------------------------------------------------------------
    Entry { module: "erl_parse", spec: "erl_parse:parse_term/1", args: &[0], body: "erl_parse:parse_term(element(2, erl_scan:string(\"1 + 2.\")))" },
    Entry { module: "erl_parse", spec: "erl_scan:string/1", args: &[0], body: "length(element(2, erl_scan:string(\"foo(1).\")))" },
    Entry { module: "erl_parse", spec: "erl_parse:abstract/1", args: &[0], body: "erl_parse:abstract(42)" },
    Entry { module: "erl_parse", spec: "erl_parse:inop_prec/1", args: &[0], body: "erl_parse:inop_prec('+')" },
    // --- erl_pp ------------------------------------------------------------------
    Entry { module: "erl_pp", spec: "erl_pp:form/1", args: &[0], body: "length(lists:flatten(erl_pp:form({attribute, 1, module, foo})))" },
    Entry { module: "erl_pp", spec: "erl_pp:form/1", args: &[0], body: "lists:flatten(erl_pp:form({attribute, 1, module, foo}))" },
    Entry { module: "erl_pp", spec: "erl_pp:expr/1", args: &[0], body: "length(lists:flatten(erl_pp:expr({integer, 1, 42})))" },
    Entry { module: "erl_pp", spec: "erl_pp:expr/1", args: &[0], body: "lists:flatten(erl_pp:expr({op, 1, '+', {integer, 1, 1}, {integer, 1, 2}}))" },
    // --- erl_scan ----------------------------------------------------------------
    Entry { module: "erl_scan", spec: "erl_scan:string/1", args: &[7], body: "length(element(2, erl_scan:string(\"foo\" ++ integer_to_list(A1))))" },
    Entry { module: "erl_scan", spec: "erl_scan:string/1", args: &[7], body: "length(element(2, erl_scan:string(integer_to_list(A1) ++ \", bar.\")))" },
    Entry { module: "erl_scan", spec: "erl_scan:reserved_word/1", args: &[7], body: "erl_scan:reserved_word(foo)" },
    Entry { module: "erl_scan", spec: "erl_scan:format_error/1", args: &[7], body: "erl_scan:format_error({illegal, 1})" },
    // --- ets ---------------------------------------------------------------------
    Entry { module: "ets", spec: "ets:lookup/2", args: &[7], body: "begin T = ets:new(t, []), ets:insert(T, {1, A1}), ets:lookup(T, 1) end" },
    Entry { module: "ets", spec: "ets:member/2", args: &[7], body: "begin T = ets:new(t, []), ets:insert(T, {1, A1}), ets:member(T, 1) end" },
    Entry { module: "ets", spec: "ets:info/2", args: &[7], body: "begin T = ets:new(t, []), ets:insert(T, {1, A1}), ets:delete(T, 1), ets:info(T, size) end" },
    Entry { module: "ets", spec: "ets:last/1", args: &[7], body: "begin T = ets:new(t, []), ets:insert(T, {1, A1}), ets:last(T) end" },
    Entry { module: "ets", spec: "ets:info/2", args: &[7], body: "begin T = ets:new(t, []), ets:insert(T, {1, A1}), ets:delete_all_objects(T), ets:info(T, size) end" },
    Entry { module: "ets", spec: "ets:first/1", args: &[7], body: "begin T = ets:new(t, [ordered_set]), ets:insert(T, {A1, a}), ets:first(T) end" },
    Entry { module: "ets", spec: "ets:lookup_element/3", args: &[7], body: "begin T = ets:new(t, []), ets:insert(T, {1, A1}), ets:lookup_element(T, 1, 2) end" },
    // --- filelib -----------------------------------------------------------------
    Entry { module: "filelib", spec: "filelib:is_dir/1", args: &[0], body: "filelib:is_dir(\"/\")" },
    Entry { module: "filelib", spec: "filelib:is_file/1", args: &[0], body: "filelib:is_file(\"/nonexistent-xyz-123\")" },
    // --- filename ----------------------------------------------------------------
    Entry { module: "filename", spec: "filename:split/1", args: &[3], body: "filename:split(\"a/b/c\" ++ integer_to_list(A1))" },
    Entry { module: "filename", spec: "filename:join/2", args: &[3], body: "filename:join(\"a\", integer_to_list(A1))" },
    Entry { module: "filename", spec: "filename:extension/1", args: &[3], body: "filename:extension(\"a.b.c\")" },
    Entry { module: "filename", spec: "filename:nativename/1", args: &[3], body: "filename:nativename(\"a/b\")" },
    Entry { module: "filename", spec: "filename:basename/2", args: &[3], body: "filename:basename(\"/a/b/c.txt\", \".txt\")" },
    Entry { module: "filename", spec: "filename:absname_join/2", args: &[0], body: "filename:absname_join(\"/base\", \"a/b\")" },
    Entry { module: "filename", spec: "filename:basedir/2", args: &[0], body: "filename:basedir(user_data, \"myapp\")" },
    Entry { module: "filename", spec: "filename:flatten/1", args: &[0], body: "filename:flatten([\"a\", [\"b\", \"c\"]])" },
    // --- gb_sets -----------------------------------------------------------------
    Entry { module: "gb_sets", spec: "gb_sets:size/1", args: &[0], body: "gb_sets:size(gb_sets:empty())" },
    Entry { module: "gb_sets", spec: "gb_sets:size/1", args: &[9], body: "gb_sets:size(gb_sets:singleton(A1))" },
    Entry { module: "gb_sets", spec: "gb_sets:is_member/2", args: &[1], body: "gb_sets:is_member(A1, gb_sets:from_list([1, 2, 3]))" },
    Entry { module: "gb_sets", spec: "gb_sets:size/1", args: &[0], body: "gb_sets:size(gb_sets:intersection(gb_sets:from_list([1, 2]), gb_sets:from_list([2, 3])))" },
    Entry { module: "gb_sets", spec: "gb_sets:is_subset/2", args: &[0], body: "gb_sets:is_subset(gb_sets:from_list([1]), gb_sets:from_list([1, 2]))" },
    Entry { module: "gb_sets", spec: "gb_sets:take_smallest/1", args: &[0], body: "gb_sets:take_smallest(gb_sets:from_list([1, 2, 3]))" },
    Entry { module: "gb_sets", spec: "gb_sets:size/1", args: &[0], body: "gb_sets:size(gb_sets:map(fun(X) -> X * 2 end, gb_sets:from_list([1, 2, 3])))" },
    Entry { module: "gb_sets", spec: "gb_sets:singleton/1", args: &[0], body: "gb_sets:singleton(1) =:= gb_sets:singleton(1)" },
    Entry { module: "gb_sets", spec: "gb_sets:add_element/2", args: &[0], body: "gb_sets:add_element(4, gb_sets:from_list([1, 2, 3])) =:= gb_sets:from_list([1, 2, 3, 4])" },
    Entry { module: "gb_sets", spec: "gb_sets:take_largest/1", args: &[0], body: "gb_sets:take_largest(gb_sets:from_list([1, 2, 3]))" },
    Entry { module: "gb_sets", spec: "gb_sets:is_set/1", args: &[0], body: "gb_sets:is_set(gb_sets:from_list([1]))" },
    Entry { module: "gb_sets", spec: "gb_sets:iterator/1", args: &[0], body: "gb_sets:iterator(gb_sets:from_list([1, 2, 3]))" },
    // --- gb_trees ----------------------------------------------------------------
    Entry { module: "gb_trees", spec: "gb_trees:size/1", args: &[0], body: "gb_trees:size(gb_trees:empty())" },
    Entry { module: "gb_trees", spec: "gb_trees:size/1", args: &[3], body: "gb_trees:size(gb_trees:delete(1, gb_trees:from_list([{1, A1}, {2, 20}])))" },
    Entry { module: "gb_trees", spec: "gb_trees:values/1", args: &[3], body: "gb_trees:values(gb_trees:from_list([{2, A1}, {1, 10}]))" },
    Entry { module: "gb_trees", spec: "gb_trees:take_largest/1", args: &[3], body: "gb_trees:take_largest(gb_trees:from_list([{1, A1}, {2, 20}]))" },
    Entry { module: "gb_trees", spec: "gb_trees:is_empty/1", args: &[3], body: "gb_trees:is_empty(gb_trees:empty())" },
    Entry { module: "gb_trees", spec: "gb_trees:larger/2", args: &[3], body: "gb_trees:larger(1, gb_trees:from_list([{1, A1}, {2, 20}]))" },
    Entry { module: "gb_trees", spec: "gb_trees:from_orddict/1", args: &[3], body: "gb_trees:from_orddict([{1, A1}, {2, 20}])" },
    Entry { module: "gb_trees", spec: "gb_trees:larger/2", args: &[3], body: "gb_trees:larger(1, gb_trees:from_list([{1, A1}, {3, 3}]))" },
    // --- io_lib ------------------------------------------------------------------
    Entry { module: "io_lib", spec: "io_lib:format/2", args: &[42], body: "hd(io_lib:format(\"~b\", [A1]))" },
    Entry { module: "io_lib", spec: "io_lib:format/2", args: &[8], body: "lists:flatten(io_lib:format(\"~.2f\", [A1 * 1.0]))" },
    Entry { module: "io_lib", spec: "io_lib:write/1", args: &[7], body: "length(lists:flatten(io_lib:write(A1)))" },
    Entry { module: "io_lib", spec: "io_lib:write/1", args: &[7], body: "length(lists:flatten(io_lib:write([A1, 2, 3])))" },
    Entry { module: "io_lib", spec: "io_lib:printable_list/1", args: &[7], body: "io_lib:printable_list(\"abc\")" },
    Entry { module: "io_lib", spec: "io_lib:print/1", args: &[7], body: "hd(io_lib:print(A1))" },
    Entry { module: "io_lib", spec: "io_lib:format/2", args: &[7], body: "lists:flatten(io_lib:format(\"~.16b\", [255]))" },
    Entry { module: "io_lib", spec: "io_lib:format/2", args: &[7], body: "lists:flatten(io_lib:format(\"~10w|~n\", [A1]))" },
    Entry { module: "io_lib", spec: "io_lib:format/2", args: &[7], body: "lists:flatten(io_lib:format(\"~p\", [[1, 2, A1]]))" },
    Entry { module: "io_lib", spec: "io_lib:latin1_char_list/1", args: &[7], body: "io_lib:latin1_char_list(\"abc\")" },
    Entry { module: "io_lib", spec: "io_lib:write_atom_as_latin1/1", args: &[7], body: "io_lib:write_atom_as_latin1(ok)" },
    Entry { module: "io_lib", spec: "io_lib:write_string/1", args: &[7], body: "lists:flatten(io_lib:write_string(\"ab\"))" },
    // --- io_lib_pretty -----------------------------------------------------------
    Entry { module: "io_lib_pretty", spec: "io_lib_pretty:print/2", args: &[7], body: "length(lists:flatten(io_lib_pretty:print([1, 2, A1], 80)))" },
    // --- lists -------------------------------------------------------------------
    Entry { module: "lists", spec: "lists:sum/1", args: &[5], body: "lists:sum(lists:seq(1, A1))" },
    Entry { module: "lists", spec: "lists:sort/1", args: &[2, 3, 1], body: "lists:sort([A1, A2, A3])" },
    Entry { module: "lists", spec: "lists:seq/2", args: &[4], body: "lists:seq(A1, A1 + 4)" },
    Entry { module: "lists", spec: "lists:last/1", args: &[10, 20, 30], body: "lists:last([A1, A2, A3])" },
    Entry { module: "lists", spec: "lists:usort/1", args: &[2], body: "lists:usort([3, 1, A1, 1, 2])" },
    Entry { module: "lists", spec: "lists:flatten/1", args: &[1, 2, 3], body: "length(lists:flatten([[A1], [A2, [A3]]]))" },
    Entry { module: "lists", spec: "lists:sublist/2", args: &[3], body: "lists:sublist([1, 2, 3, 4, 5], A1)" },
    Entry { module: "lists", spec: "lists:splitwith/2", args: &[4], body: "lists:splitwith(fun(X) -> X < 3 end, [1, 2, 3, A1])" },
    Entry { module: "lists", spec: "lists:foldr/3", args: &[10], body: "lists:foldr(fun(X, Acc) -> Acc + X end, A1, [1, 2, 3])" },
    Entry { module: "lists", spec: "lists:takewhile/2", args: &[4], body: "lists:takewhile(fun(X) -> X < A1 end, [1, 2, 3, 4, 5])" },
    Entry { module: "lists", spec: "lists:concat/1", args: &[7], body: "lists:concat([A1, \"-\", 2])" },
    Entry { module: "lists", spec: "lists:keysort/2", args: &[9, 8], body: "lists:keysort(1, [{2, A1}, {1, A2}])" },
    Entry { module: "lists", spec: "lists:zipwith/3", args: &[1, 2, 3], body: "lists:zipwith(fun(X, Y) -> X + Y end, [1, 2, 3], [A1, A2, A3])" },
    Entry { module: "lists", spec: "lists:search/2", args: &[2], body: "lists:search(fun(X) -> X > A1 end, [1, 2, 3, 4])" },
    Entry { module: "lists", spec: "lists:suffix/2", args: &[1, 2, 3], body: "case lists:suffix([A2, A3], [A1, A2, A3]) of true -> 1; false -> 0 end" },
    Entry { module: "lists", spec: "lists:merge/2", args: &[5], body: "lists:merge([1, 3], [2, A1])" },
    Entry { module: "lists", spec: "lists:droplast/1", args: &[3], body: "lists:droplast([1, 2, A1])" },
    Entry { module: "lists", spec: "lists:mapfoldl/3", args: &[10], body: "lists:mapfoldl(fun(X, Acc) -> {X + A1, Acc + X} end, 0, [1, 2, 3])" },
    Entry { module: "lists", spec: "lists:zipwith3/4", args: &[1], body: "lists:zipwith3(fun(X, Y, Z) -> X + Y + Z end, [1], [2], [A1])" },
    Entry { module: "lists", spec: "lists:keyreplace/4", args: &[3], body: "lists:keyreplace(A1, 1, [{1, a}, {2, b}, {3, c}], {3, z})" },
    Entry { module: "lists", spec: "lists:unzip3/1", args: &[1, 2, 3], body: "lists:unzip3([{1, A1, 5}, {2, A2, 6}, {3, A3, 7}])" },
    Entry { module: "lists", spec: "lists:merge3/3", args: &[11, 12], body: "lists:merge3([1, 4], [2, A1], [3, A2])" },
    Entry { module: "lists", spec: "lists:ukeymerge/3", args: &[5], body: "lists:ukeymerge(1, [{1, a}], [{1, b}, {2, c}]) =:= [{1, b}, {2, c}]" },
    Entry { module: "lists", spec: "lists:sort/2", args: &[5], body: "lists:sort(fun(A, B) -> A =< B end, [3, 1, A1])" },
    Entry { module: "lists", spec: "lists:keymerge/3", args: &[5], body: "lists:keymerge(1, [{1, a}], [{2, b}])" },
    Entry { module: "lists", spec: "lists:seq/3", args: &[5], body: "length(lists:seq(1, 10, 3))" },
    // --- maps --------------------------------------------------------------------
    Entry { module: "maps", spec: "maps:size/1", args: &[1], body: "maps:size(maps:put(A1, 0, maps:new()))" },
    Entry { module: "maps", spec: "maps:is_key/2", args: &[1], body: "case maps:is_key(A1, #{1 => 10}) of true -> 1; false -> 0 end" },
    Entry { module: "maps", spec: "maps:values/1", args: &[9], body: "lists:sort(maps:values(#{1 => A1, 2 => 20}))" },
    Entry { module: "maps", spec: "maps:keys/1", args: &[5], body: "lists:sort(maps:keys(maps:merge(#{1 => a, 2 => A1}, #{3 => b})))" },
    Entry { module: "maps", spec: "maps:get/2", args: &[9], body: "maps:get(1, maps:update(1, A1, #{1 => 0}))" },
    Entry { module: "maps", spec: "maps:size/1", args: &[15], body: "maps:size(maps:filter(fun(_K, V) -> V > A1 end, #{1 => 10, 2 => 20}))" },
    Entry { module: "maps", spec: "maps:size/1", args: &[1], body: "maps:size(maps:without([A1], #{1 => a, 2 => b}))" },
    Entry { module: "maps", spec: "maps:next/1", args: &[7], body: "maps:next(maps:iterator(#{1 => A1}))" },
    Entry { module: "maps", spec: "maps:get/2", args: &[5], body: "maps:get(1, maps:update_with(1, fun(V) -> V + A1 end, #{1 => 5}))" },
    Entry { module: "maps", spec: "maps:to_list/1", args: &[0], body: "lists:sort(maps:to_list(maps:groups_from_list(fun(X) -> X rem 2 end, fun(X) -> X end, [1, 2, 3, 4])))" },
    Entry { module: "maps", spec: "maps:keys/1", args: &[1, 2], body: "lists:sort(maps:keys(maps:intersect(#{1 => a, 2 => b}, #{2 => c, 3 => d})))" },
    Entry { module: "maps", spec: "maps:get/2", args: &[42], body: "maps:get(2, maps:from_keys([1, 2, A1], 7))" },
    Entry { module: "maps", spec: "maps:is_key/2", args: &[5], body: "maps:is_key(1, maps:from_list([{1, A1}]))" },
    Entry { module: "maps", spec: "maps:get/2", args: &[5], body: "maps:get(2, maps:update(1, A1, #{1 => 0, 2 => 9}))" },
    Entry { module: "maps", spec: "maps:get/2", args: &[5], body: "maps:get(3, maps:with([1, 3], #{1 => A1, 2 => b, 3 => c}))" },
    // --- math --------------------------------------------------------------------
    Entry { module: "math", spec: "math:pi/0", args: &[0], body: "trunc(math:pi() * 1000000)" },
    Entry { module: "math", spec: "math:exp/1", args: &[1], body: "trunc(math:exp(A1) * 1000)" },
    Entry { module: "math", spec: "math:log10/1", args: &[100], body: "trunc(math:log10(A1) * 1000000)" },
    Entry { module: "math", spec: "math:tan/1", args: &[1], body: "trunc(math:tan(A1) * 1000000)" },
    Entry { module: "math", spec: "math:acos/1", args: &[1], body: "trunc(math:acos(A1) * 1000000)" },
    Entry { module: "math", spec: "math:sinh/1", args: &[1], body: "trunc(math:sinh(A1) * 1000000)" },
    Entry { module: "math", spec: "math:asinh/1", args: &[1], body: "trunc(math:asinh(A1) * 1000000)" },
    Entry { module: "math", spec: "math:ceil/1", args: &[7], body: "math:ceil(A1 * 1.5)" },
    Entry { module: "math", spec: "math:pow/2", args: &[3, 2], body: "math:pow(A1, A2)" },
    Entry { module: "math", spec: "math:pow/2", args: &[3], body: "trunc(math:pow(A1, 3) * 1000)" },
    Entry { module: "math", spec: "math:atan/1", args: &[0], body: "trunc(math:atan(A1) * 1000000)" },
    Entry { module: "math", spec: "math:tanh/1", args: &[0], body: "trunc(math:tanh(A1) * 1000000)" },
    Entry { module: "math", spec: "math:acosh/1", args: &[3], body: "trunc(math:acosh(A1) * 1000000)" },
    Entry { module: "math", spec: "math:fmod/2", args: &[17], body: "trunc(math:fmod(A1, 5.0) * 1000)" },
    Entry { module: "math", spec: "math:log10/1", args: &[5], body: "trunc(math:log10(A1) * 1000000)" },
    Entry { module: "math", spec: "math:exp/1", args: &[5], body: "trunc(math:exp(A1) * 1000)" },
    // --- orddict -----------------------------------------------------------------
    Entry { module: "orddict", spec: "orddict:size/1", args: &[0], body: "orddict:size(orddict:new())" },
    Entry { module: "orddict", spec: "orddict:fetch_keys/1", args: &[0], body: "orddict:fetch_keys(orddict:from_list([{2, b}, {1, a}]))" },
    Entry { module: "orddict", spec: "orddict:take/2", args: &[1], body: "orddict:take(A1, orddict:from_list([{1, a}, {2, b}]))" },
    Entry { module: "orddict", spec: "orddict:map/2", args: &[1], body: "orddict:map(fun(_K, V) -> V * 2 end, orddict:from_list([{1, 3}]))" },
    Entry { module: "orddict", spec: "orddict:append/3", args: &[1], body: "orddict:append(A1, 5, orddict:from_list([{1, [1]}]))" },
    Entry { module: "orddict", spec: "orddict:update/3", args: &[1], body: "orddict:update(A1, fun(V) -> V + 1 end, orddict:from_list([{1, 1}]))" },
    Entry { module: "orddict", spec: "orddict:merge/3", args: &[1], body: "orddict:merge(fun(_K, X, Y) -> X + Y end, orddict:from_list([{1, 10}]), orddict:from_list([{1, 5}, {2, 5}]))" },
    Entry { module: "orddict", spec: "orddict:store/3", args: &[1], body: "orddict:store(A1, 9, orddict:from_list([{1, 1}, {2, 2}]))" },
    // --- ordsets -----------------------------------------------------------------
    Entry { module: "ordsets", spec: "ordsets:size/1", args: &[0], body: "ordsets:size(ordsets:new())" },
    Entry { module: "ordsets", spec: "ordsets:is_subset/2", args: &[3], body: "ordsets:is_subset(ordsets:from_list([1]), ordsets:from_list([1, A1]))" },
    Entry { module: "ordsets", spec: "ordsets:is_equal/2", args: &[3], body: "ordsets:is_equal(ordsets:from_list([1, A1]), ordsets:from_list([A1, 1]))" },
    Entry { module: "ordsets", spec: "ordsets:fold/3", args: &[3], body: "ordsets:fold(fun(X, Acc) -> Acc + X end, 0, ordsets:from_list([1, A1]))" },
    Entry { module: "ordsets", spec: "ordsets:map/2", args: &[3], body: "ordsets:map(fun(X) -> X * 2 end, ordsets:from_list([1, A1]))" },
    Entry { module: "ordsets", spec: "ordsets:is_element/2", args: &[3], body: "ordsets:is_element(A1, ordsets:from_list([1, 2, 3]))" },
    // --- os ----------------------------------------------------------------------
    Entry { module: "os", spec: "os:type/0", args: &[0], body: "os:type()" },
    Entry { module: "os", spec: "os:type/0", args: &[0], body: "element(1, os:type())" },
    // --- proplists ---------------------------------------------------------------
    Entry { module: "proplists", spec: "proplists:get_value/2", args: &[1], body: "proplists:get_value(A1, [{1, 10}, {2, 20}])" },
    Entry { module: "proplists", spec: "proplists:get_bool/2", args: &[1], body: "proplists:get_bool(A1, [{1, true}, {2, false}])" },
    Entry { module: "proplists", spec: "proplists:delete/2", args: &[1], body: "proplists:delete(A1, [{1, a}, {2, b}])" },
    Entry { module: "proplists", spec: "proplists:compact/1", args: &[1], body: "proplists:compact([a, {1, 1}, b, {2, 2}])" },
    Entry { module: "proplists", spec: "proplists:unfold/1", args: &[1], body: "proplists:unfold([{1, x}, {2, y}])" },
    Entry { module: "proplists", spec: "proplists:property/1", args: &[1], body: "proplists:property({a, 1})" },
    Entry { module: "proplists", spec: "proplists:to_map/1", args: &[1], body: "maps:size(proplists:to_map([{1, a}, {2, b}]))" },
    Entry { module: "proplists", spec: "proplists:expand/2", args: &[1], body: "proplists:expand([{A1, [x, y]}], [{1, a}])" },
    Entry { module: "proplists", spec: "proplists:delete/2", args: &[1], body: "proplists:delete(A1, [{1, a}, {1, b}])" },
    Entry { module: "proplists", spec: "proplists:lookup_all/2", args: &[1], body: "proplists:lookup_all(A1, [{1, a}, {1, b}])" },
    // --- queue -------------------------------------------------------------------
    Entry { module: "queue", spec: "queue:len/1", args: &[0], body: "queue:len(queue:new())" },
    Entry { module: "queue", spec: "queue:peek/1", args: &[5], body: "queue:peek(queue:in(A1, queue:new()))" },
    Entry { module: "queue", spec: "queue:head/1", args: &[5], body: "queue:head(queue:in(A1, queue:from_list([1])))" },
    Entry { module: "queue", spec: "queue:init/1", args: &[5], body: "queue:init(queue:in(A1, queue:from_list([1])))" },
    Entry { module: "queue", spec: "queue:member/2", args: &[5], body: "queue:member(A1, queue:from_list([1, 2, 3]))" },
    Entry { module: "queue", spec: "queue:fold/3", args: &[5], body: "queue:fold(fun(X, Acc) -> Acc + X end, 0, queue:from_list([1, 2, A1]))" },
    Entry { module: "queue", spec: "queue:delete/2", args: &[5], body: "queue:delete(A1, queue:from_list([1, A1, 3]))" },
    Entry { module: "queue", spec: "queue:any/2", args: &[5], body: "queue:any(fun(X) -> X > 2 end, queue:from_list([1, A1]))" },
    Entry { module: "queue", spec: "queue:filter/2", args: &[5], body: "queue:filter(fun(X) -> X > 1 end, queue:from_list([1, 2, A1])) =:= queue:from_list([2, A1])" },
    Entry { module: "queue", spec: "queue:get_r/1", args: &[5], body: "queue:get_r(queue:from_list([1, A1]))" },
    // --- rand --------------------------------------------------------------------
    Entry { module: "rand", spec: "rand:uniform_s/2", args: &[5], body: "element(1, rand:uniform_s(1000, rand:seed_s(exsss, {A1, 2, 3})))" },
    Entry { module: "rand", spec: "rand:bytes_s/2", args: &[7], body: "element(1, rand:bytes_s(4, rand:seed_s(exsss, {A1, 2, 3})))" },
    Entry { module: "rand", spec: "rand:normal_s/1", args: &[7], body: "trunc(element(1, rand:normal_s(rand:seed_s(exsss, {A1, 2, 3}))) * 100000)" },
    Entry { module: "rand", spec: "rand:uniform_real_s/1", args: &[9], body: "trunc(element(1, rand:uniform_real_s(rand:seed_s(exsss, {1, A1, 3}))) * 1000000)" },
    Entry { module: "rand", spec: "rand:uniform_s/1", args: &[5], body: "element(1, rand:uniform_s(rand:seed_s(exsss, {A1, 2, 3})))" },
    Entry { module: "rand", spec: "rand:shuffle_s/2", args: &[5], body: "element(1, rand:shuffle_s([1, 2, 3, 4], rand:seed_s(exsss, {A1, 2, 3})))" },
    Entry { module: "rand", spec: "rand:uniform/1", args: &[5], body: "begin rand:seed(exsss, {A1, 2, 3}), rand:uniform(1000) end" },
    // --- re ----------------------------------------------------------------------
    Entry { module: "re", spec: "re:version/0", args: &[0], body: "re:version()" },
    Entry { module: "re", spec: "re:split/2", args: &[0], body: "re:split(\"a,b\", \",\")" },
    // --- sets --------------------------------------------------------------------
    Entry { module: "sets", spec: "sets:size/1", args: &[3], body: "sets:size(sets:from_list([1, 2, A1]))" },
    Entry { module: "sets", spec: "sets:to_list/1", args: &[3], body: "lists:sort(sets:to_list(sets:from_list([3, 1, A1])))" },
    Entry { module: "sets", spec: "sets:is_element/2", args: &[3], body: "sets:is_element(A1, sets:from_list([1, 2, 3]))" },
    Entry { module: "sets", spec: "sets:size/1", args: &[3], body: "sets:size(sets:add_element(A1, sets:from_list([1, 2])))" },
    Entry { module: "sets", spec: "sets:size/1", args: &[3], body: "sets:size(sets:union(sets:from_list([1]), sets:from_list([A1])))" },
    // --- sofs --------------------------------------------------------------------
    Entry { module: "sofs", spec: "sofs:to_external/1", args: &[1], body: "lists:sort(sofs:to_external(sofs:from_term([A1, 2, 3])))" },
    Entry { module: "sofs", spec: "sofs:to_external/1", args: &[1], body: "lists:sort(sofs:to_external(sofs:union(sofs:from_term([1, 2]), sofs:from_term([2, A1]))))" },
    Entry { module: "sofs", spec: "sofs:to_external/1", args: &[1], body: "lists:sort(sofs:to_external(sofs:symdiff(sofs:from_term([1, 2]), sofs:from_term([2, A1]))))" },
    Entry { module: "sofs", spec: "sofs:is_equal/2", args: &[1], body: "sofs:is_equal(sofs:from_term([1, A1]), sofs:from_term([A1, 1]))" },
    Entry { module: "sofs", spec: "sofs:is_empty_set/1", args: &[1], body: "sofs:is_empty_set(sofs:from_term([]))" },
    Entry { module: "sofs", spec: "sofs:to_external/1", args: &[1], body: "lists:sort(sofs:to_external(sofs:product(sofs:from_term([1, 2]), sofs:from_term([A1]))))" },
    Entry { module: "sofs", spec: "sofs:no_elements/1", args: &[1], body: "sofs:no_elements(sofs:from_term([]))" },
    Entry { module: "sofs", spec: "sofs:to_external/1", args: &[1], body: "lists:sort(sofs:to_external(sofs:converse(sofs:from_term([{1, 2}]))))" },
    // --- string ------------------------------------------------------------------
    Entry { module: "string", spec: "string:to_upper/1", args: &[5], body: "string:to_upper(\"abcde\")" },
    Entry { module: "string", spec: "string:tokens/2", args: &[3], body: "string:tokens(\"a,b,c\", \",\")" },
    Entry { module: "string", spec: "string:strip/1", args: &[0], body: "string:strip(\"  x  \")" },
    Entry { module: "string", spec: "string:centre/2", args: &[0], body: "string:centre(\"abcdef\", 8)" },
    Entry { module: "string", spec: "string:concat/2", args: &[0], body: "string:concat(\"ab\", \"cd\")" },
    Entry { module: "string", spec: "string:span/2", args: &[1], body: "string:span(\"aab\", \"a\")" },
    Entry { module: "string", spec: "string:rchr/2", args: &[1], body: "string:rchr(\"abcb\", $b)" },
    Entry { module: "string", spec: "string:slice/3", args: &[5], body: "string:slice(\"abcdef\", 2, 3)" },
    Entry { module: "string", spec: "string:take/2", args: &[5], body: "string:take(\"aabx\", \"ab\")" },
    Entry { module: "string", spec: "string:split/3", args: &[0], body: "string:split(\"a,b,c\", \",\", all)" },
    Entry { module: "string", spec: "string:prefix/2", args: &[0], body: "string:prefix(\"abcdef\", \"xy\")" },
    Entry { module: "string", spec: "string:uppercase/1", args: &[0], body: "string:uppercase(\"abc\")" },
    Entry { module: "string", spec: "string:length/1", args: &[0], body: "string:length(\"abcd\")" },
    Entry { module: "string", spec: "string:trim/2", args: &[5], body: "string:trim(\"  abc  \", leading)" },
    Entry { module: "string", spec: "string:nth_lexeme/3", args: &[0], body: "string:nth_lexeme(\"a b c\", 2, \" \")" },
    Entry { module: "string", spec: "string:to_float/1", args: &[0], body: "element(1, string:to_float(\"1.5\"))" },
    Entry { module: "string", spec: "string:len/1", args: &[0], body: "string:len(\"abcd\")" },
    Entry { module: "string", spec: "string:replace/3", args: &[0], body: "string:replace(\"aXbXc\", \"X\", \"-\")" },
    Entry { module: "string", spec: "string:to_lower/1", args: &[0], body: "string:to_lower(\"ABC\")" },
    Entry { module: "string", spec: "string:find/2", args: &[0], body: "string:find(\"abcdef\", \"cd\")" },
    // --- unicode -----------------------------------------------------------------
    Entry { module: "unicode", spec: "unicode:characters_to_list/1", args: &[0], body: "unicode:characters_to_list(integer_to_list(A1 + 65))" },
    Entry { module: "unicode", spec: "unicode:characters_to_list/1", args: &[0], body: "unicode:characters_to_list(<<65, 66, 67>>)" },
    Entry { module: "unicode", spec: "unicode:bom_to_encoding/1", args: &[0], body: "unicode:bom_to_encoding(<<254, 255>>)" },
    Entry { module: "unicode", spec: "unicode:category/1", args: &[0], body: "unicode:category($a)" },
    Entry { module: "unicode", spec: "unicode:is_whitespace/1", args: &[0], body: "unicode:is_whitespace($\\s)" },
    Entry { module: "unicode", spec: "unicode:characters_to_nfkd_list/1", args: &[0], body: "length(unicode:characters_to_nfkd_list(\"abc\"))" },
    Entry { module: "unicode", spec: "unicode:is_id_continue/1", args: &[0], body: "unicode:is_id_continue($0)" },
    Entry { module: "unicode", spec: "unicode:bom_to_encoding/1", args: &[0], body: "unicode:bom_to_encoding(<<255, 254>>)" },
    Entry { module: "unicode", spec: "unicode:encoding_to_bom/1", args: &[0], body: "unicode:encoding_to_bom(utf8)" },
    // --- uri_string --------------------------------------------------------------
    Entry { module: "uri_string", spec: "uri_string:parse/1", args: &[0], body: "maps:size(uri_string:parse(\"http://example.com/path\"))" },
    Entry { module: "uri_string", spec: "uri_string:percent_decode/1", args: &[0], body: "uri_string:percent_decode(<<\"a%20b\">>)" },
    Entry { module: "uri_string", spec: "uri_string:unquote/1", args: &[0], body: "uri_string:unquote(\"a%20b\")" },
    Entry { module: "uri_string", spec: "uri_string:normalize/1", args: &[0], body: "uri_string:normalize(\"/a/./b/../c\")" },
    Entry { module: "uri_string", spec: "uri_string:is_host/1", args: &[0], body: "uri_string:is_host(\"example.com\")" },
];

/// The Erlang parameter list for an `arity`-argument wrapper function.
fn params(arity: usize) -> String {
    (1..=arity)
        .map(|index| format!("A{index}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Generate, group and leak one wrapper module per distinct real module.
///
/// The returned map is keyed by the real module name; each value is the source of a
/// `sweep_<module>` wrapper exporting one `f<entry-index>` per entry of that module. Only
/// the source (and, in [`realize`], the module/function names) is leaked — the sweep is a
/// single, bounded test process, so the leaked strings are reclaimed at process exit.
fn wrapper_sources() -> BTreeMap<&'static str, &'static str> {
    let mut by_module: BTreeMap<&'static str, Vec<usize>> = BTreeMap::new();
    for (index, entry) in SWEEP.iter().enumerate() {
        by_module.entry(entry.module).or_default().push(index);
    }

    let mut sources = BTreeMap::new();
    for (module, indices) in by_module {
        let mut source = format!("-module(sweep_{module}).\n");
        let exports: Vec<String> = indices
            .iter()
            .map(|&index| format!("f{index}/{}", SWEEP[index].args.len()))
            .collect();
        source.push_str(&format!("-export([{}]).\n", exports.join(", ")));
        for &index in &indices {
            let entry = &SWEEP[index];
            source.push_str(&format!(
                "f{index}({}) -> {}.\n",
                params(entry.args.len()),
                entry.body
            ));
        }
        let leaked: &'static str = Box::leak(source.into_boxed_str());
        sources.insert(module, leaked);
    }
    sources
}

/// Build the harness [`Case`] for one sweep entry, resolving the name of its generated
/// wrapper module + function.
fn case_for(index: usize, entry: &Entry, sources: &BTreeMap<&'static str, &'static str>) -> Case {
    let module: &'static str = Box::leak(format!("sweep_{}", entry.module).into_boxed_str());
    let function: &'static str = Box::leak(format!("f{index}").into_boxed_str());
    Case {
        id: "module-sweep",
        description: "a real system module runs end to end under both runtimes",
        contracts: CONTRACTS,
        kind: CaseKind::Module(ModuleRun {
            source: sources[entry.module],
            module,
            function,
            arity: entry.args.len() as u32,
            args: entry.args,
            // The sweep runs plain system modules: no C/NIF fixture (contract A.3) is needed.
            nif: None,
        }),
        // No stdout golden: the sweep records *agreement*, not a value. The oracle is only
        // required to exit 0 (a well-formed entry); a non-zero oracle exit is corpus drift.
        golden: Golden {
            stdout: None,
            exit_code: 0,
        },
    }
}

/// The stdout of an observed side, trimmed, for the per-entry report.
fn observed_stdout(side: &differential::SideResult) -> String {
    match side.observation() {
        Some(observation) => String::from_utf8_lossy(&observation.stdout)
            .trim_end()
            .to_string(),
        None => String::new(),
    }
}

#[test]
fn real_module_execution_sweep_never_mismatches() {
    // The sweep must stay substantially wider than the shipped corpus group it mirrors.
    assert!(
        SWEEP.len() > 250,
        "the sweep must be a wide sample of real system modules (got {} entries)",
        SWEEP.len()
    );

    if let Err(error) = resolve_oracle() {
        eprintln!("SKIP: module sweep — no oracle available ({error})");
        return;
    }
    match module_build::resolve_erlc() {
        Ok(Some(_)) => {}
        Ok(None) => {
            eprintln!("SKIP: module sweep — no erlc available (the wrappers cannot be compiled)");
            return;
        }
        Err(error) => {
            eprintln!("SKIP: module sweep — erlc resolution failed ({error})");
            return;
        }
    }

    // An explicit context so the sweep observes the freshly built debug `beam_rt` rather
    // than the (stale-prone) `release`-first workspace default.
    let context = RuntimeContext {
        oracle: resolve_oracle().expect("the oracle resolves (checked above)"),
        rust: common::fresh_rust_spec(),
    };
    let sources = wrapper_sources();

    let mut agree = 0usize;
    let mut unsupported = 0usize;
    let mut mismatches: Vec<String> = Vec::new();
    // Per real module: (agree, unsupported).
    let mut per_module: BTreeMap<&'static str, (usize, usize)> = BTreeMap::new();

    for (index, entry) in SWEEP.iter().enumerate() {
        let case = case_for(index, entry, &sources);
        let outcome = run_case_in(&context, &case).expect("the harness runs a sweep case");
        let counts = per_module.entry(entry.module).or_insert((0, 0));
        match outcome.verdict {
            Verdict::Agree => {
                agree += 1;
                counts.0 += 1;
                println!(
                    "[agree]       {} {:?} -> {}",
                    entry.spec,
                    entry.args,
                    observed_stdout(&outcome.oracle)
                );
            }
            Verdict::Unsupported => {
                unsupported += 1;
                counts.1 += 1;
                let reason = outcome
                    .rust
                    .unsupported_reason()
                    .unwrap_or("(no reason)");
                println!(
                    "[unsupported] {} {:?} — {reason}",
                    entry.spec, entry.args
                );
            }
            other => {
                // A real divergence (or a skip the pre-checks did not foresee) is loud.
                mismatches.push(format!(
                    "{} {:?}: oracle=<{}> rust=<{}> verdict={} diffs={:?}",
                    entry.spec,
                    entry.args,
                    observed_stdout(&outcome.oracle),
                    observed_stdout(&outcome.rust),
                    other.as_str(),
                    outcome.diffs,
                ));
            }
        }
    }

    println!();
    for (module, (a, u)) in &per_module {
        println!("  {module:<12} {a} agree, {u} unsupported");
    }
    println!(
        "SWEEP: {} entries across {} real modules — {agree} agree, {unsupported} unsupported, {} not-agreeing",
        SWEEP.len(),
        per_module.len(),
        mismatches.len()
    );

    assert!(
        mismatches.is_empty(),
        "the module sweep must never mismatch — {} entry/entries diverged or could not run:\n  {}",
        mismatches.len(),
        mismatches.join("\n  ")
    );
}
