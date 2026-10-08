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

//! Contract **A.2 / A.7** — **real system modules execute** end to end.
//!
//! Where [`super::modules`] runs a compiled wrapper over the runtime's own BIF floor,
//! this group is the *execution* evidence for the **installed OTP standard library**:
//! each case is a tiny embedded wrapper module (compiled with the system `erlc`) whose
//! exported function calls a *real* `stdlib`/`kernel` module — `calendar`, `ordsets`,
//! `queue`, `gb_trees`, `gb_sets`, `proplists`, `orddict`, `array`, `string`,
//! `unicode`, `io_lib`, `filename`, `erl_scan`, `base64` — by name. So the Rust
//! runtime's `run` action must **load and execute the real system module** from the
//! code path (the wrapper's call is a cross-module call into the library), and the
//! oracle executes the very same module. Only stdout + exit status are compared.
//!
//! Every case is **verified against both runtimes** (`differential run <id>` →
//! `AGREE`), so the corpus never claims agreement it does not have. A wrapper function
//! takes **integer** arguments and returns an **integer** (the shape a module-run
//! golden pins), building whatever list/tuple the real call needs internally; the
//! module it exercises is the point. Each case is tagged [`Contract::BeamAsm`] (a
//! compiled `.beam` executes) and [`Contract::OtpApps`] (a real OTP application module
//! is loaded and run).
//!
//! The module `dict` is deliberately **absent** here: the runtime does not reproduce it
//! exactly — `dict:store/3` raises a `function_clause` where the oracle returns a value, a
//! plain Erlang reason, i.e. a `mismatch`, not a capability gap — so it must not be claimed as
//! agreement; it is reported, not added. (`sets` agrees; the `math` and `phash`/`phash2`
//! kernels have their own groups, [`super::math_bifs`] and [`super::phash`].)

use super::{Case, CaseKind, Contract, Golden, ModuleRun};

/// The `calendar` wrapper: integer arithmetic, `rem/2`, tuple access and boolean
/// guards inside the real stdlib module (`calendar:last_day_of_the_month/2`,
/// `calendar:day_of_the_week/3`, `calendar:date_to_gregorian_days/3`).
const MEXEC_CALENDAR: &str = "-module(mexec_calendar).\n\
     -export([last_day/2, day_of_week/3, greg_days/3]).\n\
     last_day(Y, M) -> calendar:last_day_of_the_month(Y, M).\n\
     day_of_week(Y, M, D) -> calendar:day_of_the_week(Y, M, D).\n\
     greg_days(Y, M, D) -> calendar:date_to_gregorian_days(Y, M, D).\n";

/// The `ordsets` wrapper — the real ordered-set module, which itself calls `lists`.
const MEXEC_ORDSETS: &str = "-module(mexec_ordsets).\n\
     -export([from_size/1, add_size/1, is_member/1, union_size/1]).\n\
     from_size(N) -> length(ordsets:from_list([1, 2, 2, N])).\n\
     add_size(N) -> length(ordsets:add_element(N, ordsets:from_list([1, 2, 3]))).\n\
     is_member(N) -> case ordsets:is_element(N, ordsets:from_list([1, 3, 5])) of\n\
     \x20                  true -> 1;\n\
     \x20                  false -> 0\n\
     \x20              end.\n\
     union_size(N) -> length(ordsets:union(ordsets:from_list([1, 2]), ordsets:from_list([2, N]))).\n";

/// The `queue` wrapper — the real functional FIFO (`queue:from_list/1`, `in/2`,
/// `out/1`, `len/1`).
const MEXEC_QUEUE: &str = "-module(mexec_queue).\n\
     -export([from_size/1, in_size/1, out_size/1, head/1]).\n\
     from_size(N) -> queue:len(queue:from_list([N, N + 1, N + 2])).\n\
     in_size(N) -> queue:len(queue:in(N, queue:from_list([1, 2, 3]))).\n\
     out_size(N) -> queue:len(element(2, queue:out(queue:from_list([N, 9, 8])))).\n\
     head(N) -> element(2, element(1, queue:out(queue:from_list([N, 9, 8])))).\n";

