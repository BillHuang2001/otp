//! Contract **A.1** — the exception protocol: the three classes (`error`, `exit`,
//! `throw`) and the *reason term* each carries.
//!
//! The probe wrapper exists precisely for these: `erl -noshell -eval '<expr with an
//! uncaught error>'` prints **nothing** and exits `1`, so without the wrapper an
//! exception would be invisible to a stdout diff. With the wrapper a failure is one
//! observable line — `exception <class> <reason>` — and exit status `1`.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.1 exception cases: class + reason for `error`, `exit` and `throw`.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "errors/throw_class",
            description: "throw/1 raises class throw with the thrown term as reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "throw(oops)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception throw oops"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/exit_normal",
            description: "exit/1 raises class exit with the given reason",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "exit(normal)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception exit normal"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/exit_tuple_reason",
            description: "exit/1 preserves a compound reason term",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "exit({shutdown,1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception exit {shutdown,1}"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/error_tuple_reason",
            description: "erlang:error/1 carries an arbitrary reason term",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "erlang:error({bad,1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error {bad,1}"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/hd_of_empty_list",
            description: "hd/1 of the empty list raises error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "hd([])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/element_out_of_range",
            description: "element/2 with an out-of-range index raises error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "element(5, {1,2})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/list_to_integer_badarg",
            description: "list_to_integer/1 rejects a non-numeric list with error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "list_to_integer(\"x\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
    ]
}
