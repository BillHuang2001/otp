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

/// The A.7 cases: core BIFs plus the `lists` and `maps` stdlib surface (the `bifs/`, `lists/` and
/// `maps/` id prefixes).
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
        // The whole block now **agrees**: the timer family and `function_exported/3` are
        // registered in `beam_rt`, so `bifs/start_timer_returns_ref` and
        // `bifs/function_exported_lists_member` are ordinary agreeing cases (a Rust side
        // that exits `3` with `beam_rt: unsupported: ...` would instead be an honest
        // capability gap, never a mismatch).
        //
        // Deliberately NOT pinned: `erlang:module_loaded(lists)` -- the oracle answers
        // `true` while `beam_rt` answers `false` (a plain differing value, i.e. a genuine
        // mismatch, recorded in `divergences.md`), and `code:module_loaded/1` does not exist
        // in either runtime (`error:undef`). And no volatile rendering is asserted: the timer
        // `Ref`s are observed only through `is_reference/1`, and `registered()`/`processes()`
        // only through their shape (`is_list/1`, `lists:member/2`), never the per-VM atom/pid
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
            description: "erlang:function_exported/3 reports the declared `bif.tab` builtin lists:member/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `lists:member/2` IS declared in `erts/emulator/beam/bif.tab`, so the builtin
                // membership rule answers `true` regardless of whether the module is loaded —
                // this case is load-state independent (unlike `lists:sort/1`, which is only
                // exported once the module is loaded).
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
            description: "code:is_loaded/1 answers the `{file, Path}` shape for lists once it is loaded",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The path is machine-specific, so only the `file` tag is asserted (the
                // `flatten`/`preloaded`/`false` shapes are the other possibilities).
                // `code:ensure_loaded/1` makes the load state explicit: the Rust runtime's
                // `code:is_loaded/1` is path-based, so without it the case would agree only
                // because the oracle happens to have `lists` loaded.
                expr: "begin code:ensure_loaded(lists), element(1, code:is_loaded(lists)) end",
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
        // ---------------------------------------------------------------------------------
        // The widened `lists`/`string`/`unicode`/`maps`/`timer` surface and the `erlang`
        // conversion BIFs. Every expression was probed under BOTH runtimes before pinning
        // (`differential run <id>` ⇒ `[AGREE]`), and every golden is the oracle's exact
        // (single-line) answer. No `~w` of a multi-key map is pinned: every map case goes
        // through `lists:sort(maps:to_list(M))`, `maps:keys/1` of a one-key map, or a
        // single-key result, so no map-internal key order ever reaches a golden.
        // ---------------------------------------------------------------------------------
        Case {
            id: "bifs/lists_append_flatten_zip_unzip",
            description: "lists:append/2, flatten/1, zip/2 and unzip/1 on documented inputs",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:append([1,2],[3]), lists:flatten([1,[2,[3]]]), \
                       lists:zip([1,2],[a,b]), lists:unzip([{1,a},{2,b}])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[1,2,3],[1,2,3],[{1,a},{2,b}],{[1,2],[a,b]}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_split_last_duplicate_sublist",
            description: "lists:split/2, last/1, duplicate/2 and the 3-arity sublist/3",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:split(2, [1,2,3,4]), lists:last([1,2,3]), \
                       lists:duplicate(3, x), lists:sublist([1,2,3,4,5], 2, 2)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{[1,2],[3,4]},3,[x,x,x],[2,3]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_keyfind_keymember_keydelete",
            description: "the lists key* family over a tuple list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:keyfind(b, 1, [{a,1},{b,2}]), lists:keymember(b, 1, [{a,1},{b,2}]), \
                       lists:keydelete(b, 1, [{a,1},{b,2}])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{b,2},true,[{a,1}]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_min_max_sum",
            description: "lists:min/1, max/1 and sum/1 (including the empty list)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:min([3,1,2]), lists:max([3,1,2]), lists:sum([1,2,3,4]), lists:sum([])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {1,3,10,0}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_partition_foldr_mapfoldl",
            description: "lists:partition/2, foldr/3 and mapfoldl/3 through the FunCaller seam",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:partition(fun(X) -> X rem 2 =:= 0 end, [1,2,3,4]), \
                       lists:foldr(fun(X,A) -> [X|A] end, [], [1,2,3]), \
                       lists:mapfoldl(fun(X,A) -> {X*2, A+X} end, 0, [1,2,3])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{[2,4],[1,3]},[1,2,3],{[2,4,6],6}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_takewhile_droplast_search_nthtail",
            description: "lists:takewhile/2, dropwhile/2, droplast/1, search/2 and nthtail/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:takewhile(fun(X) -> X < 3 end, [1,2,3,1]), \
                       lists:dropwhile(fun(X) -> X < 3 end, [1,2,3,1]), lists:droplast([1,2,3]), \
                       lists:search(fun(X) -> X > 1 end, [1,2,3]), lists:nthtail(2, [1,2,3,4])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[1,2],[3,1],[1,2],{value,2},[3,4]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_prefix_suffix_merge_filtermap",
            description: "lists:prefix/2, suffix/2, merge/2 and filtermap/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:prefix([1,2],[1,2,3]), lists:suffix([2,3],[1,2,3]), \
                       lists:suffix([], [1]), lists:merge([1,3],[2,4]), \
                       lists:filtermap(fun(X) -> case X rem 2 of 0 -> {true,X}; 1 -> false end end, \
                                       [1,2,3,4])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {true,true,true,[1,2,3,4],[2,4]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_join_seq3_zip3_unzip3",
            description: "lists:join/2, the 3-arity seq/3, zip3/3, unzip3/1 and reverse/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:join($,, [\"a\",\"b\"]), lists:seq(1, 10, 3), \
                       lists:zip3([1,2],[a,b],[x,y]), lists:unzip3([{1,a,x},{2,b,y}]), \
                       lists:reverse([1,2,3], [4])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {[[97],44,[98]],[1,4,7,10],[{1,a,x},{2,b,y}],{[1,2],[a,b],[x,y]},[3,2,1,4]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_concat",
            description:
                "lists:concat/1 flattens a mixed list of strings/atoms/integers to a byte list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "lists:concat([\"a\",\"b\",1])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,49]"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/string_case_trim_split_join",
            description: "string:uppercase/1, lowercase/1, trim/1, split/2 and join/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{string:uppercase(\"abc\"), string:lowercase(\"ABC\"), string:trim(\"  x  \"), \
                       string:split(\"a,b,c\", \",\"), string:join([\"a\",\"b\"], \"-\")}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[65,66,67],[97,98,99],[120],[[97],[98,44,99]],[97,45,98]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/string_find_replace_slice",
            description: "string:find/2, replace/3, replace/4 with `all`, slice/3 and reverse/1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{string:find(\"hello world\", \"world\"), \
                       string:replace(\"hello world\", \"world\", \"there\"), \
                       string:replace(\"a-b-c\", \"-\", \"+\", all), string:slice(\"hello\", 1, 3), \
                       string:reverse(\"abc\")}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {[119,111,114,108,100],[[104,101,108,108,111,32],[116,104,101,114,101],[]],\
                     [[97],[43],[98],[43],[99]],[101,108,108],[99,98,97]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/string_pad_tokens_graphemes",
            description: "string:pad/2,4, tokens/2, lexemes/2, to_graphemes/1 and length/1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{string:pad(\"7\", 3), string:pad(\"7\", 3, leading, $0), \
                       string:tokens(\"a b\", \" \"), string:lexemes(\"a,,b\", \",\"), \
                       string:to_graphemes(\"ab\"), string:length(\"h\u{e9}llo\")}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[[55],32,32],[[48,48],[55]],[[97],[98]],[[97],[98]],[97,98],5}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/string_concat_two",
            description: "string:concat/2 concatenates two byte strings",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "string:concat(\"a\",\"b\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98]"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/unicode_characters_round_trip",
            description: "unicode:characters_to_binary/1, characters_to_list/1,2 and the bad-UTF-8 refusal",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{unicode:characters_to_binary(\"abc\"), unicode:characters_to_list(<<195,169>>), \
                       unicode:characters_to_list([97,98], utf8), \
                       unicode:characters_to_binary(<<255>>, utf8)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<97,98,99>>,[233],[97,98],{error,<<>>,<<255>>}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/maps_with_without_filter_map_fold",
            description: "maps:with/2, without/2, filter/2, map/2 and fold/3, each rendered sorted",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Every map result goes through `lists:sort(maps:to_list(M))`: a multi-key
                // map's own `~w` order is not a portable observable.
                expr: "{lists:sort(maps:to_list(maps:with([a,b], #{a=>1,b=>2,c=>3}))), \
                       lists:sort(maps:to_list(maps:without([c], #{a=>1,b=>2,c=>3}))), \
                       lists:sort(maps:to_list(maps:filter(fun(_,N) -> N > 1 end, #{a=>1,b=>2}))), \
                       lists:sort(maps:to_list(maps:map(fun(_,N) -> N*2 end, #{a=>1,b=>2}))), \
                       lists:sort(maps:fold(fun(K,N,A) -> [{K,N}|A] end, [], #{a=>1,b=>2}))}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{a,1},{b,2}],[{a,1},{b,2}],[{b,2}],[{a,2},{b,4}],[{a,1},{b,2}]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/maps_take_find_get_put_update_merge",
            description: "maps:take/2, find/2, get/3, put/3, update/3, merge/2, keys/1 and size/1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{maps:take(a, #{a=>1,b=>2}), maps:find(a, #{a=>1}), maps:get(c, #{a=>1}, default), \
                       maps:put(a, 9, #{a=>1}), maps:update(a, 9, #{a=>1}), \
                       lists:sort(maps:to_list(maps:merge(#{a=>1}, #{b=>2}))), maps:keys(#{a=>1}), \
                       maps:size(#{})}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {{1,#{b => 2}},{ok,1},default,#{a => 9},#{a => 9},[{a,1},{b,2}],[a],0}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/timer_arithmetic_and_sleep",
            description: "the pure timer conversions plus timer:sleep/1 returning ok",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{timer:sleep(1), timer:seconds(1), timer:minutes(2), timer:hours(1)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,1000,120000,3600000}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/timer_now_diff_of_two_timestamps",
            description: "timer:now_diff/2 is the microsecond difference of two timestamp tuples",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "timer:now_diff({0,0,0},{1,2,3})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok -1000002000003"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/timer_tc_returns_the_fun_value",
            description: "timer:tc/1 returns {Time, Value}; element 2 is the fun's value",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "element(2, timer:tc(fun () -> 42 end))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/timer_apply_after_first_element_is_ok",
            description: "timer:apply_after/4 returns {ok, {once, Ref}} (element 1 is the atom ok)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "element(1, timer:apply_after(50, erlang, self, []))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok ok"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/integer_radix_conversions",
            description: "integer_to_binary/2 and binary_to_integer/1,2 across radices, incl. a badarg and a bignum",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{integer_to_binary(255, 16), integer_to_binary(-255, 36), \
                       binary_to_integer(<<\"FF\">>, 16), binary_to_integer(<<\"255\">>), \
                       erlang:binary_to_integer(erlang:integer_to_binary(100000000000000000000000)), \
                       begin try binary_to_integer(<<\"12a\">>) catch error:Reason -> Reason end end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<70,70>>,<<45,55,51>>,255,255,100000000000000000000000,badarg}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/atom_and_binary_name_conversions",
            description: "binary_to_atom/2 with utf8, atom_to_binary/2 round trip, latin1 and the list forms",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The `é` atom is rendered back through `atom_to_binary/2` so the golden stays
                // pure ASCII (the printer never renders the raw character).
                expr: "{binary_to_atom(<<\"hello\">>, utf8), \
                       atom_to_binary(binary_to_atom(<<195,169>>, utf8), utf8), \
                       atom_to_binary(ok, latin1), list_to_atom(\"hello\"), atom_to_list(hello)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {hello,<<195,169>>,<<111,107>>,hello,[104,101,108,108,111]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/round_trunc_floor_ceil_abs",
            description: "round/1, trunc/1, floor/1, ceil/1 and abs/1 on the documented half-way cases",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{trunc(3.7), trunc(-3.7), round(3.5), round(2.5), round(-0.5), floor(1.7), \
                       ceil(1.2), floor(-1.7), ceil(-1.2), abs(-3), abs(-3.5)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {3,-3,4,3,-1,1,2,-2,-1,3,3.5}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/tuple_construction_bifs",
            description: "the erlang tuple construction/deconstruction BIFs",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:make_tuple(3, x), erlang:tuple_to_list({1,2}), erlang:list_to_tuple([1,2]), \
                       erlang:setelement(2,{1,2,3},9), erlang:element(2,{1,2,3}), \
                       erlang:append_element({1,2}, 3), erlang:insert_element(2, {1,3}, 2), \
                       erlang:delete_element(2, {1,2,3})}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{x,x,x},[1,2],{1,2},{1,9,3},2,{1,2,3},{1,2,3},{1,3}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/iolist_and_binary_part_bifs",
            description: "iolist_size/1, iolist_to_binary/1 and both binary_part/2,3 argument forms",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:iolist_size([1,2,[3,4]]), erlang:iolist_to_binary([1,2]), \
                       erlang:binary_part(<<1,2,3,4>>, 1, 2), erlang:binary_part(<<1,2,3,4>>, {1,2})}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {4,<<1,2>>,<<2,3>>,<<2,3>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/process_dictionary_put_get_erase",
            description: "erlang:put/get/erase on a private key (the process dictionary is per-process)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The key is private to the probe process and the expression is self-contained,
                // so only the put/get/erase *sequence* is observable (never the whole
                // dictionary, which the `-eval` process inherits from `init`).
                expr: "begin erlang:put(differential_probe_key, 1), \
                       Res = {erlang:get(differential_probe_key), erlang:erase(differential_probe_key), \
                              erlang:get(differential_probe_key)}, Res end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {1,1,undefined}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/external_size_of_a_term",
            description: "erlang:external_size/1 counts the term_to_binary bytes of a term",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A small integer and an atom: 3 and 7 bytes respectively (the documented
                // `external_size/1` encoding sizes).
                expr: "{erlang:external_size(42), erlang:external_size(blah)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {3,7}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/is_builtin_distinguishes_a_bif_from_a_library_function",
            description: "erlang:is_builtin/3 is true for a bif.tab BIF and false for a stdlib function",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:is_builtin(erlang, length, 1), erlang:is_builtin(lists, reverse, 1)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {true,false}"),
                exit_code: 0,
            },
        },
        // ---------------------------------------------------------------------------------
        // The second widened block: the `binary:`/`string:`/`lists:`/`maps:`/`float`
        // conversion surface that the first block does not reach, plus the two `erlang`
        // integer-conversion and time BIFs. Every expression was probed under BOTH runtimes
        // before pinning (`differential run <id>` ⇒ `[AGREE]`) and every golden is the
        // oracle's own single line. No `~w` of a multi-key map is pinned: a map-bearing case
        // renders through `lists:sort(maps:to_list(M))` or reduces to `maps:size/1`.
        // ---------------------------------------------------------------------------------
        Case {
            id: "bifs/integer_to_list_radix_and_binary_digits",
            description: "integer_to_list/1,2 across radices (incl. a negative) and integer_to_binary/2 in base 2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{integer_to_list(255, 16), integer_to_list(-1), integer_to_binary(255, 2), \
                       binary_to_integer(<<\"FF\">>, 16)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[70,70],[45,49],<<49,49,49,49,49,49,49,49>>,255}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/binary_split_match_search",
            description: "binary:split/2,3 (with `global`), match/2, matches/2 and longest_common_prefix/suffix/1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:split(<<\"a,b,c\">>, <<\",\">>), \
                       binary:split(<<\"a,b,c\">>, <<\",\">>, [global]), \
                       binary:match(<<\"hello\">>, <<\"ll\">>), \
                       binary:matches(<<\"hello\">>, <<\"l\">>), \
                       binary:longest_common_prefix([<<\"ab\">>, <<\"ac\">>]), \
                       binary:longest_common_suffix([<<\"ab\">>, <<\"cb\">>])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],{2,2},[{2,1},{3,1}],1,1}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/binary_part_first_last_at_copy",
            description: "binary:part/2,3, first/1, last/1, at/2 and copy/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:part(<<\"hello\">>, 1, 3), binary:part(<<\"hello\">>, {1,3}), \
                       binary:first(<<\"ab\">>), binary:last(<<\"ab\">>), binary:at(<<\"ab\">>, 1), \
                       binary:copy(<<\"ab\">>, 3)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<101,108,108>>,<<101,108,108>>,97,98,98,<<97,98,97,98,97,98>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/binary_replace_encode_decode_unsigned",
            description: "binary:replace/3,4, encode_unsigned/1,2 (big/little), decode_unsigned/1,2 and bin_to_list/list_to_bin",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:replace(<<\"hello\">>, <<\"l\">>, <<\"L\">>), \
                       binary:replace(<<\"hello\">>, <<\"l\">>, <<\"L\">>, [global]), \
                       binary:encode_unsigned(300), binary:encode_unsigned(300, little), \
                       binary:decode_unsigned(<<1,44>>), binary:decode_unsigned(<<44,1>>, little), \
                       binary:bin_to_list(<<\"ab\">>), binary:list_to_bin([1,2])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {<<104,101,76,108,111>>,<<104,101,76,76,111>>,<<1,44>>,<<44,1>>,300,300,[97,98],<<1,2>>}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/string_to_number_substr_and_padding",
            description: "string:to_integer/1, to_float/1, substr/3, str/2, left/2, right/3, centre/3, words/2 and sub_word/3",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{string:to_integer(\"42ab\"), string:to_float(\"1.5ab\"), \
                       string:substr(\"hello\", 2, 3), string:str(\"hello\", \"ll\"), \
                       string:left(\"ab\", 4), string:right(\"ab\", 4, $0), \
                       string:centre(\"ab\", 6, $-), string:words(\"a b c\", 32), \
                       string:sub_word(\"a b c\", 2, 32)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {{42,[97,98]},{1.5,[97,98]},[101,108,108],3,[97,98,32,32],\
                     [48,48,97,98],[45,45,97,98,45,45],3,[98]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/string_next_grapheme_split_all_is_empty_equal",
            description: "string:next_grapheme/1, split/3 with `all`, is_empty/1 and equal/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{string:next_grapheme(\"abc\"), string:split(\"a-b-c\", \"-\", all), \
                       string:is_empty(\"\"), string:equal(\"a\", \"a\")}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[97,98,99],[[97],[98],[99]],true,true}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_usort_splitwith_mapfoldr_sort_fun",
            description: "lists:sort/2 through a fun, usort/1, splitwith/2 and mapfoldr/3",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:sort(fun(A, B) -> A >= B end, [1,3,2]), lists:usort([3,1,3,2]), \
                       lists:usort([a,b,a]), lists:splitwith(fun(X) -> X < 3 end, [1,2,3]), \
                       lists:mapfoldr(fun(X, A) -> {X, A} end, 0, [1,2])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[3,2,1],[1,2,3],[a,b],{[1,2],[3]},{[1,2],0}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_all_any_flatmap_flatlength",
            description: "lists:all/2, any/2 (incl. the empty list), flatmap/2 and flatlength/1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:all(fun(X) -> X > 0 end, [1,2]), \
                       lists:any(fun(X) -> X > 2 end, [1,2]), \
                       lists:any(fun(_) -> true end, []), \
                       lists:flatmap(fun(X) -> [X,X] end, [1,2]), \
                       lists:flatlength([[1],[2,3]])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {true,false,false,[1,1,2,2],3}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_zipwith_enumerate",
            description: "lists:zipwith/3, zipwith3/4 and the 1- and 2-arity enumerate/1,2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:zipwith(fun(A, B) -> A + B end, [1,2], [3,4]), \
                       lists:zipwith3(fun(A, B, K) -> A + B + K end, [1], [2], [3]), \
                       lists:enumerate([a,b]), lists:enumerate(5, [a,b])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[4,6],[6],[{1,a},{2,b}],[{5,a},{6,b}]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_keyreplace_keystore_keytake_keymap",
            description: "lists:keyreplace/4, keystore/4, keytake/3 and keymap/3 over a tuple list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:keyreplace(b, 1, [{a,1},{b,2}], {b,9}), \
                       lists:keystore(b, 1, [{a,1},{b,2}], {b,9}), \
                       lists:keytake(b, 1, [{a,1},{b,2}]), \
                       lists:keymap(fun(X) -> X*2 end, 2, [{a,1}]), lists:keyfind(1, 2, [{a,1}])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{a,1},{b,9}],[{a,1},{b,9}],{value,{b,2},[{a,1}]},[{a,2}],{a,1}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/lists_umerge_and_usort_of_lists",
            description: "lists:umerge/1 merges sorted sublists and usort/1 orders a list of lists",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{lists:umerge([[1],[2],[1]]), lists:usort([[2],[1]])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[1,2],[[1],[2]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/maps_update_with_remove_merge_from_list",
            description: "maps:update_with/2,3, remove/2, merge/2 and from_list/1, each rendered sorted",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Every multi-key map result goes through `lists:sort(maps:to_list(M))`; the
                // `from_list/1` result is only measured with `maps:size/1`.
                expr: "{lists:sort(maps:to_list(maps:update_with(a, fun(X) -> X+1 end, #{a=>1}))), \
                       lists:sort(maps:to_list(maps:update_with(z, fun(X) -> X+1 end, 0, #{}))), \
                       lists:sort(maps:to_list(maps:remove(a, #{a=>1,b=>2}))), \
                       lists:sort(maps:to_list(maps:merge(#{a=>1}, #{b=>2}))), \
                       maps:size(maps:from_list([{a,1},{b,2}]))}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{a,2}],[{z,0}],[{b,2}],[{a,1},{b,2}],2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/float_to_binary_and_back",
            description: "float_to_binary/1,2 (decimals/short), binary_to_float/1, list_to_float/1 and float_to_list/1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `float_to_binary(1.5)` and `float_to_list(1.5)` are the same 26-byte decimal
                // image (the default `[{scientific,20}]` rendering), pinned byte for byte.
                expr: "{float_to_binary(1.5), float_to_binary(1.5, [{decimals,2}]), \
                       float_to_binary(0.1, [short]), binary_to_float(<<\"1.5\">>), \
                       binary_to_float(<<\"1.5E2\">>), list_to_float(\"1.5\"), float_to_list(1.5)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {<<49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,101,43,48,48>>,\
                     <<49,46,53,48>>,<<48,46,49>>,1.5,150.0,1.5,\
                     [49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,101,43,48,48]}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/iolist_bitstring_and_size_bifs",
            description: "iolist_size/1, iolist_to_binary/1, list_to_bitstring/1, bit_size/1, byte_size/1 and the bitstring type tests",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{iolist_size([1,[2,3]]), iolist_to_binary([<<\"a\">>,[<<\"b\">>]]), \
                       list_to_bitstring([1,2,<<3:4>>]), bit_size(<<1:3>>), byte_size(<<1:3>>), \
                       is_bitstring(<<1:1>>), is_binary(<<1:1>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {3,<<97,98>>,<<1,2,3:4>>,3,1,true,false}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/apply_and_binary_list_conversions",
            description: "erlang:apply/2,3 on a module and a fun, split_binary/2, binary_to_list/1 and list_to_binary/1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{erlang:apply(lists, reverse, [[1,2]]), erlang:apply(fun(X) -> X+1 end, [1]), \
                       erlang:split_binary(<<1,2,3>>, 2), erlang:binary_to_list(<<1,2>>), \
                       list_to_binary([1,2])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[2,1],2,{<<1,2>>,<<3>>},[1,2],<<1,2>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/io_lib_format_integer_char_string_float",
            description: "io_lib:format/2 with the ~b/~c/~s/~p integer/char/string/term controls and the ~.2f/~e/~10.3f float controls",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{io_lib:format(\"~b ~c ~s\", [10, 97, \"ab\"]), \
                       iolist_to_binary(io_lib:format(\"~p\", [{1,2}])), \
                       io_lib:format(\"~.2f\", [1.5]), iolist_to_binary(io_lib:format(\"~e\", [100.0])), \
                       iolist_to_binary(io_lib:format(\"~10.3f\", [3.14159]))}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {[49,48,32,97,32,[97,98]],<<123,49,44,50,125>>,[49,46,53,48],\
                     <<49,46,48,48,48,48,48,101,43,50>>,<<32,32,32,32,32,51,46,49,52,50>>}",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "bifs/monotonic_and_unique_integer_are_integers",
            description: "erlang:unique_integer/0,1, system_time/0 and monotonic_time/0 are integers (shape, not value)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A per-VM-start value, so only the shape is pinned: a `[positive]` unique integer
                // is > 0 and every clock reading is an integer.
                expr: "{erlang:unique_integer([positive]) > 0, is_integer(erlang:unique_integer()), \
                       is_integer(erlang:system_time()), is_integer(erlang:monotonic_time())}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {true,true,true,true}"),
                exit_code: 0,
            },
        },
        // ---------------------------------------------------------------------------------
        // The `maps/` prefix: the `maps:iterator/1,2` + `maps:next/1` family and the
        // fun-taking `maps` fast paths (`map/2`, `fold/3`, `filter/2`, `foreach/2`,
        // `update_with/3,4`, `merge_with/3`, `intersect_with/3`, `filtermap/2`).
        //
        // Iterator semantics pinned here (probed against the oracle before pinning):
        //   * `maps:iterator/1` is a **list** `[Order|Map]` (`is_list/1` is `true`), *not* an
        //     `{ok, Iterator}` pair, so the iterator term is passed straight to `maps:next/1`;
        //   * `maps:iterator(M, ordered)` walks the keys in term order (numbers before atoms);
        //     `maps:iterator(M, undefined)` (== `maps:iterator/1`) visits the same key set in
        //     an *undefined* order, so an unordered walk is only observed through `lists:sort/1`;
        //   * `maps:next/1` answers a three-tuple `{Key, Value, NextIterator}` and `none` at the
        //     end of the iterator (an empty map's iterator answers `none` immediately).
        //
        // Golden discipline: the iterator term itself (`[Order|Map]`) and any multi-key map are
        // runtime-internal renderings, so every case below projects them — `maps:next/1` is
        // destructured down to `{Key, Value}`, every multi-key map goes through
        // `lists:sort(maps:to_list(M))` / `maps:size(M)`, and a full walk is compared with
        // `=:=` against `lists:seq/2` (a single `true`/`false`).
        // ---------------------------------------------------------------------------------
        Case {
            id: "maps/iterator_next_of_a_single_key_map",
            description: "maps:next/1 of a maps:iterator/1 answers the {Key, Value, NextIterator} triple",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A single-key map keeps the rendering stable; only `{K, Val}` is observed, never
                // the iterator term itself.
                expr: "begin {K,Val,_}=maps:next(maps:iterator(#{a=>1})), {K,Val} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {a,1}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/iterator_ordered_next_is_the_smallest_key",
            description: "maps:iterator/2 with `ordered` starts at the smallest key in term order",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin {K,Val,_}=maps:next(maps:iterator(#{c=>3,a=>1,b=>2}, ordered)), {K,Val} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {a,1}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/iterator_walks_every_key_in_term_order",
            description: "an `ordered` iterator walks every key/pair in term order (numbers before atoms)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The complete walk is itself an observable, so the accumulated list is compared
                // as-is: `ordered` is documented to visit the keys in term order, and the second
                // walk's mixed-type keys (integer 1 before the atoms a/b) pin that ordering.
                expr: "begin Walk=fun W(It,Acc) -> case maps:next(It) of \
                       none -> lists:reverse(Acc); {K,Val,N} -> W(N,[{K,Val}|Acc]) end end, \
                       {Walk(maps:iterator(#{c=>3,a=>1,b=>2}, ordered), []), \
                       Walk(maps:iterator(#{b=>2, 1=>x, a=>3}, ordered), [])} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{a,1},{b,2},{c,3}],[{1,x},{a,3},{b,2}]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/iterator_undefined_order_visits_the_same_key_set",
            description: "maps:iterator/1 (undefined order) visits the same key set as the ordered walk",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The *visit order* of an unordered iterator is not a portable observable, so the
                // walk is sorted first — the pinned fact is the key set (and the pairs).
                expr: "begin Walk=fun W(It,Acc) -> case maps:next(It) of \
                       none -> lists:reverse(Acc); {K,Val,N} -> W(N,[{K,Val}|Acc]) end end, \
                       lists:sort(Walk(maps:iterator(#{c=>3,a=>1,b=>2}), [])) end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,1},{b,2},{c,3}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/iterator_exhausted_and_empty_maps_answer_none",
            description: "maps:next/1 answers `none` past the end of an iterator and for an empty map",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin {_,_,I1}=maps:next(maps:iterator(#{a=>1})), \
                       {maps:next(I1), maps:next(maps:iterator(#{})), \
                       maps:next(maps:iterator(#{}, ordered))} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {none,none,none}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/next_of_none_is_none",
            description: "maps:next/1 accepts the documented terminal `none` and answers `none`",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The documented iterator type is `iterator() | none`, so `none` is a legal input
                // (it ends the walk) while any other non-iterator term is a `badarg`.
                expr: "maps:next(none)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok none"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/iterator_ordered_walk_of_a_hamt_is_term_ordered",
            description: "an `ordered` walk of a 40-key (HAMT-backed) map is exactly the term-ordered keys",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // 40 keys crosses the flatmap/`HAMT` boundary, so this exercises the iterator over
                // the non-flatmap representation. The walked key list is compared with `=:=` to
                // `lists:seq(1,40)` (one `true`), plus its length/ends, to keep the golden short.
                expr: "begin M=maps:from_list([{I,I} || I <- lists:seq(1,40)]), \
                       Walk=fun W(It,Acc) -> case maps:next(It) of \
                       none -> lists:reverse(Acc); {K,_,N} -> W(N,[K|Acc]) end end, \
                       Ks=Walk(maps:iterator(M, ordered), []), \
                       {length(Ks), hd(Ks), lists:last(Ks), Ks =:= lists:seq(1,40)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {40,1,40,true}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/iterator_and_next_error_shapes",
            description: "maps:iterator/2 and maps:next/1 refuse a bad order, a non-map and a non-iterator",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Every refusal is caught inside the expression and reduced to its reason, so the
                // case pins the fault classes: `badarg` for an unknown order atom and for a
                // `maps:next/1` argument that is not an iterator, and `{badmap,Term}` for a
                // `maps:iterator/1` argument that is not a map.
                expr: "{try maps:iterator(#{a=>1}, bogus) of _ -> no_raise catch error:Reason1 -> {error,Reason1} end, \
                       try maps:iterator([{a,1}]) of _ -> no_raise catch error:Reason2 -> {error,Reason2} end, \
                       try maps:next(foo) of _ -> no_raise catch error:Reason3 -> {error,Reason3} end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{error,badarg},{error,{badmap,[{a,1}]}},{error,badarg}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/map_fold_filter_foreach_result_shape",
            description: "maps:map/2, maps:fold/3, maps:filter/2 and maps:foreach/2 apply a fun",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin {maps:foreach(fun(_,_) -> ok end, #{a=>1}), \
                       lists:sort(maps:to_list(maps:map(fun(_,N) -> N*2 end, #{a=>1,b=>2}))), \
                       lists:sort(maps:fold(fun(K,N,A) -> [{K,N}|A] end, [], #{a=>1,b=>2})), \
                       lists:sort(maps:to_list(maps:filter(fun(K,_) -> K =:= a end, #{a=>1,b=>2})))} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[{a,2},{b,4}],[{a,1},{b,2}],[{a,1}]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/iteration_over_an_empty_map",
            description: "the fun-taking maps fast paths on an empty map (foreach/2 answers `ok`)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `maps:foreach/2` returns `ok` (not a map), so its result is observed directly;
                // the map-valued results are measured with `maps:size/1`.
                expr: "begin {maps:size(maps:map(fun(_,N) -> N*2 end, #{})), \
                       maps:fold(fun(_,_,A) -> A+1 end, 0, #{}), \
                       maps:foreach(fun(_,_) -> ok end, #{}), \
                       maps:size(maps:filter(fun(_,_) -> true end, #{}))} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {0,0,ok,0}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/keys_values_to_list_and_from_list",
            description: "maps:from_list/1 (last binding wins), maps:keys/1, values/1, to_list/1 and size/1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin M=maps:from_list([{a,1},{b,2},{a,9}]), \
                       {maps:size(M), maps:get(a,M), lists:sort(maps:to_list(M)), \
                       lists:sort(maps:keys(M)), lists:sort(maps:values(M))} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {2,9,[{a,9},{b,2}],[a,b],[2,9]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/get_2_and_3_on_present_and_absent_keys",
            description: "maps:get/2 and maps:get/3 with the `{badkey,Key}` error of a missing key",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{maps:get(a, #{a=>1}), maps:get(z, #{a=>1}, default), \
                       try maps:get(z, #{a=>1}) of _ -> no_raise \
                       catch error:Reason -> {error,Reason} end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {1,default,{error,{badkey,z}}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/update_with_arity_3_and_4",
            description: "maps:update_with/3,4 update-or-insert, and /3 faults on an absent key",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{maps:get(a, maps:update_with(a, fun(X) -> X+1 end, #{a=>1})), \
                       maps:get(z, maps:update_with(z, fun(X) -> X+1 end, 0, #{})), \
                       try maps:update_with(z, fun(X) -> X end, #{a=>1}) of _ -> no_raise \
                       catch error:Reason -> {error,Reason} end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {2,0,{error,{badkey,z}}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/with_without_take_intersect",
            description: "maps:with/2, maps:without/2, maps:take/2 (hit and miss) and maps:intersect/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `maps:take/2` on a hit is a one-key map (a stable rendering); on a miss it is the
                // bare atom `error`.
                expr: "begin {lists:sort(maps:to_list(maps:with([a,c], #{a=>1,b=>2}))), \
                       lists:sort(maps:to_list(maps:without([b], #{a=>1,b=>2}))), \
                       maps:take(a, #{a=>1,b=>2}), maps:take(z, #{a=>1}), \
                       lists:sort(maps:to_list(maps:intersect(#{a=>1,b=>2}, #{b=>3,c=>4})))} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{a,1}],[{a,1}],{1,#{b => 2}},error,[{b,3}]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/merge_merge_with_intersect_with",
            description: "maps:merge/2, maps:merge_with/3 and maps:intersect_with/3 with a combining fun",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin {lists:sort(maps:to_list(maps:merge(#{a=>1,b=>2}, #{b=>9}))), \
                       lists:sort(maps:to_list(maps:merge_with(fun(_,X,Y) -> X+Y end, #{a=>1,b=>2}, #{b=>3}))), \
                       lists:sort(maps:to_list(maps:intersect_with(fun(_,X,Y) -> X+Y end, \
                       #{a=>1,b=>2}, #{b=>3,c=>4})))} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{a,1},{b,9}],[{a,1},{b,5}],[{b,5}]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/groups_from_list_and_filtermap",
            description: "maps:groups_from_list/2,3 (grouping keeps insertion order) and maps:filtermap/2",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `groups_from_list/3` is `(KeyFun, ValueFun, List)`; the value lists inside each
                // group are in input order (`lists:sort/1` sorts the outer `{Key, List}` pairs only).
                expr: "begin {lists:sort(maps:to_list(maps:groups_from_list(fun(X) -> X rem 2 end, [1,2,3,4]))), \
                       lists:sort(maps:to_list(maps:groups_from_list(fun(X) -> X rem 2 end, \
                       fun(X) -> X*10 end, [1,2,3]))), \
                       lists:sort(maps:to_list(maps:filtermap(fun(_K,N) -> \
                       if N > 1 -> {true,N*2}; true -> false end end, #{a=>1,b=>2}))), \
                       maps:size(maps:filtermap(fun(_,_) -> false end, #{a=>1,b=>2}))} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{0,[2,4]},{1,[1,3]}],[{0,[20]},{1,[10,30]}],[{b,4}],0}"),
                exit_code: 0,
            },
        },
        Case {
            id: "maps/fun_taking_fast_paths_on_a_hamt_map",
            description: "maps:map/2, fold/3, filter/2 and foreach/2 over a 40-key (HAMT) map",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A 40-key map is HAMT-backed, so these exercise the fun-taking fast paths away
                // from the flatmap representation; the sums/sizes are order-independent.
                expr: "begin M=maps:from_list([{I,I} || I <- lists:seq(1,40)]), \
                       {lists:sum(maps:values(maps:map(fun(_,N) -> N*2 end, M))), \
                       maps:fold(fun(_,N,A) -> N+A end, 0, M), \
                       maps:size(maps:filter(fun(_,N) -> N > 10 end, M)), \
                       maps:foreach(fun(_,_) -> ok end, M)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {1640,820,30,ok}"),
                exit_code: 0,
            },
        },
    ]
}
#[cfg(test)]
mod tests {
    //! Invariants of the A.7 BIF group: every case is a `value` eval case with a
    //! one-line expression and an oracle golden, and no case ever renders a multi-key map.
    use super::group;
    use crate::corpus::{CaseKind, Contract, Probe};

    #[test]
    fn every_case_is_an_otp_apps_value_probe() {
        for case in group() {
            assert_eq!(case.contracts, &[Contract::OtpApps], "case {}", case.id);
            assert!(!case.description.is_empty(), "case {}", case.id);
            let CaseKind::Eval {
                expr,
                extra_argv,
                probe,
            } = case.kind
            else {
                panic!("case {}: an A.7 BIF case must be an eval case", case.id);
            };
            assert_eq!(probe, Probe::Value, "case {}", case.id);
            assert!(extra_argv.is_empty(), "case {}", case.id);
            assert!(!expr.is_empty() && !expr.contains('\n'), "case {}", case.id);
            assert!(
                case.golden.stdout.is_some_and(|s| !s.is_empty()),
                "case {}: empty golden",
                case.id
            );
            // The probe wrapper binds V/C/R: an expression that binds one of them would make
            // the *oracle* fail to match its own try-clause (a guaranteed false mismatch).
            assert!(
                !binds_probe_names(expr),
                "case {}: the expression binds a probe-wrapper name: {expr}",
                case.id
            );
        }
    }

    /// `true` when `expr` binds the bare name `V`, `C` or `R` (the probe wrapper's variables).
    /// String literals are removed first, so a `~p`/`~s` format string mentioning a reserved name
    /// is never mistaken for a bound variable.
    fn binds_probe_names(expr: &str) -> bool {
        identifiers(expr)
            .iter()
            .any(|token| matches!(*token, "V" | "C" | "R"))
    }

    /// The identifiers of `expr` with string literals removed.
    fn identifiers(expr: &str) -> Vec<&str> {
        let bytes = expr.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        let mut in_string = false;
        while i < bytes.len() {
            match bytes[i] {
                b'"' => {
                    in_string = !in_string;
                    i += 1;
                }
                b'\\' if in_string => i += 2,
                b if !in_string && (b.is_ascii_alphanumeric() || b == b'_' || b == b'@') => {
                    let start = i;
                    while i < bytes.len()
                        && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'@')
                    {
                        i += 1;
                    }
                    out.push(&expr[start..i]);
                }
                _ => i += 1,
            }
        }
        out
    }

    /// The widened families: the `lists`/`string`/`unicode`/`maps`/`timer` functions and the
    /// `erlang` conversion BIFs. Each golden is the oracle's own single-line answer.
    #[test]
    fn the_widened_families_pin_the_oracle_values() {
        let golden = |id: &str| {
            group()
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no case {id}"))
                .golden
                .stdout
                .expect("a golden")
        };
        for (id, stdout) in [
            (
                "bifs/lists_append_flatten_zip_unzip",
                "ok {[1,2,3],[1,2,3],[{1,a},{2,b}],{[1,2],[a,b]}}",
            ),
            ("bifs/lists_min_max_sum", "ok {1,3,10,0}"),
            (
                "bifs/lists_prefix_suffix_merge_filtermap",
                "ok {true,true,true,[1,2,3,4],[2,4]}",
            ),
            (
                "bifs/string_case_trim_split_join",
                "ok {[65,66,67],[97,98,99],[120],[[97],[98,44,99]],[97,45,98]}",
            ),
            (
                "bifs/unicode_characters_round_trip",
                "ok {<<97,98,99>>,[233],[97,98],{error,<<>>,<<255>>}}",
            ),
            (
                "bifs/maps_take_find_get_put_update_merge",
                "ok {{1,#{b => 2}},{ok,1},default,#{a => 9},#{a => 9},[{a,1},{b,2}],[a],0}",
            ),
            ("bifs/timer_arithmetic_and_sleep", "ok {ok,1000,120000,3600000}"),
            ("bifs/timer_now_diff_of_two_timestamps", "ok -1000002000003"),
            ("bifs/timer_tc_returns_the_fun_value", "ok 42"),
            ("bifs/timer_apply_after_first_element_is_ok", "ok ok"),
            ("bifs/lists_concat", "ok [97,98,49]"),
            ("bifs/string_concat_two", "ok [97,98]"),
            (
                "bifs/integer_radix_conversions",
                "ok {<<70,70>>,<<45,55,51>>,255,255,100000000000000000000000,badarg}",
            ),
            (
                "bifs/round_trunc_floor_ceil_abs",
                "ok {3,-3,4,3,-1,1,2,-2,-1,3,3.5}",
            ),
            ("bifs/external_size_of_a_term", "ok {3,7}"),
            (
                "bifs/is_builtin_distinguishes_a_bif_from_a_library_function",
                "ok {true,false}",
            ),
            ("bifs/process_dictionary_put_get_erase", "ok {1,1,undefined}"),
        ] {
            assert_eq!(golden(id), stdout, "case {id}");
        }
        // Every map-bearing case renders through `lists:sort(maps:to_list(M))` (or a one-key
        // map), so no map-internal key order can ever reach a golden.
        for id in [
            "bifs/maps_with_without_filter_map_fold",
            "bifs/maps_take_find_get_put_update_merge",
        ] {
            let expr = group()
                .into_iter()
                .find(|c| c.id == id)
                .expect("registered")
                .expr()
                .expect("eval case");
            assert!(
                expr.contains("lists:sort(maps:to_list("),
                "case {id}: a multi-key map must be rendered sorted, got {expr}"
            );
        }
    }

    /// The second widened block: the `binary:`/`string:`/`lists:`/`maps:`/`float` conversion
    /// surface and the `erlang` integer/time BIFs. Each golden is the oracle's own single line
    /// (re-measured with `differential run <id>`).
    #[test]
    fn the_second_widened_block_pins_the_oracle_lines() {
        let golden = |id: &str| {
            group()
                .into_iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("no case {id}"))
                .golden
                .stdout
                .expect("a golden")
        };
        for (id, stdout) in [
            (
                "bifs/integer_to_list_radix_and_binary_digits",
                "ok {[70,70],[45,49],<<49,49,49,49,49,49,49,49>>,255}",
            ),
            (
                "bifs/binary_split_match_search",
                "ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],{2,2},[{2,1},{3,1}],1,1}",
            ),
            (
                "bifs/binary_part_first_last_at_copy",
                "ok {<<101,108,108>>,<<101,108,108>>,97,98,98,<<97,98,97,98,97,98>>}",
            ),
            (
                "bifs/binary_replace_encode_decode_unsigned",
                "ok {<<104,101,76,108,111>>,<<104,101,76,76,111>>,<<1,44>>,<<44,1>>,300,300,[97,98],<<1,2>>}",
            ),
            (
                "bifs/string_to_number_substr_and_padding",
                "ok {{42,[97,98]},{1.5,[97,98]},[101,108,108],3,[97,98,32,32],[48,48,97,98],\
                 [45,45,97,98,45,45],3,[98]}",
            ),
            (
                "bifs/string_next_grapheme_split_all_is_empty_equal",
                "ok {[97,98,99],[[97],[98],[99]],true,true}",
            ),
            (
                "bifs/lists_usort_splitwith_mapfoldr_sort_fun",
                "ok {[3,2,1],[1,2,3],[a,b],{[1,2],[3]},{[1,2],0}}",
            ),
            (
                "bifs/lists_all_any_flatmap_flatlength",
                "ok {true,false,false,[1,1,2,2],3}",
            ),
            (
                "bifs/lists_zipwith_enumerate",
                "ok {[4,6],[6],[{1,a},{2,b}],[{5,a},{6,b}]}",
            ),
            (
                "bifs/lists_keyreplace_keystore_keytake_keymap",
                "ok {[{a,1},{b,9}],[{a,1},{b,9}],{value,{b,2},[{a,1}]},[{a,2}],{a,1}}",
            ),
            (
                "bifs/lists_umerge_and_usort_of_lists",
                "ok {[1,2],[[1],[2]]}",
            ),
            (
                "bifs/maps_update_with_remove_merge_from_list",
                "ok {[{a,2}],[{z,0}],[{b,2}],[{a,1},{b,2}],2}",
            ),
            (
                "bifs/float_to_binary_and_back",
                "ok {<<49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,101,43,48,48>>,\
                 <<49,46,53,48>>,<<48,46,49>>,1.5,150.0,1.5,\
                 [49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,101,43,48,48]}",
            ),
            (
                "bifs/iolist_bitstring_and_size_bifs",
                "ok {3,<<97,98>>,<<1,2,3:4>>,3,1,true,false}",
            ),
            (
                "bifs/apply_and_binary_list_conversions",
                "ok {[2,1],2,{<<1,2>>,<<3>>},[1,2],<<1,2>>}",
            ),
            (
                "bifs/io_lib_format_integer_char_string_float",
                "ok {[49,48,32,97,32,[97,98]],<<123,49,44,50,125>>,[49,46,53,48],\
                 <<49,46,48,48,48,48,48,101,43,50>>,<<32,32,32,32,32,51,46,49,52,50>>}",
            ),
            (
                "bifs/monotonic_and_unique_integer_are_integers",
                "ok {true,true,true,true}",
            ),
        ] {
            assert_eq!(golden(id), stdout, "case {id}");
        }
        // The one new multi-key-map case renders every map result sorted (or measures it with
        // `maps:size/1`), so no map-internal key order can ever reach its golden.
        let expr = group()
            .into_iter()
            .find(|c| c.id == "bifs/maps_update_with_remove_merge_from_list")
            .expect("registered")
            .expr()
            .expect("eval case");
        assert!(
            expr.contains("lists:sort(maps:to_list("),
            "a multi-key map must be rendered sorted, got {expr}"
        );
    }
}