/// The `gb_trees` wrapper — the real balanced-tree module (a `foldl` build, `get/2`,
/// `is_defined/2`, `smallest/1`).
const MEXEC_GB_TREES: &str = "-module(mexec_gb_trees).\n\
     -export([size/1, get/1, is_defined/1, smallest/1]).\n\
     size(N) -> gb_trees:size(lists:foldl(fun(X, T) -> gb_trees:insert(X, X * 10, T) end,\n\
     \x20                                        gb_trees:empty(), lists:seq(1, N))).\n\
     get(N) -> gb_trees:get(N, gb_trees:insert(N, N * 10, gb_trees:empty())).\n\
     is_defined(N) -> case gb_trees:is_defined(N, gb_trees:insert(1, 1, gb_trees:empty())) of\n\
     \x20                   true -> 1;\n\
     \x20                   false -> 0\n\
     \x20               end.\n\
     smallest(N) -> element(1, gb_trees:smallest(gb_trees:insert(N, 1, gb_trees:insert(5, 2, gb_trees:empty())))).\n";

/// The `gb_sets` wrapper — the real balanced-tree set (a different module from
/// `gb_trees`; `from_list/1`, `add_element/2`, `is_element/2`, `smallest/1`).
const MEXEC_GB_SETS: &str = "-module(mexec_gb_sets).\n\
     -export([size/1, add_size/1, is_member/1, smallest/1]).\n\
     size(N) -> gb_sets:size(gb_sets:from_list([1, 2, 2, N])).\n\
     add_size(N) -> gb_sets:size(gb_sets:add_element(N, gb_sets:from_list([1, 2, 3]))).\n\
     is_member(N) -> case gb_sets:is_element(N, gb_sets:from_list([1, 3, 5])) of\n\
     \x20                  true -> 1;\n\
     \x20                  false -> 0\n\
     \x20              end.\n\
     smallest(N) -> gb_sets:smallest(gb_sets:from_list([N, 9, 8])).\n";

/// The `proplists` wrapper — the real property-list module (`get_value/3`, its
/// default form, and `is_defined/2`).
const MEXEC_PROPLISTS: &str = "-module(mexec_proplists).\n\
     -export([get_hit/2, get_default/2, is_defined/1]).\n\
     get_hit(K, D) -> proplists:get_value(K, [{1, 10}, {2, 20}, {3, 30}], D).\n\
     get_default(N, D) -> proplists:get_value(N, [{1, 10}, {2, 20}], D).\n\
     is_defined(N) -> case proplists:is_defined(N, [{1, x}, {2, y}]) of\n\
     \x20                    true -> 1;\n\
     \x20                    false -> 0\n\
     \x20                end.\n";

/// The `orddict` wrapper — the real ordered dictionary (`fetch/2`, `size/1`,
/// `is_key/1`, `store/3`).
const MEXEC_ORDDICT: &str = "-module(mexec_orddict).\n\
     -export([fetch/2, size/1, is_key/1, store_size/2]).\n\
     fetch(K, D) -> orddict:fetch(K, orddict:from_list([{1, 10}, {2, 20}])) + (D - D).\n\
     size(N) -> orddict:size(orddict:from_list([{1, a}, {2, b}, {3, c}])) + (N - N).\n\
     is_key(N) -> case orddict:is_key(N, orddict:from_list([{1, a}])) of\n\
     \x20                 true -> 1;\n\
     \x20                 false -> 0\n\
     \x20             end.\n\
     store_size(K, V) -> orddict:size(orddict:store(K, V, orddict:from_list([{1, a}]))).\n";

