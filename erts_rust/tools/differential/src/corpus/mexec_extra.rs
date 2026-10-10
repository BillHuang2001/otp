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

//! Contract **A.2 / A.7** — real system modules execute, widened: a second block of
//! module-run cases over the stdlib/kernel surface.
//!
//! A continuation of [`super::mexec`]: each case is a tiny embedded wrapper module (compiled
//! with the system `erlc`) whose integer-arg exported function calls a *real* system module,
//! driven under both runtimes. Case ids use the `mexec_extra/…` prefix. See the node
//! `CONTEXT.md` ("How to add a corpus case").
//!
//! The wrappers reach a broad slice of the installed standard library — `lists`, `maps`,
//! `binary`/bitstring operators, `string`, `unicode`, `base64`, `proplists`, `orddict`,
//! `gb_trees`, `gb_sets`, `ordsets`, `sets`, `queue`, `array`, `calendar`, `math`,
//! `filename`, `io_lib_pretty` and `digraph` — each function building whatever list/tuple
//! the real call needs internally and returning an **integer** (the shape a module-run
//! golden pins). Every case is **verified against both runtimes** (`differential run <id>`
//! → `AGREE`), so the corpus never claims agreement it does not have. Each case is tagged
//! [`Contract::BeamAsm`] (a compiled `.beam` executes) and [`Contract::OtpApps`] (a real
//! OTP application module is loaded and run).
//!
//! Deliberately avoided (documented divergences, not agreement): `lists:usort/2` with an
//! ascending comparator (the Rust runtime produces a wrong *result*), the `math:erf/1`/
//! `erfc/1` pair (1–5 ULP) and the whole `filelib` module (the Rust runtime reports it as a
//! capability gap, not a value). Any golden that renders a map or a multiset here is reduced
//! to an integer (size / sorted-key or sorted-value head), never an order-unstable `~w`.

use super::{Case, CaseKind, Contract, Golden, ModuleRun};

/// The `lists` wrapper: a broad slice of the real list module over integer-returning
/// wrappers (`sort/1,2`, `foldl/3`, `foldr/3`, `partition/2`, `zip/2,3`, `mapfoldl/3`,
/// `filter/2`, `takewhile/2`, `usort/1`, …).
const MEXEC2_LISTS: &str = "-module(mexec2_lists).\n\
     -export([sort_len/1, sort_fun_head/1, foldl_sum/1, foldr_sum/1, partition_left_len/1,\n\
     \x20        zip_len/1, zipwith_head/1, unzip_left_len/1, nth/1, sum/1, max/1,\n\
     \x20        flatten_len/1, sublist_len/1, last/1, keyfind/1, mapfoldl_sum/1,\n\
     \x20        filter_len/1, takewhile_len/1, usort_len/1]).\n\
     sort_len(N) -> length(lists:sort([N, 1, 2, 1])).\n\
     sort_fun_head(N) -> hd(lists:sort(fun(A, B) -> A >= B end, [N, 1, 3])).\n\
     foldl_sum(N) -> lists:foldl(fun(X, Acc) -> X + Acc end, 0, lists:seq(1, N)).\n\
     foldr_sum(N) -> lists:foldr(fun(X, Acc) -> X + Acc end, 0, lists:seq(1, N)).\n\
     partition_left_len(N) -> length(element(1, lists:partition(fun(X) -> X rem 2 =:= 0 end, lists:seq(1, N)))).\n\
     zip_len(N) -> length(lists:zip(lists:seq(1, N), lists:seq(1, N))).\n\
     zipwith_head(N) -> hd(lists:zipwith(fun(A, B) -> A + B end, [N, 2], [4, 5])).\n\
     unzip_left_len(N) -> length(element(1, lists:unzip(lists:zip(lists:seq(1, N), lists:seq(1, N))))).\n\
     nth(N) -> lists:nth(N, [10, 20, 30, 40]).\n\
     sum(N) -> lists:sum(lists:seq(1, N)).\n\
     max(N) -> lists:max([N, 1, 2]).\n\
     flatten_len(N) -> length(lists:flatten([[N, 1], [2, 3]])).\n\
     sublist_len(N) -> length(lists:sublist(lists:seq(1, N), 3)).\n\
     last(N) -> lists:last([1, 2, N]).\n\
     keyfind(N) -> {_, V} = lists:keyfind(N, 1, [{1, 10}, {2, 20}, {3, 30}]), V.\n\
     mapfoldl_sum(N) -> element(2, lists:mapfoldl(fun(X, Acc) -> {X, Acc + X} end, 0, lists:seq(1, N))).\n\
     filter_len(N) -> length(lists:filter(fun(X) -> X > N end, lists:seq(1, 4))).\n\
     takewhile_len(N) -> length(lists:takewhile(fun(X) -> X < N end, [1, 2, 3, 4])).\n\
     usort_len(N) -> length(lists:usort([N, 3, 1, 2, 1, 3])).\n";

