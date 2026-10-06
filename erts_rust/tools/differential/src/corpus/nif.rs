//! Contract **A.3** — the observable NIF surface.
//!
//! The NIF **C ABI** (`enif_*`) is not reachable through the black-box CLI: a NIF is
//! C code linked into the emulator, driven by the loader. What *is* observable is the
//! error surface a NIF exposes to Erlang code — `erlang:nif_error/1,2`, which a NIF
//! calls to raise a reason when it cannot fulfil a call. These cases pin that raise
//! exactly, over a spread of reason *shapes* (atom, integer, list, flat and nested
//! tuples, and the `/2` arity); the structural C-declaration half of A.3 (that a program
//! referencing the `enif_*` declarations still compiles) is asserted in `tests/abi.rs`.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.3 cases: the `erlang:nif_error/1,2` raise surface.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "nif/nif_error_atom_reason",
            description: "erlang:nif_error/1 raises error with the given reason",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(foo)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error foo"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_tuple_reason",
            description: "erlang:nif_error/1 preserves a compound reason term",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error({nif_not_loaded, m, f, 0})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {nif_not_loaded,m,f,0}"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_default",
            description: "erlang:nif_error/1 with the conventional `undefined` reason",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(undefined)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error undefined"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_badarg_tuple_reason",
            description: "erlang:nif_error/1 carries the `{badarg, Args}` reason a NIF uses for bad input",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error({badarg, [1,2,3]})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {badarg,[1,2,3]}"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_nested_tuple_reason",
            description: "erlang:nif_error/1 preserves a nested compound reason",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error({nif_not_loaded, m, f, {1,2}})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {nif_not_loaded,m,f,{1,2}}"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_integer_reason",
            description: "erlang:nif_error/1 preserves a non-atom, non-tuple reason (integer)",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(1)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error 1"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_list_reason",
            description: "erlang:nif_error/1 preserves a list reason without string-rewriting it",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error([a,b,c])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error [a,b,c]"),
                exit_code: 1,
            },
        },
        Case {
            id: "nif/nif_error_two_uses_first_argument",
            description: "erlang:nif_error/2 raises with its first argument as the reason (the second is metadata)",
            contracts: &[Contract::NifAbi],
            kind: CaseKind::Eval {
                expr: "erlang:nif_error(foo, [1,2])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error foo"),
                exit_code: 1,
            },
        },
    ]
}
