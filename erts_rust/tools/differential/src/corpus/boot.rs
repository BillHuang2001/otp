//! Contract **A.9** — build / bootstrapping observability.
//!
//! A.9 is "the system can build and start itself". Its black-box residue is what the
//! *booted* system exposes about itself: the version banner, the code path, and the
//! set of loaded modules. Every assertion here is **structural** (`is_list`, `is_atom`,
//! …) rather than by exact value: the oracle is OTP 29 while the rewrite targets
//! OTP 30, so an exact release string would be a guaranteed false mismatch, and the
//! code path is machine-dependent. What both runtimes must share is the *shape*.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.9 cases: boot/version observability, asserted by shape.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "boot/system_version_is_list",
            description: "erlang:system_info(system_version) is a string (shape, not value)",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(erlang:system_info(system_version))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/code_path_is_list",
            description: "code:get_path/0 is a list of directory entries",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(code:get_path())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/all_loaded_is_list",
            description: "code:all_loaded/0 is a list of {Module, Beam} pairs",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_list(code:all_loaded())",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "boot/loaded_entry_is_tagged_tuple",
            description: "every code:all_loaded/0 entry is tagged by a module atom",
            contracts: &[Contract::Build],
            kind: CaseKind::Eval {
                expr: "is_atom(element(1, hd(code:all_loaded())))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
    ]
}