/// The `maps` wrapper: the real map module over small maps (always reduced to an integer,
/// so no `~w` of an order-unstable map is ever asserted).
const MEXEC2_MAPS: &str = "-module(mexec2_maps).\n\
     -export([from_list_size/1, keys_head/1, get_default/1, merge_size/1, without_size/1,\n\
     \x20        fold_sum/1, find_value/1, values_head/1, put_size/1, update_value/1,\n\
     \x20        map_sum/1, filter_size/1]).\n\
     from_list_size(N) -> maps:size(maps:from_list([{1, N}, {2, 20}, {3, 30}])).\n\
     keys_head(N) -> hd(lists:sort(maps:keys(maps:from_list([{N, 1}, {2, 2}, {3, 3}])))).\n\
     get_default(N) -> maps:get(N, maps:from_list([{1, 10}, {2, 20}]), 99).\n\
     merge_size(N) -> maps:size(maps:merge(maps:from_list([{1, N}]), maps:from_list([{2, 2}]))).\n\
     without_size(N) -> maps:size(maps:without([2], maps:from_list([{1, N}, {2, 2}, {3, 3}]))).\n\
     fold_sum(N) -> maps:fold(fun(_K, V, Acc) -> V + Acc end, 0, maps:from_list([{1, N}, {2, 2}])).\n\
     find_value(N) -> case maps:find(N, maps:from_list([{1, 10}, {2, 20}])) of {ok, V} -> V; error -> 0 end.\n\
     values_head(N) -> hd(lists:sort(maps:values(maps:from_list([{1, N}, {2, 5}])))).\n\
     put_size(N) -> maps:size(maps:put(4, N, maps:from_list([{1, 1}, {2, 2}]))).\n\
     update_value(N) -> maps:get(1, maps:update(1, N, maps:from_list([{1, 0}, {2, 2}]))).\n\
     map_sum(N) -> lists:sum(maps:values(maps:map(fun(_K, V) -> V + N end, maps:from_list([{1, 1}, {2, 2}])))).\n\
     filter_size(N) -> maps:size(maps:filter(fun(_K, V) -> V > N end, maps:from_list([{1, 10}, {2, 20}]))).\n";

/// The `binary`/bitstring wrapper: literal construction, `binary:at/2`, `binary:part/3`,
/// `binary:match/2` and the `bit_size/1`/`band` operators.
const MEXEC2_BINARY: &str = "-module(mexec2_binary).\n\
     -export([size/1, at/1, part_size/1, match_pos/1, bitsize/1, bit_construct/1, band_bits/1]).\n\
     size(N) -> byte_size(list_to_binary(lists:seq(1, N))).\n\
     at(N) -> binary:at(list_to_binary(lists:seq(1, 8)), N).\n\
     part_size(N) -> byte_size(binary:part(list_to_binary(lists:seq(1, 8)), 1, N)).\n\
     match_pos(N) -> case binary:match(list_to_binary(lists:seq(1, 8)), list_to_binary([N])) of {P, _} -> P; nomatch -> -1 end.\n\
     bitsize(N) -> bit_size(<<N:16>>).\n\
     bit_construct(N) -> bit_size(<<N:4, 0:4>>).\n\
     band_bits(N) -> N band 6.\n";

