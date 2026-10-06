//! Contract **A.3** — the observable NIF surface.
//!
//! The NIF **C ABI** (`enif_*`) is not reachable through the black-box CLI: a NIF is
//! C code linked into the emulator, driven by the loader. What *is* observable is the
//! error surface a NIF exposes to Erlang code — `erlang:nif_error/1`, which a NIF
//! calls to raise a reason when it cannot fulfil a call. These cases pin that raise
//! exactly; the structural C-declaration half of A.3 (that a program referencing the
//! `enif_*` declarations still compiles) is asserted in `tests/abi.rs`.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.3 cases: the `erlang:nif_error/1` raise surface.
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
    ]
}
