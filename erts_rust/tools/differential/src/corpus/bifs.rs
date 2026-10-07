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
    ]
}
