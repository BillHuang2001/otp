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
    ]
}