/// The `string` wrapper: the real character-list module (`length/1`, `uppercase/1`,
/// `slice/3`, `concat/2`), itself a `unicode` consumer.
const MEXEC2_STRING: &str = "-module(mexec2_string).\n\
     -export([str_len/1, upper_head/1, concat_head/1]).\n\
     str_len(N) -> string:length(lists:duplicate(N, $x)).\n\
     upper_head(N) -> hd(string:uppercase([$a + N])).\n\
     concat_head(N) -> hd(string:concat(\"a\", integer_to_list(N))).\n";

/// The `unicode` wrapper: `characters_to_binary/1` on a multi-byte code point.
const MEXEC2_UNICODE: &str = "-module(mexec2_unicode).\n\
     -export([bin_size/1]).\n\
     bin_size(N) -> byte_size(unicode:characters_to_binary([$a, 200 + N])).\n";

/// The `base64` wrapper: the real codec, whose result is an off-heap binary
/// (`encode/1`, `decode/1`).
const MEXEC2_BASE64: &str = "-module(mexec2_base64).\n\
     -export([encode_size/1, decode_size/1]).\n\
     encode_size(N) -> byte_size(base64:encode(list_to_binary(lists:seq(1, N)))).\n\
     decode_size(N) -> byte_size(base64:decode(base64:encode(list_to_binary(lists:seq(1, N))))).\n";

/// The `proplists` wrapper: `get_value/3` and `delete/2`.
const MEXEC2_PROPLISTS: &str = "-module(mexec2_proplists).\n\
     -export([get_hit/1, delete_len/1]).\n\
     get_hit(N) -> proplists:get_value(N, [{1, 10}, {2, 20}], 0).\n\
     delete_len(N) -> length(proplists:delete(N, [1, 2, 3])).\n";

/// The `orddict` wrapper: `fetch/2` and `store/3`.
const MEXEC2_ORDDICT: &str = "-module(mexec2_orddict).\n\
     -export([fetch/1, store_size/1]).\n\
     fetch(N) -> orddict:fetch(N, orddict:from_list([{1, 10}, {2, 20}])).\n\
     store_size(N) -> orddict:size(orddict:store(2, N, orddict:from_list([{1, a}]))).\n";

/// The `gb_trees` wrapper: `from_orddict/1` and `take_smallest/1` on a balanced tree.
const MEXEC2_GB_TREES: &str = "-module(mexec2_gb_trees).\n\
     -export([from_orddict_size/1, take_smallest_value/1]).\n\
     from_orddict_size(N) -> gb_trees:size(gb_trees:from_orddict([{1, 1}, {N, N}])).\n\
     take_smallest_value(N) -> element(2, gb_trees:take_smallest(gb_trees:insert(N, 1, gb_trees:insert(5, 2, gb_trees:empty())))).\n";

/// The `gb_sets` wrapper: `union/2` and `largest/1`.
const MEXEC2_GB_SETS: &str = "-module(mexec2_gb_sets).\n\
     -export([union_size/1, largest/1]).\n\
     union_size(N) -> gb_sets:size(gb_sets:union(gb_sets:from_list([1, 2]), gb_sets:from_list([2, N]))).\n\
     largest(N) -> gb_sets:largest(gb_sets:from_list([N, 9, 8])).\n";

/// The `ordsets` wrapper: `intersection/2` and `subtract/2`.
const MEXEC2_ORDSETS: &str = "-module(mexec2_ordsets).\n\
     -export([intersection_size/1, subtract_size/1]).\n\
     intersection_size(N) -> length(ordsets:intersection(ordsets:from_list([1, N]), ordsets:from_list([N, 3]))).\n\
     subtract_size(N) -> length(ordsets:subtract(ordsets:from_list([1, N, 3]), ordsets:from_list([N]))).\n";

