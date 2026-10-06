//! Contract **A.1** — the exception protocol: the three classes (`error`, `exit`,
//! `throw`) and the *reason term* each carries.
//!
//! The probe wrapper exists precisely for these: `erl -noshell -eval '<expr with an
//! uncaught error>'` prints **nothing** and exits `1`, so without the wrapper an
//! exception would be invisible to a stdout diff. With the wrapper a failure is one
//! observable line — `exception <class> <reason>` — and exit status `1`.
//!
//! Alongside the plain class cases, the group pins the **structured `error` reasons** a
//! raise carries (contract A.1). Each such case is `try (EXPR) catch error:Reason ->
//! Reason end`, so the raise is *caught inside the expression* and returned as an ordinary
//! value: the probe prints `ok <ETF of Reason>` with exit `0`, making the reason a
//! first-class, byte-exact observable (contract A.2's canonical encoding, observed through
//! A.1's protocol). The reason is asserted as its own term rather than the rendered
//! `exception error <reason>` line so the *structure* (tuple arity, which value the
//! failure carries) is pinned exactly.

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
        Case {
            id: "errors/tl_of_empty_list",
            description: "tl/1 of the empty list raises error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "tl([])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/element_zero_index",
            description: "element/2 with a zero index (one-based) raises error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "element(0, {1,2})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "errors/binary_to_integer_badarg",
            description: "binary_to_integer/1 rejects a non-digit binary with error:badarg",
            contracts: &[Contract::LanguageSemantics, Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "binary_to_integer(<<120>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // --- Structured `error` reasons, caught in-expression and returned as a value ---
        // `try (EXPR) catch error:Reason -> Reason end` turns the raise into an ordinary
        // value, so the probe prints `ok <ETF of Reason>` (exit 0) and the reason term is
        // pinned byte-exactly (an untupled failure renders as the bare atom, e.g. `badarith`).
        Case {
            id: "errors/case_clause_reason",
            description: "a failed case match raises error:{case_clause,Value} carrying the scrutinee",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (case 1 of 2 -> x end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,11,99,97,115,101,95,99,108,97,117,115,101,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/case_clause_carries_compound_value",
            description: "the {case_clause,Value} reason preserves a compound (list) scrutinee",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (case [a,b] of [x] -> y end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,11,99,97,115,101,95,99,108,97,117,115,101,108,0,0,0,2,119,1,97,119,1,98,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/if_clause_reason",
            description: "an if with no true branch raises error:if_clause (an untupled reason)",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin if false -> x end end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,9,105,102,95,99,108,97,117,115,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_reason",
            description: "a failed match raises error:{badmatch,Value} carrying the right-hand side",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin 5 = 6 end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,97,6>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_atom_value",
            description: "the {badmatch,Value} reason preserves an atom right-hand side",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin ok = error end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,119,5,101,114,114,111,114>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_tuple_value",
            description: "the {badmatch,Value} reason preserves a tuple whose size differs from the pattern",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin {A, B} = {1,2,3} end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,104,3,97,1,97,2,97,3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_empty_list_value",
            description: "matching a cons pattern against [] raises error:{badmatch,[]}",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin [H|T] = [] end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,106>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_binary_value",
            description: "matching a 1-byte binary pattern against a 2-byte binary raises {badmatch,Binary}",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin <<X:8>> = <<1,2>> end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,109,0,0,0,2,1,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_map_literal_value",
            description: "a literal pattern against a map raises error:{badmatch,#{}} carrying the map",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (begin 2 = #{} end) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,116,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badmatch_non_map_lhs_map_pattern",
            description: "an empty-map pattern against a non-map raises error:{badmatch,Value}",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (#{} = 2) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,8,98,97,100,109,97,116,99,104,97,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "errors/badarith_reason",
            description: "arithmetic on a non-number raises error:badarith (an untupled reason)",
            contracts: &[Contract::LanguageSemantics],
            kind: CaseKind::Eval {
                expr: "try (1 + a) catch error:Reason -> Reason end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,8,98,97,100,97,114,105,116,104>>"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, CaseKind, Probe};

    /// The structured-reason cases this group adds. Each evaluates
    /// `try (EXPR) catch error:Reason -> Reason end`, so the raise is caught *in-expression*
    /// and the reason becomes an ordinary value (`ok <ETF of Reason>`, exit `0`).
    const STRUCTURED_REASON_CASES: &[&str] = &[
        "errors/case_clause_reason",
        "errors/case_clause_carries_compound_value",
        "errors/if_clause_reason",
        "errors/badmatch_reason",
        "errors/badmatch_atom_value",
        "errors/badmatch_tuple_value",
        "errors/badmatch_empty_list_value",
        "errors/badmatch_binary_value",
        "errors/badmatch_map_literal_value",
        "errors/badmatch_non_map_lhs_map_pattern",
        "errors/badarith_reason",
    ];

    fn tokens(source: &str) -> impl Iterator<Item = &str> {
        source.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
    }

    #[test]
    fn structured_reason_cases_follow_the_caught_reason_form() {
        let cases = group();
        for id in STRUCTURED_REASON_CASES {
            let case = cases
                .iter()
                .find(|c| c.id == *id)
                .unwrap_or_else(|| panic!("case {id} is missing from the errors group"));
            let CaseKind::Eval { expr, probe, .. } = case.kind else {
                panic!("case {id} must be an Eval case");
            };
            assert_eq!(probe, Probe::TermToBinary, "case {id}");
            assert!(
                expr.starts_with("try (") && expr.ends_with(") catch error:Reason -> Reason end"),
                "case {id}: unexpected caught-reason form {expr:?}"
            );
            // The inner expression must not reuse the probe wrapper's variables (`V`/`C`/`R`):
            // a collision makes the oracle's `erl_eval` fail with `unsafe_var`.
            for var in ["V", "C", "R"] {
                assert!(
                    !tokens(expr).any(|token| token == var),
                    "case {id}: expression uses the reserved probe variable `{var}`"
                );
            }
            assert_eq!(case.golden.exit_code, 0, "case {id}: a caught reason is a value");
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {id} asserts stdout"));
            assert!(
                golden.starts_with("ok <<131,"),
                "case {id}: a caught reason is ETF-encoded, got {golden:?}"
            );
        }
    }

    #[test]
    fn structured_reason_cases_pin_distinct_reasons() {
        let cases = group();
        let mut reasons = std::collections::HashSet::new();
        for id in STRUCTURED_REASON_CASES {
            let golden = cases
                .iter()
                .find(|c| c.id == *id)
                .expect("case is registered")
                .golden
                .stdout
                .expect("asserts stdout");
            assert!(
                reasons.insert(golden),
                "two structured-reason cases assert the same reason bytes: {golden:?}"
            );
        }
    }
}