/// The `array` wrapper — the real tuple-backed array (`from_list/1`, `size/1`,
/// `get/2`).
const MEXEC_ARRAY: &str = "-module(mexec_array).\n\
     -export([size/1, get_first/1, get_elem/2]).\n\
     size(N) -> array:size(array:from_list(lists:seq(1, N))).\n\
     get_first(N) -> array:get(1, array:from_list([N, 20, 30])).\n\
     get_elem(I, D) -> array:get(I, array:from_list(lists:duplicate(5, D))).\n";

/// The `string` wrapper — the real string module (a `unicode` consumer):
/// `length/1`, `uppercase/1`, `trim/1`, `reverse/1`.
const MEXEC_STRING: &str = "-module(mexec_string).\n\
     -export([str_length/1, upper_first/1, trim_length/1, reverse_first/1]).\n\
     str_length(N) -> string:length(lists:duplicate(N, $a)).\n\
     upper_first(N) -> hd(string:uppercase([$a + (N rem 26)])).\n\
     trim_length(N) -> string:length(string:trim(lists:duplicate(N, $\\s) ++ \"x\")).\n\
     reverse_first(N) -> hd(string:reverse(integer_to_list(N))).\n";

/// The `unicode` wrapper — the real Unicode module (`characters_to_list/1`,
/// `characters_to_binary/1`).
const MEXEC_UNICODE: &str = "-module(mexec_unicode).\n\
     -export([to_list_length/1, to_binary_size/1, roundtrip_size/1]).\n\
     to_list_length(N) -> length(unicode:characters_to_list(integer_to_list(N))).\n\
     to_binary_size(N) -> byte_size(unicode:characters_to_binary(integer_to_list(N))).\n\
     roundtrip_size(N) -> byte_size(unicode:characters_to_binary(unicode:characters_to_list(integer_to_list(N)))).\n";

/// The `io_lib` wrapper — the real formatting module (`format/2`).
const MEXEC_IO_LIB: &str = "-module(mexec_io_lib).\n\
     -export([format_length/1, format_first/1, format_size/1]).\n\
     format_length(N) -> length(lists:flatten(io_lib:format(\"~w\", [N]))).\n\
     format_first(N) -> hd(io_lib:format(\"~p\", [N])).\n\
     format_size(N) -> iolist_size(io_lib:format(\"~w\", [N])).\n";

/// The `filename` wrapper — the real path module (`basename/1`, `extension/1`,
/// `join/1`).
const MEXEC_FILENAME: &str = "-module(mexec_filename).\n\
     -export([base_first/1, ext_length/1, join_length/1]).\n\
     base_first(N) -> hd(filename:basename(integer_to_list(N) ++ \".txt\")).\n\
     ext_length(N) -> length(filename:extension(\"file\" ++ integer_to_list(N) ++ \".ext\")).\n\
     join_length(N) -> length(filename:join([\"/tmp\", integer_to_list(N)])).\n";

/// The `erl_scan` wrapper — the real Erlang tokenizer (`string/1`).
const MEXEC_ERL_SCAN: &str = "-module(mexec_erl_scan).\n\
     -export([token_count/1]).\n\
     token_count(N) -> case erl_scan:string(integer_to_list(N)) of\n\
     \x20                     {ok, Tokens, _} -> length(Tokens);\n\
     \x20                     {error, _, _} -> 0\n\
     \x20                 end.\n";

/// The `base64` wrapper — the real base64 codec, which builds an off-heap binary
/// (`encode/1`).
const MEXEC_BASE64: &str = "-module(mexec_base64).\n\
     -export([encode_size/1, encode_byte_size/1]).\n\
     encode_size(N) -> byte_size(base64:encode(list_to_binary(lists:seq(1, N)))).\n\
     encode_byte_size(N) -> byte_size(base64:encode(<<N>>)).\n";

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

