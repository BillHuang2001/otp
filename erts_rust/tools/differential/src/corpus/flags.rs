//! Contract **A.6** — CLI flags and configuration.
//!
//! `init:get_argument/1` is the observable side of the emulator command line: a runtime
//! that accepts the same argv must expose the same arguments, including user flags it
//! does not know about (`-differential_test_flag hello` here).

use super::{Case, CaseKind, Contract, Golden, Probe};

/// The A.6 cases: the `-noshell` flag and an arbitrary user flag.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "flags/noshell_argument",
            description: "the emulator argv (which always carries -noshell) is visible to init",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(noshell)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/custom_argument",
            description: "an unknown -flag with a value is stored verbatim for init:get_argument/1",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_test_flag)",
                extra_argv: &["-differential_test_flag", "hello"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[[104,101,108,108,111]]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/bare_flag_has_no_value",
            description: "a bare -flag with no argument is stored as an empty value list",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_bare_flag)",
                extra_argv: &["-differential_bare_flag"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,[[]]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/absent_argument_is_error",
            description: "an argument that was not passed is reported as the atom `error`",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(differential_absent_flag)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok error"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/plain_arguments_are_empty",
            description: "init:get_plain_arguments/0 excludes every modelled switch",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_plain_arguments()",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        Case {
            id: "flags/pa_argument_is_stored",
            description: "a -pa code-path switch is recorded verbatim for init:get_argument/1",
            contracts: &[Contract::Cli],
            kind: CaseKind::Eval {
                expr: "init:get_argument(pa)",
                extra_argv: &["-pa", "/differential/xyz"],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok {ok,[[[47,100,105,102,102,101,114,101,110,116,105,97,108,47,120,121,122]]]}",
                ),
                exit_code: 0,
            },
        },
    ]
}
