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

//! Contract **A.7** — BIF / OTP application surface.
//!
//! The BIFs and `lists` functions every program depends on. They are exercised through
//! the *value* probe (`~w`), which is a rendering a runtime must reproduce exactly.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.7 cases: core BIFs and a `lists` function.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "bifs/length",
            description: "length/1 counts the elements of an improper-free list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "length([a,b,c])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/integer_to_binary",
            description: "integer_to_binary/1 renders a base-10 byte string",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "integer_to_binary(255)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<50,53,53>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_reverse",
            description: "lists:reverse/1 from the stdlib reverses a list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:reverse([1,2,3])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [3,2,1]"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_seq",
            description: "lists:seq/2 builds the inclusive integer interval",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:seq(1,5)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3,4,5]"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_sort",
            description: "lists:sort/1 orders a list of integers",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:sort([3,1,2])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_sum",
            description: "lists:sum/1 adds the elements of a list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:sum([1,2,3,4])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 10"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/maps_get",
            description: "maps:get/2 reads a key from a map",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "maps:get(a, #{a => 1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/map_size",
            description: "map_size/1 counts the keys of a map",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "map_size(#{a => 1, b => 2})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 2"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/tuple_size",
            description: "tuple_size/1 reports the arity of a tuple",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "tuple_size({1,2,3})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/byte_size",
            description: "byte_size/1 reports the size of a binary in bytes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "byte_size(<<1,2,3>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/binary_encode_unsigned",
            description: "binary:encode_unsigned/1 is the inverse of decode_unsigned/1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "binary:encode_unsigned(258)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<1,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/binary_decode_unsigned",
            description: "binary:decode_unsigned/1 reads a big-endian unsigned integer",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "binary:decode_unsigned(<<1,2>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 258"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/atom_to_binary",
            description: "atom_to_binary/1 renders the atom as a UTF-8 binary",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "atom_to_binary(hello)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/list_to_binary",
            description: "list_to_binary/1 packs a byte list into a binary",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "list_to_binary([104,105])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<104,105>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/integer_to_list",
            description: "integer_to_list/1 renders a base-10 digit list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "integer_to_list(123)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [49,50,51]"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/hd_tl",
            description: "hd/1 and tl/1 decompose a list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{hd([9,8]), tl([9,8])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {9,[8]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_member_true",
            description: "lists:member/2 is true for a present element",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:member(2, [1,2,3])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_nth",
            description: "lists:nth/2 indexes a list one-based",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:nth(2, [10,20,30])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 20"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/maps_is_key",
            description: "maps:is_key/2 is true for a present key",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "maps:is_key(a, #{a => 1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/binary_part",
            description: "binary:part/3 extracts a sub-binary",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "binary:part(<<1,2,3,4>>, 1, 2)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<2,3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/setelement",
            description: "setelement/3 replaces one tuple element and returns the tuple",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "setelement(2, {1,2,3}, 9)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {1,9,3}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/type_test_bifs",
            description: "the is_*/1 type-test BIFs agree on their own literals",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{is_atom(ok), is_integer(1), is_list([1]), is_binary(<<1>>), is_tuple({1}), is_map(#{})}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {true,true,true,true,true,true}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/list_to_integer",
            description: "list_to_integer/1 parses a base-10 digit list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "list_to_integer(\"1234\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1234"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/named_fun_apply",
            description: "a named fun (`fun erlang:abs/1`) is a first-class value that applies",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin F = fun erlang:abs/1, F(-7) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 7"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/io_lib_format",
            description: "io_lib:format/2 returns the formatted text as a deep list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "io_lib:format(\"~w\", [42])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [[52,50]]"),
                exit_code: 0,
            },
        },
        Case {
            id: "lists/usort_2_degenerate",
            description: "lists:usort/2 reproduces the oracle for degenerate (non-total) comparators",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `usort/2` is a faithful port of the pure-Erlang `lists.erl` (`usort_1` /
                // `ufsplit_*` / `ufmergel`), so the *ordering of equal elements* is
                // observable. The five comparators below are the `lists.erl` `Fun(A, B) -> true
                // if A =< B` shapes: an always-true and an always-false comparator, a reversed
                // arithmetic comparison (`A > B` keeps equal elements, `A >= B` removes them),
                // the expected ascending total order (`A =< B`), and one that is inconsistent
                // for equal elements (`A < B`). Every list is the small duplicate-bearing input
                // [3,1,2,1,2].
                expr: "{lists:usort(fun(_,_) -> true end, [3,1,2,1,2]), \
                       lists:usort(fun(_,_) -> false end, [1,2,3]), \
                       lists:usort(fun(A,B) -> A > B end, [3,1,2,1,2]), \
                       lists:usort(fun(A,B) -> A >= B end, [3,1,2,1,2]), \
                       lists:usort(fun(A,B) -> A =< B end, [3,1,2,1,2]), \
                       lists:usort(fun(A,B) -> A < B end, [3,1,2,1,2])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[3],[3,2,1],[3,2,2,1,1],[3,2,1],[1,2,3],[1,1,2,2,3]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "lists/usort_1_dedup",
            description: "lists:usort/1 removes duplicates and collapses arithmetic-equal elements",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `usort/1` keeps the *first* of each arithmetically-equal run, so
                // `[1.0, 1, 2]` collapses to `[1.0, 2]` while `[1, 1.0, 2]` collapses to
                // `[1, 2]` — the surviving representative is observable. The last list is the
                // `lists.erl` doc example (mixed types, repeated elements).
                expr: "{lists:usort([1.0, 1, 2]), lists:usort([1, 1.0, 2]), \
                       lists:usort([3,2,a,3,2,a,1,3,b,2,2,1])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[1.0,2],[1,2],[1,2,3,a,b]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "lists/fun_taking_eval",
            description: "the fun-taking lists BIFs run through the -eval `FunCaller` seam",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `lists:map/2`, `lists:foldl/3` and `lists:sort/2` take a fun a BIF cannot
                // invoke itself, so they exercise the runtime's `FunCaller` seam end to end on
                // the `-eval` path (a literal fun is applied by the evaluator).
                expr: "{lists:map(fun(X) -> X*2 end, [1,2,3]), \
                       lists:foldl(fun(X, Acc) -> X+Acc end, 0, [1,2,3,4]), \
                       lists:sort(fun(A,B) -> A =< B end, [3,1,2,1,3])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[2,4,6],10,[1,1,2,3,3]}"),
                exit_code: 0,
            },
        },
        // ---------------------------------------------------------------------------------
        // Newly-registered BIF families: the timer family, process control, code loading,
        // and the `erts_internal` / `erts_debug` introspection modules. Every expression
        // below was probed under BOTH runtimes before pinning, and every golden is the
        // oracle's exact (single-line) answer.
        //
        // A Rust side that exits `3` with `beam_rt: unsupported: ...` is an honest
        // capability gap (`unsupported`), never a mismatch: `bifs/start_timer_returns_ref`
        // and `bifs/function_exported_lists_member` are pinned that way (the timer family
        // and `function_exported/3` are not registered in `beam_rt` yet). They flip to
        // `agree` automatically once the BIFs land, because the golden is the oracle's.
        //
        // Deliberately NOT pinned: `erlang:module_loaded(lists)` -- the oracle answers
        // `true` while `beam_rt` answers `false` (a plain differing value, i.e. a genuine
        // mismatch), and `code:module_loaded/1` does not exist in the oracle at all
        // (`error:undef`). And no volatile rendering is asserted: the timer `Ref`s are
        // observed only through `is_reference/1`, and `registered()`/`processes()` only
        // through their shape (`is_list/1`, `lists:member/2`), never the per-VM atom/pid
        // list (`boot/processes_is_list` / `boot/processes_contains_self` already cover
        // the process-list shape).
        // ---------------------------------------------------------------------------------
        Case {
            id: "bifs/start_timer_returns_ref",
            description: "erlang:start_timer/3 answers a reference (identity is per-VM)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The `Ref` itself is per-VM-start, so only its *type* is observed.
                expr: "is_reference(erlang:start_timer(1000,self(),ok))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/send_after_returns_ref",
            description: "erlang:send_after/3 answers a reference (identity is per-VM)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "is_reference(erlang:send_after(1000,self(),ok))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/cancel_timer_of_fresh_ref_is_false",
            description: "erlang:cancel_timer/1 is false for a timer that never existed",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:cancel_timer(make_ref())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok false"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/is_process_alive_self",
            description: "erlang:is_process_alive/1 is true for the calling process",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:is_process_alive(self())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/whereis_init_is_pid",
            description: "erlang:whereis/1 finds the `init` process as a pid",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "is_pid(whereis(init))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/registered_is_list",
            description: "erlang:registered/0 answers a list (contents are per-VM)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Only the shape is asserted: the atom list is per-VM and must never be
                // pinned by exact value.
                expr: "is_list(erlang:registered())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/registered_contains_init",
            description: "erlang:registered/0 contains the boot `init` process name",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:member(init, erlang:registered())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/processes_has_at_least_one",
            description: "erlang:processes/0 lists at least the calling process",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The pid list is per-VM (and `boot/processes_is_list` /
                // `boot/processes_contains_self` already pin its shape), so assert only
                // that it is non-empty.
                expr: "length(erlang:processes()) >= 1",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/function_exported_lists_member",
            description: "erlang:function_exported/3 sees the loaded lists:member/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:function_exported(lists,member,2)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/module_loaded_lists_shape",
            description: "code:is_loaded/1 answers the `{file, Path}` shape for lists",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The path is machine-specific, so only the `file` tag is asserted (the
                // `flatten`/`preloaded`/`false` shapes are the other possibilities).
                expr: "element(1, code:is_loaded(lists))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok file"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/erts_debug_size_of_tuple",
            description: "erts_debug:size/1 counts the words of a small tuple",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A 2-tuple is a header word plus two payload words: 3 words.
                expr: "erts_debug:size({a,1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/erts_internal_cmp_term",
            description: "erts_internal:cmp_term/2 returns the term-order integer",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // 1 < 2 in term order, so the comparison integer is -1.
                expr: "erts_internal:cmp_term(1,2)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok -1"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/base64_encode_empty_binary",
            description: "base64:encode/1 of a zero-length binary is the zero-length binary <<>>",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A zero-length *result* must be returned, not refused. The input is built at
                // run time (`binary:copy/2` of one byte repeated 0 times) so it is not folded
                // into a literal, and `base64:encode/1` still hands back an off-heap binary.
                expr: "base64:encode(binary:copy(<<$A>>, 0))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<>>"),
                exit_code: 0,
            },
        },
        // --- `erlang:send/3`: the option list belongs to the BIF, the delivery to the host --
        // `send_3` (`bif.c::send_3`) parses its own option list — a proper list of the atoms
        // `noconnect`/`nosuspend`/`priority`, order- and duplicate-insensitive — *before* the
        // destination is validated, then delegates the delivery. Its *successful* answer is the
        // atom `ok`, never the message (`send/2` returns the message), and an unknown option is
        // `badarg`. Every golden below is the live oracle's output.
        Case {
            id: "bifs/send_3_noconnect_answers_ok",
            description: "erlang:send/3 with the noconnect option answers the atom ok for a local destination",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:send(self(), hello, [noconnect])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok ok"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/send_3_options_are_a_set",
            description: "erlang:send/3 accepts a multi-element option list (the options are a set)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:send(self(), hello, [nosuspend, noconnect])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok ok"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/send_3_unknown_option_is_badarg",
            description: "erlang:send/3 rejects an option that is not one of noconnect/nosuspend/priority with badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "erlang:send(self(), hello, [bogus])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "bifs/send_3_delivers_the_message_to_self",
            description: "a noconnect send to the calling process really enqueues the message",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The `after 0` clause keeps the case self-terminating: a runtime that dropped
                // the message answers `timeout` instead of `delivered`.
                expr: "begin erlang:send(self(), hello, [noconnect]), \
                       receive hello -> delivered after 0 -> timeout end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok delivered"),
                exit_code: 0,
            },
        },
        // --- `beam_lib:md5/1`: the module digest of a real `.beam` image ---------------------
        Case {
            id: "bifs/beam_lib_md5_of_a_real_module_is_a_sixteen_byte_binary",
            description: "beam_lib:md5/1 hashes a loaded module's .beam image into a 16-byte digest",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `code:which/1` locates the installed `lists.beam` and `beam_lib:md5/1` hashes
                // it. The digest *bytes* are installation-dependent, so only their shape is
                // pinned (an MD5 is always a 16-byte binary); the `{ok,{Module,Digest}}` wrapper
                // itself is asserted by the pattern match, which both runtimes must satisfy.
                expr: "begin {ok,{lists,Digest}} = beam_lib:md5(code:which(lists)), \
                       {is_binary(Digest), byte_size(Digest)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {true,16}"),
                exit_code: 0,
            },
        },
    ]
}