/// The `sets` wrapper: `from_list/1` and `union/2`.
const MEXEC2_SETS: &str = "-module(mexec2_sets).\n\
     -export([from_list_size/1, union_size/1]).\n\
     from_list_size(N) -> sets:size(sets:from_list([1, 2, N, 2])).\n\
     union_size(N) -> sets:size(sets:union(sets:from_list([1, 2]), sets:from_list([2, N]))).\n";

/// The `queue` wrapper: `from_list/1` + `len/1`.
const MEXEC2_QUEUE: &str = "-module(mexec2_queue).\n\
     -export([len/1]).\n\
     len(N) -> queue:len(queue:from_list(lists:seq(1, N))).\n";

/// The `array` wrapper: `from_list/1` + `size/1` and `to_list/1`.
const MEXEC2_ARRAY: &str = "-module(mexec2_array).\n\
     -export([size/1, to_list_len/1]).\n\
     size(N) -> array:size(array:from_list(lists:seq(1, N))).\n\
     to_list_len(N) -> length(array:to_list(array:from_list(lists:seq(1, N)))).\n";

/// The `calendar` wrapper: `is_leap_year/1` reduced to an integer.
const MEXEC2_CALENDAR: &str = "-module(mexec2_calendar).\n\
     -export([is_leap/1]).\n\
     is_leap(N) -> case calendar:is_leap_year(N) of true -> 1; false -> 0 end.\n";

/// The `math` wrapper: bit-exact float kernels only (`sqrt/1`, `pow/2`, `log2/1`),
/// each reduced to an integer. Never `erf/1`/`erfc/1` (1–5 ULP).
const MEXEC2_MATH: &str = "-module(mexec2_math).\n\
     -export([sqrt_trunc/1, pow_trunc/1, log2_trunc/1]).\n\
     sqrt_trunc(N) -> trunc(math:sqrt(N)).\n\
     pow_trunc(N) -> trunc(math:pow(2, N)).\n\
     log2_trunc(N) -> trunc(math:log2(N)).\n";

/// The `filename` wrapper: `basename/1` and `dirname/1` over an integer-bearing path.
const MEXEC2_FILENAME: &str = "-module(mexec2_filename).\n\
     -export([basename_len/1, dirname_len/1]).\n\
     basename_len(N) -> length(filename:basename(\"dir/file\" ++ integer_to_list(N) ++ \".txt\")).\n\
     dirname_len(N) -> length(filename:dirname(\"/a/b\" ++ integer_to_list(N))).\n";

/// The `io_lib_pretty` wrapper: `print/1` of a small list, flattened to a length.
const MEXEC2_IO_LIB_PRETTY: &str = "-module(mexec2_io_lib_pretty).\n\
     -export([print_len/1]).\n\
     print_len(N) -> length(lists:flatten(io_lib_pretty:print([N, 1, 2]))).\n";

/// The `digraph` wrapper: the real directed-graph module (graph-first API), observing the
/// vertex/edge set it builds.
const MEXEC2_DIGRAPH: &str = "-module(mexec2_digraph).\n\
     -export([vertices_len/1, out_degree/1]).\n\
     vertices_len(N) -> G = digraph:new(), digraph:add_vertex(G, N), length(digraph:vertices(G)).\n\
     out_degree(N) -> G = digraph:new(), digraph:add_vertex(G, N), digraph:add_vertex(G, N + 1), digraph:add_edge(G, N, N + 1), digraph:out_degree(G, N).\n";