/// The A.2/A.7 real-system-module execution cases. Every one is verified `AGREE`.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- calendar ---------------------------------------------------------------
        case(
            "mexec/calendar_last_day_of_month",
            "the real calendar:last_day_of_the_month/2 executes on a leap February",
            MEXEC_CALENDAR,
            "mexec_calendar",
            "last_day",
            2,
            &[2024, 2],
            "29",
        ),
        case(
            "mexec/calendar_day_of_the_week",
            "the real calendar:day_of_the_week/3 executes on 2000-01-01",
            MEXEC_CALENDAR,
            "mexec_calendar",
            "day_of_week",
            3,
            &[2000, 1, 1],
            "6",
        ),
        case(
            "mexec/calendar_date_to_gregorian_days",
            "the real calendar:date_to_gregorian_days/3 executes on 2000-01-01",
            MEXEC_CALENDAR,
            "mexec_calendar",
            "greg_days",
            3,
            &[2000, 1, 1],
            "730485",
        ),
        // --- ordsets (calls into lists) --------------------------------------------
        case(
            "mexec/ordsets_from_list_size",
            "the real ordsets:from_list/1 deduplicates (a cross-module lists call)",
            MEXEC_ORDSETS,
            "mexec_ordsets",
            "from_size",
            1,
            &[5],
            "3",
        ),
        case(
            "mexec/ordsets_add_element_size",
            "the real ordsets:add_element/2 grows the set",
            MEXEC_ORDSETS,
            "mexec_ordsets",
            "add_size",
            1,
            &[4],
            "4",
        ),
        case(
            "mexec/ordsets_is_element",
            "the real ordsets:is_element/2 finds a member",
            MEXEC_ORDSETS,
            "mexec_ordsets",
            "is_member",
            1,
            &[3],
            "1",
        ),
        // --- queue -----------------------------------------------------------------
        case(
            "mexec/queue_from_list_len",
            "the real queue:from_list/1 + queue:len/1 execute",
            MEXEC_QUEUE,
            "mexec_queue",
            "from_size",
            1,
            &[5],
            "3",
        ),
        case(
            "mexec/queue_in_len",
            "the real queue:in/2 grows the queue by one",
            MEXEC_QUEUE,
            "mexec_queue",
            "in_size",
            1,
            &[4],
            "4",
        ),
        case(
            "mexec/queue_out_len",
            "the real queue:out/1 shrinks the queue by one",
            MEXEC_QUEUE,
            "mexec_queue",
            "out_size",
            1,
            &[7],
            "2",
        ),
        // --- gb_trees --------------------------------------------------------------
        case(
            "mexec/gb_trees_size",
            "the real gb_trees structural build reports its size",
            MEXEC_GB_TREES,
            "mexec_gb_trees",
            "size",
            1,
            &[5],
            "5",
        ),
        case(
            "mexec/gb_trees_get",
            "the real gb_trees:get/2 looks a key up",
            MEXEC_GB_TREES,
            "mexec_gb_trees",
            "get",
            1,
            &[3],
            "30",
        ),
        case(
            "mexec/gb_trees_is_defined",
            "the real gb_trees:is_defined/2 finds a present key",
            MEXEC_GB_TREES,
            "mexec_gb_trees",
            "is_defined",
            1,
            &[1],
            "1",
        ),
        // --- gb_sets ---------------------------------------------------------------
        case(
            "mexec/gb_sets_size",
            "the real gb_sets:from_list/1 deduplicates",
            MEXEC_GB_SETS,
            "mexec_gb_sets",
            "size",
            1,
            &[4],
            "3",
        ),
        case(
            "mexec/gb_sets_add_element_size",
            "the real gb_sets:add_element/2 grows the set",
            MEXEC_GB_SETS,
            "mexec_gb_sets",
            "add_size",
            1,
            &[4],
            "4",
        ),
        // --- proplists -------------------------------------------------------------
        case(
            "mexec/proplists_get_value",
            "the real proplists:get_value/3 finds a present key",
            MEXEC_PROPLISTS,
            "mexec_proplists",
            "get_hit",
            2,
            &[2, 0],
            "20",
        ),
        case(
            "mexec/proplists_get_value_default",
            "the real proplists:get_value/3 falls back to its default",
            MEXEC_PROPLISTS,
            "mexec_proplists",
            "get_default",
            2,
            &[9, 77],
            "77",
        ),
        // --- orddict ---------------------------------------------------------------
        case(
            "mexec/orddict_fetch",
            "the real orddict:fetch/2 looks a key up",
            MEXEC_ORDDICT,
            "mexec_orddict",
            "fetch",
            2,
            &[2, 0],
            "20",
        ),
        case(
            "mexec/orddict_size",
            "the real orddict:from_list/1 + orddict:size/1 execute",
            MEXEC_ORDDICT,
            "mexec_orddict",
            "size",
            1,
            &[9],
            "3",
        ),
        // --- array -----------------------------------------------------------------
        case(
            "mexec/array_size",
            "the real array:from_list/1 + array:size/1 execute",
            MEXEC_ARRAY,
            "mexec_array",
            "size",
            1,
            &[4],
            "4",
        ),
        case(
            "mexec/array_get",
            "the real array:get/2 reads the first element",
            MEXEC_ARRAY,
            "mexec_array",
            "get_first",
            1,
            &[11],
            "20",
        ),
        // --- string (a unicode consumer) -------------------------------------------
        case(
            "mexec/string_length",
            "the real string:length/1 counts a duplicated character list",
            MEXEC_STRING,
            "mexec_string",
            "str_length",
            1,
            &[4],
            "4",
        ),
        case(
            "mexec/string_reverse_first",
            "the real string:reverse/1 reverses an integer string",
            MEXEC_STRING,
            "mexec_string",
            "reverse_first",
            1,
            &[1234],
            "52",
        ),
        // --- unicode ---------------------------------------------------------------
        case(
            "mexec/unicode_characters_to_list_length",
            "the real unicode:characters_to_list/1 round-trips an integer string",
            MEXEC_UNICODE,
            "mexec_unicode",
            "to_list_length",
            1,
            &[12345],
            "5",
        ),
        case(
            "mexec/unicode_characters_to_binary_size",
            "the real unicode:characters_to_binary/1 encodes an integer string",
            MEXEC_UNICODE,
            "mexec_unicode",
            "to_binary_size",
            1,
            &[42],
            "2",
        ),
        // --- io_lib ----------------------------------------------------------------
        case(
            "mexec/io_lib_format_length",
            "the real io_lib:format/2 renders an integer",
            MEXEC_IO_LIB,
            "mexec_io_lib",
            "format_length",
            1,
            &[12345],
            "5",
        ),
        case(
            "mexec/io_lib_format_size",
            "iolist_size/1 of a real io_lib:format/2 result agrees",
            MEXEC_IO_LIB,
            "mexec_io_lib",
            "format_size",
            1,
            &[12345],
            "5",
        ),
        // --- filename --------------------------------------------------------------
        case(
            "mexec/filename_basename_first",
            "the real filename:basename/1 splits a path (a cross-module call)",
            MEXEC_FILENAME,
            "mexec_filename",
            "base_first",
            1,
            &[42],
            "52",
        ),
        case(
            "mexec/filename_extension_length",
            "the real filename:extension/1 sizes an extension",
            MEXEC_FILENAME,
            "mexec_filename",
            "ext_length",
            1,
            &[9],
            "4",
        ),
        // --- erl_scan --------------------------------------------------------------
        case(
            "mexec/erl_scan_token_count",
            "the real erl_scan:string/1 tokenizes an integer literal",
            MEXEC_ERL_SCAN,
            "mexec_erl_scan",
            "token_count",
            1,
            &[42],
            "1",
        ),
        // --- base64 ----------------------------------------------------------------
        case(
            "mexec/base64_encode_size",
            "the real base64:encode/1 builds an off-heap binary",
            MEXEC_BASE64,
            "mexec_base64",
            "encode_size",
            1,
            &[5],
            "8",
        ),
    ]
}
