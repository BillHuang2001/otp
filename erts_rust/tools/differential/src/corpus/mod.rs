//! The built-in conformance corpus: one small, fast case group per compatibility contract.
//!
//! Every case is a plain Erlang **expression** plus a [`Probe`] and, crucially, a
//! [`Golden`] expectation of the probe's output line. The golden makes the corpus
//! self-testing: today the oracle reproduces it while the Rust side (`beam_rt` is a
//! version-banner stub) is reported `unsupported`; if the oracle ever stops matching its
//! own golden, that is corpus drift — a real failure, not a skip.
//!
//! Keep the default corpus **small and fast** (smoke level, Part I §C): heavy suites are
//! milestone-only. Cases are grouped by the contract they exercise:
//!
//! | module | contract |
//! |---|---|
//! | [`etf`] | A.2 — `.beam` binary / `term_to_binary` bytes |
//! | [`terms`] | A.1 — language semantics (ordering, equality, exceptions) |
//! | [`bifs`] | A.7 — BIF / OTP app surface |
//! | [`system_info`] | A.8 — introspection |
//! | [`flags`] | A.6 — CLI / configuration |

mod bifs;
mod etf;
mod flags;
mod system_info;
mod terms;

use std::sync::OnceLock;

/// One of the nine ERTS compatibility contracts that the rewrite must reproduce exactly
/// (`ERTS_RUST_REWRITE_EVALUATION.md` §A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Contract {
    /// A.1 — language semantics (term ordering/equality, arithmetic, exceptions).
    LanguageSemantics,
    /// A.2 — `.beam` binary / assembly and the canonical External Term Format bytes.
    BeamAsm,
    /// A.3 — the NIF C ABI (`enif_*`).
    NifAbi,
    /// A.4 — the driver C ABI (`driver_*` / `erl_drv_*`).
    DriverAbi,
    /// A.5 — distribution: wire protocol / handshake.
    Distribution,
    /// A.6 — CLI flags and configuration.
    Cli,
    /// A.7 — OTP application contracts (preloaded/boot, `erts_internal`, `prim_*`, BIFs).
    OtpApps,
    /// A.8 — introspection and tooling (`erlang:system_info/1`).
    Introspection,
    /// A.9 — build / bootstrapping.
    Build,
}

impl Contract {
    /// The stable contract id (`"A.1"` … `"A.9"`).
    pub fn id(&self) -> &'static str {
        match self {
            Contract::LanguageSemantics => "A.1",
            Contract::BeamAsm => "A.2",
            Contract::NifAbi => "A.3",
            Contract::DriverAbi => "A.4",
            Contract::Distribution => "A.5",
            Contract::Cli => "A.6",
            Contract::OtpApps => "A.7",
            Contract::Introspection => "A.8",
            Contract::Build => "A.9",
        }
    }

    /// A one-line human label for the contract.
    pub fn label(&self) -> &'static str {
        match self {
            Contract::LanguageSemantics => "language semantics",
            Contract::BeamAsm => ".beam binary/ETF bytes",
            Contract::NifAbi => "NIF C ABI",
            Contract::DriverAbi => "driver C ABI",
            Contract::Distribution => "distribution wire protocol",
            Contract::Cli => "CLI flags and configuration",
            Contract::OtpApps => "OTP application contracts",
            Contract::Introspection => "introspection / system_info",
            Contract::Build => "build and bootstrapping",
        }
    }
}

impl serde::Serialize for Contract {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.id())
    }
}

/// How a case's expression is wrapped before it is handed to a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Probe {
    /// Print `term_to_binary/1` of the result — the default (contract A.2).
    TermToBinary,
    /// Print the result with `~w` (textual rendering, no ETF encode).
    Value,
}

impl Probe {
    /// Stable identifier used in listings (`"term_to_binary"` / `"value"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Probe::TermToBinary => "term_to_binary",
            Probe::Value => "value",
        }
    }
}

/// The independent expectation a case holds about the oracle's probe output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Golden {
    /// The expected probe stdout **line**, without the trailing newline that the probe's
    /// `~n` emits (`None` asserts nothing about stdout).
    pub stdout: Option<&'static str>,
    /// The expected process exit code (`0` for a value, `1` for a BEAM exception).
    pub exit_code: i32,
}

/// One conformance case: an expression, how to probe it, and what the oracle must print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Case {
    /// Stable case id, `<group>/<name>` (unique across the corpus).
    pub id: &'static str,
    /// One-line description of the behaviour being checked.
    pub description: &'static str,
    /// The compatibility contracts this case covers (at least one).
    pub contracts: &'static [Contract],
    /// The plain Erlang expression to evaluate.
    pub expr: &'static str,
    /// Extra argv inserted before `-eval` (e.g. `-<flag> <value>` for A.6 cases).
    pub extra_argv: &'static [&'static str],
    /// How the expression is wrapped into a self-terminating probe.
    pub probe: Probe,
    /// What the oracle must print for this case.
    pub golden: Golden,
}

impl Case {
    /// The full probe expression handed to both runtimes for this case.
    pub fn eval(&self) -> String {
        probe_expr(self.expr, self.probe)
    }

