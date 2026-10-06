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
    ]
}