/// Build one real-module execution case.
fn case(
    id: &'static str,
    description: &'static str,
    source: &'static str,
    module: &'static str,
    function: &'static str,
    arity: u32,
    args: &'static [i64],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::BeamAsm, Contract::OtpApps],
        kind: CaseKind::Module(ModuleRun {
            source,
            module,
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

/// The A.2/A.7 widened real-system-module execution cases. Every one is verified `AGREE`.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- lists (a broad slice of the real list module) -------------------------
        case(
            "mexec_extra/lists_sort_len",
            "the real lists:sort/1 orders a list with a duplicate",
            MEXEC2_LISTS,
            "mexec2_lists",
            "sort_len",
            1,
            &[5],
            "4",
        ),
        case(
            "mexec_extra/lists_sort_fun_head",
            "the real lists:sort/2 with a descending fun puts the largest first",
            MEXEC2_LISTS,
            "mexec2_lists",
            "sort_fun_head",
            1,
            &[2],
            "3",
        ),
        case(
            "mexec_extra/lists_foldl_sum",
            "the real lists:foldl/3 accumulates left to right",
            MEXEC2_LISTS,
            "mexec2_lists",
            "foldl_sum",
            1,
            &[4],
            "10",
        ),
        case(
            "mexec_extra/lists_partition_left_len",
            "the real lists:partition/2 sizes the matching half",
            MEXEC2_LISTS,
            "mexec2_lists",
            "partition_left_len",
            1,
            &[4],
            "2",
        ),
        case(
            "mexec_extra/lists_zip_len",
            "the real lists:zip/2 pairs two equal-length ranges",
            MEXEC2_LISTS,
            "mexec2_lists",
            "zip_len",
            1,
            &[3],
            "3",
        ),
        case(
            "mexec_extra/lists_zipwith_head",
            "the real lists:zipwith/3 combines the two heads",
            MEXEC2_LISTS,
            "mexec2_lists",
            "zipwith_head",
            1,
            &[1],
            "5",
        ),
        case(
            "mexec_extra/lists_unzip_left_len",
            "the real lists:unzip/1 splits a zipped list back apart",
            MEXEC2_LISTS,
            "mexec2_lists",
            "unzip_left_len",
            1,
            &[2],
            "2",
        ),
        case(
            "mexec_extra/lists_nth",
            "the real lists:nth/2 reads the second element",
            MEXEC2_LISTS,
            "mexec2_lists",
            "nth",
            1,
            &[2],
            "20",
        ),
        case(
            "mexec_extra/lists_sum",
            "the real lists:sum/1 adds 1..N",
            MEXEC2_LISTS,
            "mexec2_lists",
            "sum",
            1,
            &[4],
            "10",
        ),
        case(
            "mexec_extra/lists_flatten_len",
            "the real lists:flatten/1 flattens a nested list",
            MEXEC2_LISTS,
            "mexec2_lists",
            "flatten_len",
            1,
            &[1],
            "4",
        ),
        case(
            "mexec_extra/lists_sublist_len",
            "the real lists:sublist/2 takes the first three elements",
            MEXEC2_LISTS,
            "mexec2_lists",
            "sublist_len",
            1,
            &[5],
            "3",
        ),
        case(
            "mexec_extra/lists_last",
            "the real lists:last/1 returns the final element",
            MEXEC2_LISTS,
            "mexec2_lists",
            "last",
            1,
            &[9],
            "9",
        ),
        case(
            "mexec_extra/lists_keyfind",
            "the real lists:keyfind/3 looks up a key in a tuple list",
            MEXEC2_LISTS,
            "mexec2_lists",
            "keyfind",
            1,
            &[2],
            "20",
        ),
        case(
            "mexec_extra/lists_mapfoldl_sum",
            "the real lists:mapfoldl/3 threads its accumulator",
            MEXEC2_LISTS,
            "mexec2_lists",
            "mapfoldl_sum",
            1,
            &[3],
            "6",
        ),
        case(
            "mexec_extra/lists_filter_len",
            "the real lists:filter/2 keeps the elements above the bound",
            MEXEC2_LISTS,
            "mexec2_lists",
            "filter_len",
            1,
            &[2],
            "2",
        ),
        case(
            "mexec_extra/lists_takewhile_len",
            "the real lists:takewhile/2 stops at the failing element",
            MEXEC2_LISTS,
            "mexec2_lists",
            "takewhile_len",
            1,
            &[3],
            "2",
        ),
        case(
            "mexec_extra/lists_usort_len",
            "the real lists:usort/1 sorts and deduplicates",
            MEXEC2_LISTS,
            "mexec2_lists",
            "usort_len",
            1,
            &[4],
            "4",
        ),
        // --- maps (always reduced to an integer) -----------------------------------
        case(
            "mexec_extra/maps_from_list_size",
            "the real maps:from_list/1 builds a three-key map",
            MEXEC2_MAPS,
            "mexec2_maps",
            "from_list_size",
            1,
            &[5],
            "3",
        ),
        case(
            "mexec_extra/maps_keys_head",
            "the sorted real maps:keys/1 starts at its smallest key",
            MEXEC2_MAPS,
            "mexec2_maps",
            "keys_head",
            1,
            &[1],
            "1",
        ),
        case(
            "mexec_extra/maps_get_default",
            "the real maps:get/3 falls back to its default for a missing key",
            MEXEC2_MAPS,
            "mexec2_maps",
            "get_default",
            1,
            &[9],
            "99",
        ),
        case(
            "mexec_extra/maps_merge_size",
            "the real maps:merge/2 unions two disjoint maps",
            MEXEC2_MAPS,
            "mexec2_maps",
            "merge_size",
            1,
            &[1],
            "2",
        ),
        case(
            "mexec_extra/maps_without_size",
            "the real maps:without/2 drops a key",
            MEXEC2_MAPS,
            "mexec2_maps",
            "without_size",
            1,
            &[1],
            "2",
        ),
        case(
            "mexec_extra/maps_fold_sum",
            "the real maps:fold/3 sums the values",
            MEXEC2_MAPS,
            "mexec2_maps",
            "fold_sum",
            1,
            &[4],
            "6",
        ),
        case(
            "mexec_extra/maps_find_value",
            "the real maps:find/2 returns {ok,V} for a present key",
            MEXEC2_MAPS,
            "mexec2_maps",
            "find_value",
            1,
            &[2],
            "20",
        ),
        case(
            "mexec_extra/maps_values_head",
            "the sorted real maps:values/1 starts at its smallest value",
            MEXEC2_MAPS,
            "mexec2_maps",
            "values_head",
            1,
            &[1],
            "1",
        ),
        case(
            "mexec_extra/maps_put_size",
            "the real maps:put/3 grows the map",
            MEXEC2_MAPS,
            "mexec2_maps",
            "put_size",
            1,
            &[7],
            "3",
        ),
        case(
            "mexec_extra/maps_update_value",
            "the real maps:update/3 replaces the value at a present key",
            MEXEC2_MAPS,
            "mexec2_maps",
            "update_value",
            1,
            &[5],
            "5",
        ),
        case(
            "mexec_extra/maps_map_sum",
            "the real maps:map/2 transforms every value",
            MEXEC2_MAPS,
            "mexec2_maps",
            "map_sum",
            1,
            &[1],
            "5",
        ),
        // --- binary / bitstring ----------------------------------------------------
        case(
            "mexec_extra/binary_byte_size",
            "byte_size/1 of a constructed binary agrees",
            MEXEC2_BINARY,
            "mexec2_binary",
            "size",
            1,
            &[4],
            "4",
        ),
        case(
            "mexec_extra/binary_at",
            "the real binary:at/2 reads a middle byte",
            MEXEC2_BINARY,
            "mexec2_binary",
            "at",
            1,
            &[3],
            "4",
        ),
        case(
            "mexec_extra/binary_part_size",
            "the real binary:part/3 slices a sub-binary",
            MEXEC2_BINARY,
            "mexec2_binary",
            "part_size",
            1,
            &[4],
            "4",
        ),
        case(
            "mexec_extra/binary_match_pos",
            "the real binary:match/2 locates a byte",
            MEXEC2_BINARY,
            "mexec2_binary",
            "match_pos",
            1,
            &[5],
            "4",
        ),
        case(
            "mexec_extra/binary_bit_size",
            "bit_size/1 of a 2-byte binary is 16",
            MEXEC2_BINARY,
            "mexec2_binary",
            "bitsize",
            1,
            &[7],
            "16",
        ),
        // --- string ----------------------------------------------------------------
        case(
            "mexec_extra/string_length",
            "the real string:length/1 counts a duplicated character list",
            MEXEC2_STRING,
            "mexec2_string",
            "str_len",
            1,
            &[4],
            "4",
        ),
        case(
            "mexec_extra/string_uppercase_head",
            "the real string:uppercase/1 upper-cases a character",
            MEXEC2_STRING,
            "mexec2_string",
            "upper_head",
            1,
            &[1],
            "66",
        ),
        case(
            "mexec_extra/string_concat_head",
            "the real string:concat/2 joins two strings",
            MEXEC2_STRING,
            "mexec2_string",
            "concat_head",
            1,
            &[1],
            "97",
        ),
        // --- unicode ---------------------------------------------------------------
        case(
            "mexec_extra/unicode_chars_to_binary_size",
            "the real unicode:characters_to_binary/1 encodes a multi-byte code point",
            MEXEC2_UNICODE,
            "mexec2_unicode",
            "bin_size",
            1,
            &[1],
            "3",
        ),
        // --- base64 ----------------------------------------------------------------
        case(
            "mexec_extra/base64_encode_size",
            "the real base64:encode/1 builds an off-heap binary",
            MEXEC2_BASE64,
            "mexec2_base64",
            "encode_size",
            1,
            &[5],
            "8",
        ),
        case(
            "mexec_extra/base64_decode_size",
            "the real base64:decode/1 round-trips an encoded binary",
            MEXEC2_BASE64,
            "mexec2_base64",
            "decode_size",
            1,
            &[5],
            "5",
        ),
        // --- proplists -------------------------------------------------------------
        case(
            "mexec_extra/proplists_get_value_hit",
            "the real proplists:get_value/3 finds a present key",
            MEXEC2_PROPLISTS,
            "mexec2_proplists",
            "get_hit",
            1,
            &[2],
            "20",
        ),
        case(
            "mexec_extra/proplists_delete_len",
            "the real proplists:delete/2 removes one entry",
            MEXEC2_PROPLISTS,
            "mexec2_proplists",
            "delete_len",
            1,
            &[1],
            "3",
        ),
        // --- orddict ---------------------------------------------------------------
        case(
            "mexec_extra/orddict_fetch",
            "the real orddict:fetch/2 looks a key up",
            MEXEC2_ORDDICT,
            "mexec2_orddict",
            "fetch",
            1,
            &[2],
            "20",
        ),
        // --- gb_trees --------------------------------------------------------------
        case(
            "mexec_extra/gb_trees_take_smallest",
            "the real gb_trees:take_smallest/1 returns the least value",
            MEXEC2_GB_TREES,
            "mexec2_gb_trees",
            "take_smallest_value",
            1,
            &[3],
            "1",
        ),
        // --- gb_sets ---------------------------------------------------------------
        case(
            "mexec_extra/gb_sets_union_size",
            "the real gb_sets:union/2 merges two balanced sets",
            MEXEC2_GB_SETS,
            "mexec2_gb_sets",
            "union_size",
            1,
            &[4],
            "3",
        ),
        case(
            "mexec_extra/gb_sets_largest",
            "the real gb_sets:largest/1 finds the greatest member",
            MEXEC2_GB_SETS,
            "mexec2_gb_sets",
            "largest",
            1,
            &[3],
            "9",
        ),
        // --- ordsets ---------------------------------------------------------------
        case(
            "mexec_extra/ordsets_intersection_size",
            "the real ordsets:intersection/2 keeps the common member",
            MEXEC2_ORDSETS,
            "mexec2_ordsets",
            "intersection_size",
            1,
            &[2],
            "1",
        ),
        case(
            "mexec_extra/ordsets_subtract_size",
            "the real ordsets:subtract/2 removes a member",
            MEXEC2_ORDSETS,
            "mexec2_ordsets",
            "subtract_size",
            1,
            &[2],
            "2",
        ),
        // --- sets ------------------------------------------------------------------
        case(
            "mexec_extra/sets_from_list_size",
            "the real sets:from_list/1 deduplicates",
            MEXEC2_SETS,
            "mexec2_sets",
            "from_list_size",
            1,
            &[4],
            "3",
        ),
        case(
            "mexec_extra/sets_union_size",
            "the real sets:union/2 merges two sets",
            MEXEC2_SETS,
            "mexec2_sets",
            "union_size",
            1,
            &[4],
            "3",
        ),
        // --- queue -----------------------------------------------------------------
        case(
            "mexec_extra/queue_len",
            "the real queue:from_list/1 + queue:len/1 execute",
            MEXEC2_QUEUE,
            "mexec2_queue",
            "len",
            1,
            &[4],
            "4",
        ),
        // --- array -----------------------------------------------------------------
        case(
            "mexec_extra/array_size",
            "the real array:from_list/1 + array:size/1 execute",
            MEXEC2_ARRAY,
            "mexec2_array",
            "size",
            1,
            &[4],
            "4",
        ),
        // --- calendar --------------------------------------------------------------
        case(
            "mexec_extra/calendar_is_leap_year",
            "the real calendar:is_leap_year/1 accepts a leap year",
            MEXEC2_CALENDAR,
            "mexec2_calendar",
            "is_leap",
            1,
            &[2024],
            "1",
        ),
        // --- math (bit-exact kernels only) -----------------------------------------
        case(
            "mexec_extra/math_sqrt_trunc",
            "the real math:sqrt/1 kernel agrees on 4",
            MEXEC2_MATH,
            "mexec2_math",
            "sqrt_trunc",
            1,
            &[4],
            "2",
        ),
        case(
            "mexec_extra/math_pow_trunc",
            "the real math:pow/2 kernel agrees on 2^4",
            MEXEC2_MATH,
            "mexec2_math",
            "pow_trunc",
            1,
            &[4],
            "16",
        ),
        case(
            "mexec_extra/math_log2_trunc",
            "the real math:log2/1 kernel agrees on 8",
            MEXEC2_MATH,
            "mexec2_math",
            "log2_trunc",
            1,
            &[8],
            "3",
        ),
        // --- filename --------------------------------------------------------------
        case(
            "mexec_extra/filename_basename_len",
            "the real filename:basename/1 splits an integer-bearing path",
            MEXEC2_FILENAME,
            "mexec2_filename",
            "basename_len",
            1,
            &[7],
            "9",
        ),
        case(
            "mexec_extra/filename_dirname_len",
            "the real filename:dirname/1 keeps the directory part",
            MEXEC2_FILENAME,
            "mexec2_filename",
            "dirname_len",
            1,
            &[1],
            "2",
        ),
        // --- io_lib_pretty ---------------------------------------------------------
        case(
            "mexec_extra/io_lib_pretty_print_len",
            "the real io_lib_pretty:print/1 renders a small list",
            MEXEC2_IO_LIB_PRETTY,
            "mexec2_io_lib_pretty",
            "print_len",
            1,
            &[1],
            "7",
        ),
        // --- digraph ---------------------------------------------------------------
        case(
            "mexec_extra/digraph_vertices_len",
            "the real digraph vertex set holds the added vertex",
            MEXEC2_DIGRAPH,
            "mexec2_digraph",
            "vertices_len",
            1,
            &[1],
            "1",
        ),
        case(
            "mexec_extra/digraph_out_degree",
            "the real digraph:out_degree/2 counts one outgoing edge",
            MEXEC2_DIGRAPH,
            "mexec2_digraph",
            "out_degree",
            1,
            &[1],
            "1",
        ),
    ]
}