    /// The contract ids of this case, in declaration order.
    pub fn contract_ids(&self) -> Vec<&'static str> {
        self.contracts.iter().map(Contract::id).collect()
    }
}

static CASES: OnceLock<Vec<Case>> = OnceLock::new();

fn build_corpus() -> Vec<Case> {
    let mut cases = Vec::new();
    cases.extend(etf::group());
    cases.extend(terms::group());
    cases.extend(bifs::group());
    cases.extend(system_info::group());
    cases.extend(flags::group());
    cases
}

/// The whole built-in corpus (built once, in a stable order).
pub fn cases() -> &'static [Case] {
    CASES.get_or_init(build_corpus).as_slice()
}

/// Look a case up by its id.
pub fn find_case(id: &str) -> Option<&'static Case> {
    cases().iter().find(|case| case.id == id)
}

/// Wrap a plain expression into the self-terminating probe both runtimes are given.
///
/// The wrapper is mandatory: `erl -noshell -eval '<expr with an uncaught error>'` prints
/// *nothing* and exits 1, so without it a BEAM exception would be invisible to a stdout
/// diff. Both probes print exactly one line and then halt with the corresponding code:
/// `ok <repr>` / exit `0`, or `exception <class> <reason>` / exit `1`.
pub fn probe_expr(expr: &str, probe: Probe) -> String {
    match probe {
        Probe::TermToBinary => format!(
            "try ({expr}) of V -> io:format(\"ok ~w~n\", [term_to_binary(V)]), halt(0) \
             catch C:R -> io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end."
        ),
        Probe::Value => format!(
            "try ({expr}) of V -> io:format(\"ok ~w~n\", [V]), halt(0) \
             catch C:R -> io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{cases, find_case, probe_expr, Contract, Probe};
    use std::collections::HashSet;

    #[test]
    fn case_ids_are_unique_and_well_formed() {
        let mut seen = HashSet::new();
        for case in cases() {
            assert!(seen.insert(case.id), "duplicate case id {}", case.id);
            assert!(
                case.id.contains('/'),
                "case id {} is not <group>/<name>",
                case.id
            );
            assert!(!case.id.starts_with('/') && !case.id.ends_with('/'));
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
        }
    }

    #[test]
    fn every_case_covers_at_least_one_contract_and_every_group_is_represented() {
        let mut contracts = HashSet::new();
        for case in cases() {
            assert!(
                !case.contracts.is_empty(),
                "case {} covers no contract",
                case.id
            );
            for contract in case.contracts {
                contracts.insert(contract.id());
            }
        }
        // The smoke corpus must exercise the contract groups it claims to (A.1/A.2/A.6/A.7/A.8).
        for expected in ["A.1", "A.2", "A.6", "A.7", "A.8"] {
            assert!(contracts.contains(expected), "no case covers {expected}");
        }
        assert_eq!(
            Contract::Introspection.id(),
            "A.8",
            "ids are part of the report schema"
        );
        assert!(!Contract::BeamAsm.label().is_empty());
    }

    #[test]
    fn every_case_has_a_non_empty_golden_stdout() {
        for case in cases() {
            let golden = case
                .golden
                .stdout
                .unwrap_or_else(|| panic!("case {} has no golden stdout", case.id));
            assert!(!golden.is_empty(), "case {} has an empty golden", case.id);
            assert!(
                !golden.ends_with('\n'),
                "case {}: the golden is a line, without the probe's newline",
                case.id
            );
            assert!(
                golden.starts_with("ok ") || golden.starts_with("exception "),
                "case {}: unexpected golden shape {golden:?}",
                case.id
            );
            assert!(
                !case.expr.is_empty() && !case.expr.contains('\n'),
                "case {} has a suspicious expression",
                case.id
            );
            if golden.starts_with("exception ") {
                assert_ne!(
                    case.golden.exit_code, 0,
                    "case {}: an exception must not exit 0",
                    case.id
                );
            } else {
                assert_eq!(
                    case.golden.exit_code, 0,
                    "case {}: a successful probe exits 0",
                    case.id
                );
            }
        }
    }

    #[test]
    fn find_case_looks_cases_up_by_id() {
        let case = find_case("etf/term_to_binary_tuple").expect("case present");
        assert_eq!(case.contracts, &[Contract::BeamAsm]);
        assert_eq!(case.probe, Probe::TermToBinary);
        assert_eq!(case.golden.exit_code, 0);
        assert!(find_case("nope/nope").is_none());
    }

    #[test]
    fn probe_expr_wraps_the_expression_and_self_terminates() {
        let t2b = probe_expr("{ok,42}", Probe::TermToBinary);
        assert!(t2b.starts_with("try ({ok,42}) of V -> io:format(\"ok ~w~n\", [term_to_binary(V)]), halt(0)"));
        assert!(t2b.ends_with("halt(1) end."));
        assert!(t2b.contains("catch C:R -> io:format(\"exception ~w ~w~n\", [C, R])"));

        let value = probe_expr("1+1", Probe::Value);
        assert!(value.contains("io:format(\"ok ~w~n\", [V])"));
        assert!(!value.contains("term_to_binary"));
        assert_eq!(Probe::Value.as_str(), "value");
    }
}
