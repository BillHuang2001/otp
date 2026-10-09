// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The report data model: the serde structs and their constructors.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::compare::{FieldDiff, SideResult, Verdict};
use crate::corpus::{self, Case, Contract};
use crate::error::Result;
use crate::harness::{CaseOutcome, SuiteOutcome};
use crate::ratchet::RatchetReport;
use crate::runtime::{RuntimeId, RuntimeSpec};

use super::json::{to_json_string, LIST_SCHEMA, SCHEMA};

/// The whole run report: the resolved runtimes, the summary counts and every case.
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    /// Always [`SCHEMA`].
    pub schema: &'static str,
    /// `"run"`, `"compare"` or `"suite"`.
    pub mode: &'static str,
    /// The oracle runtime that was used.
    pub oracle: RuntimeReport,
    /// The Rust runtime, whether or not it exists.
    pub rust: RuntimeReport,
    /// Verdict counts over `cases`.
    pub summary: Summary,
    /// The known-gaps ratchet classification. Present only when an allowlist drove the run
    /// (`suite --allowlist`); omitted from the JSON otherwise, so a run without one is
    /// byte-for-byte unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratchet: Option<RatchetReport>,
    /// Verdict counts rolled up per compatibility contract, keyed by the stable contract id
    /// (`"A.1"` … `"A.9"`) and ordered by it. A case contributes to every contract it
    /// declares, so these totals need not sum to [`Summary::total`]. Only contracts that
    /// appear in the run are present (an empty object for a corpus-free run, e.g. an ad-hoc
    /// `compare`).
    pub by_contract: BTreeMap<&'static str, Summary>,
    /// One entry per executed case, in execution order.
    pub cases: Vec<CaseReport>,
}

impl RunReport {
    /// Build a report from resolved runtimes and per-case reports.
    pub fn new(
        mode: &'static str,
        oracle: &RuntimeSpec,
        rust: Option<&RuntimeSpec>,
        cases: Vec<CaseReport>,
    ) -> RunReport {
        RunReport::new_with_ratchet(mode, oracle, rust, cases, None)
    }

    /// Build a report from resolved runtimes, per-case reports and an optional ratchet.
    pub fn new_with_ratchet(
        mode: &'static str,
        oracle: &RuntimeSpec,
        rust: Option<&RuntimeSpec>,
        cases: Vec<CaseReport>,
        ratchet: Option<RatchetReport>,
    ) -> RunReport {
        let summary = Summary::of(&cases);
        let by_contract = by_contract_summary(&cases);
        RunReport {
            schema: SCHEMA,
            mode,
            oracle: RuntimeReport::of(RuntimeId::Oracle, Some(oracle)),
            rust: RuntimeReport::of(RuntimeId::Rust, rust),
            summary,
            ratchet,
            by_contract,
            cases,
        }
    }

    /// Build a one-case report (the `run` and `compare` commands).
    pub fn from_case(mode: &'static str, outcome: &CaseOutcome) -> RunReport {
        RunReport::new(
            mode,
            &outcome.oracle_spec,
            outcome.rust_spec.as_ref(),
            vec![CaseReport::from_outcome(outcome)],
        )
    }

    /// Build the report of a whole suite run.
    pub fn from_suite(suite: &SuiteOutcome) -> RunReport {
        RunReport::new(
            "suite",
            &suite.context.oracle,
            suite.context.rust.as_ref(),
            suite.cases.iter().map(CaseReport::from_outcome).collect(),
        )
    }

    /// Render this report as pretty JSON (the same bytes `--json` prints).
    pub fn to_json_string(&self) -> Result<String> {
        to_json_string(self)
    }
}

/// One runtime's identity and availability.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeReport {
    /// `"oracle"` or `"rust"`.
    pub id: &'static str,
    /// Whether this runtime was resolved (and therefore observed).
    pub available: bool,
    /// The resolved program path, if any.
    pub program: Option<String>,
    /// How the program was resolved (`"$PATH"`, `"$ERTS_ORACLE"`, `"workspace-default"`).
    pub source: Option<&'static str>,
}

impl RuntimeReport {
    fn of(id: RuntimeId, spec: Option<&RuntimeSpec>) -> RuntimeReport {
        match spec {
            Some(spec) => RuntimeReport {
                id: id.id(),
                available: true,
                program: Some(spec.program.display().to_string()),
                source: Some(spec.source.as_str()),
            },
            None => RuntimeReport {
                id: id.id(),
                available: false,
                program: None,
                source: None,
            },
        }
    }
}

/// Verdict counts over a set of case reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Number of cases in the report.
    pub total: usize,
    /// Cases where both sides agreed.
    pub agree: usize,
    /// Cases where the sides (or the oracle and its golden) disagreed.
    pub mismatch: usize,
    /// Cases where a side could not be observed.
    pub unsupported: usize,
    /// Cases that were intentionally not executed.
    pub skipped: usize,
}

impl Summary {
    /// The zero summary (no cases, no verdicts).
    fn empty() -> Summary {
        Summary {
            total: 0,
            agree: 0,
            mismatch: 0,
            unsupported: 0,
            skipped: 0,
        }
    }

    /// Count one more case that produced `verdict`.
    fn add(&mut self, verdict: Verdict) {
        self.total += 1;
        match verdict {
            Verdict::Agree => self.agree += 1,
            Verdict::Mismatch => self.mismatch += 1,
            Verdict::Unsupported => self.unsupported += 1,
            Verdict::Skipped => self.skipped += 1,
        }
    }

    /// Count the verdicts of `cases`.
    pub fn of(cases: &[CaseReport]) -> Summary {
        let mut summary = Summary::empty();
        for case in cases {
            summary.add(case.verdict);
        }
        summary
    }
}

/// Roll the case verdicts up per compatibility contract.
///
/// A case may cover several contracts, so it contributes to each contract it declares; the
/// per-contract `total`s therefore need not sum to the overall [`Summary::total`]. The map
/// is keyed by the stable contract id (`"A.1"` … `"A.9"`) and ordered by it, keeping the
/// report deterministic regardless of corpus order.
fn by_contract_summary(cases: &[CaseReport]) -> BTreeMap<&'static str, Summary> {
    let mut by_contract: BTreeMap<&'static str, Summary> = BTreeMap::new();
    for case in cases {
        for contract in &case.contracts {
            by_contract
                .entry(contract.id())
                .or_insert_with(Summary::empty)
                .add(case.verdict);
        }
    }
    by_contract
}

/// One case's report entry.
#[derive(Debug, Clone, Serialize)]
pub struct CaseReport {
    /// The case id.
    pub case_id: String,
    /// The case description.
    pub description: String,
    /// The compatibility contracts covered (serialized as `"A.1"`…`"A.9"`).
    pub contracts: Vec<Contract>,
    /// The probe expression handed to both runtimes.
    pub eval: String,
    /// The verdict (`"agree"`, `"mismatch"`, `"unsupported"`, `"skipped"`).
    pub verdict: Verdict,
    /// The oracle side.
    pub oracle: SideReport,
    /// The Rust side.
    pub rust: SideReport,
    /// Field-by-field differences between the two observed sides (empty unless both ran).
    pub diffs: Vec<FieldDiff>,
}

impl CaseReport {
    /// Project a [`CaseOutcome`] onto the report schema.
    pub fn from_outcome(outcome: &CaseOutcome) -> CaseReport {
        let matched_golden = if outcome.golden_asserted {
            Some(outcome.golden.matched)
        } else {
            None
        };
        CaseReport {
            case_id: outcome.case_id.clone(),
            description: outcome.description.clone(),
            contracts: outcome.contracts.clone(),
            eval: outcome.eval.clone(),
            verdict: outcome.verdict,
            oracle: SideReport::of(&outcome.oracle, matched_golden),
            rust: SideReport::of(&outcome.rust, None),
            diffs: outcome.diffs.clone(),
        }
    }
}

/// One side's observation, as reported.
#[derive(Debug, Clone, Serialize)]
pub struct SideReport {
    /// Whether the side ran at all.
    pub available: bool,
    /// Why the side did not run (key omitted when the side ran).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Captured stdout as text (`null` when the side did not run).
    pub stdout: Option<String>,
    /// Captured stdout as dependency-free lowercase hex (`null` when the side did not run).
    pub stdout_hex: Option<String>,
    /// The process exit code (`null` when the side did not run, or when signalled).
    pub exit_code: Option<i32>,
    /// Whether the observation reproduced the case golden (key omitted when no golden was
    /// asserted, and for the Rust side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_golden: Option<bool>,
}

impl SideReport {
    fn of(side: &SideResult, matched_golden: Option<bool>) -> SideReport {
        match side {
            SideResult::Observed(observation) => SideReport {
                available: true,
                reason: None,
                stdout: Some(String::from_utf8_lossy(&observation.stdout).into_owned()),
                stdout_hex: Some(crate::compare::hex_string(&observation.stdout)),
                exit_code: observation.exit_code,
                matched_golden,
            },
            SideResult::Unsupported { reason } => SideReport {
                available: false,
                reason: Some(reason.clone()),
                stdout: None,
                stdout_hex: None,
                exit_code: None,
                matched_golden: None,
            },
        }
    }
}

/// The `list` listing: the corpus, its contracts and its prerequisites.
#[derive(Debug, Clone, Serialize)]
pub struct ListReport {
    /// Always [`LIST_SCHEMA`].
    pub schema: &'static str,
    /// The number of cases.
    pub total: usize,
    /// One entry per corpus case, in corpus order.
    pub cases: Vec<ListEntry>,
    /// The per-contract grouping. Present only with `differential list --by-contract`;
    /// omitted from the JSON otherwise, so the plain listing is byte-for-byte unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_contract: Option<Vec<ContractGroup>>,
}

impl ListReport {
    /// Build the listing of the built-in corpus.
    pub fn from_corpus() -> ListReport {
        let cases = corpus::cases();
        ListReport {
            schema: LIST_SCHEMA,
            total: cases.len(),
            cases: cases.iter().map(ListEntry::of).collect(),
            by_contract: None,
        }
    }

    /// Build the listing of the built-in corpus **grouped by compatibility contract**
    /// (the `differential list --by-contract` view).
    pub fn from_corpus_grouped() -> ListReport {
        ListReport {
            by_contract: Some(contract_groups()),
            ..ListReport::from_corpus()
        }
    }
}

/// One contract's cases in the grouped `list --by-contract` view.
#[derive(Debug, Clone, Serialize)]
pub struct ContractGroup {
    /// The stable contract id (`"A.1"` … `"A.9"`).
    pub id: &'static str,
    /// The human label of the contract.
    pub label: &'static str,
    /// How many corpus cases cover this contract.
    pub total: usize,
    /// The ids of those cases, in corpus order.
    pub case_ids: Vec<&'static str>,
}

/// Group the corpus by compatibility contract, ordered by contract id.
fn contract_groups() -> Vec<ContractGroup> {
    let mut groups: BTreeMap<&'static str, (&'static str, Vec<&'static str>)> = BTreeMap::new();
    for case in corpus::cases() {
        for contract in case.contracts {
            groups
                .entry(contract.id())
                .or_insert_with(|| (contract.label(), Vec::new()))
                .1
                .push(case.id);
        }
    }
    groups
        .into_iter()
        .map(|(id, (label, case_ids))| ContractGroup {
            id,
            label,
            total: case_ids.len(),
            case_ids,
        })
        .collect()
}

/// One corpus entry in the `list` listing.
#[derive(Debug, Clone, Serialize)]
pub struct ListEntry {
    /// The case id.
    pub case_id: &'static str,
    /// The case description.
    pub description: &'static str,
    /// The case kind: the probe wrapper for an expression case, `"module-run"` for a case
    /// that compiles and runs an embedded module.
    pub probe: &'static str,
    /// The contracts covered, with their labels.
    pub contracts: Vec<ContractEntry>,
}

impl ListEntry {
    fn of(case: &Case) -> ListEntry {
        ListEntry {
            case_id: case.id,
            description: case.description,
            probe: case.kind_label(),
            contracts: case
                .contracts
                .iter()
                .map(|contract| ContractEntry {
                    id: contract.id(),
                    label: contract.label(),
                })
                .collect(),
        }
    }
}

/// A contract id with its human label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContractEntry {
    /// The stable id (`"A.1"`…`"A.9"`).
    pub id: &'static str,
    /// The human label.
    pub label: &'static str,
}

#[cfg(test)]
mod tests {
    use crate::compare::{verdict_with, Observables, SideResult, Verdict};
    use crate::corpus::{find_case, Contract};
    use crate::harness::{run_case, run_case_in, RuntimeContext};
    use crate::report::test_support::{case, report, spec};
    use crate::report::{RunReport, Summary, SCHEMA};
    use crate::runtime::{resolve_oracle, RuntimeId, RuntimeSource};

    #[test]
    fn summary_counts_every_verdict() {
        let mut cases = Vec::new();
        for verdict in [
            Verdict::Agree,
            Verdict::Mismatch,
            Verdict::Unsupported,
            Verdict::Skipped,
        ] {
            let mut base = report(verdict, SideResult::unsupported("x"), Some(true));
            cases.append(&mut base.cases);
        }
        let summary = Summary::of(&cases);
        assert_eq!(summary.total, 4);
        assert_eq!(summary.agree, 1);
        assert_eq!(summary.mismatch, 1);
        assert_eq!(summary.unsupported, 1);
        assert_eq!(summary.skipped, 1);
    }

    /// A real (oracle-backed) run must never report *fabricated* agreement.
    ///
    /// The property is pinned deterministically, independent of how capable the Rust
    /// runtime is today: with **no** Rust runtime the Rust side is necessarily
    /// `Unsupported`, so the verdict can never be `Agree`. When the runtime *is* capable
    /// the report's verdict must equal the one re-derived from the recorded observations,
    /// and an `Agree` must be backed by an identical stdout + exit status. Skipped loudly
    /// when the oracle is unavailable.
    #[test]
    fn a_real_run_never_reports_fabricated_agreement() {
        let case = find_case("etf/term_to_binary_tuple").expect("case");

        // (1) A real oracle run with NO Rust runtime: the Rust side is necessarily
        //     `Unsupported`, so the report can never claim agreement.
        let oracle = match resolve_oracle() {
            Ok(oracle) => oracle,
            Err(err) => {
                eprintln!("SKIP: oracle erl not available ({err})");
                return;
            }
        };
        let no_rust = RuntimeContext {
            oracle,
            rust: None,
        };
        let outcome = match run_case_in(&no_rust, case) {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!("SKIP: oracle erl not available ({err})");
                return;
            }
        };
        assert!(
            !outcome.rust.is_observed(),
            "no Rust runtime ⇒ the Rust side must be unsupported"
        );
        let report = RunReport::from_case("run", &outcome);
        assert_eq!(report.schema, SCHEMA);
        assert_ne!(report.cases[0].verdict, Verdict::Agree);
        assert_eq!(report.summary.agree, 0);

        // (2) The same property, still meaningful once the runtime IS capable: re-run the
        //     case against the real resolved runtime and require the report's verdict to be
        //     exactly the one re-derived from the recorded observations, so a fabricated
        //     `Agree` can never survive.
        let outcome = match run_case(case) {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!("SKIP: oracle erl not available ({err})");
                return;
            }
        };
        let report = RunReport::from_case("run", &outcome);
        let rederived =
            verdict_with(&outcome.oracle, &outcome.rust, &outcome.golden, Observables::All);
        assert_eq!(report.cases[0].verdict, rederived);
        if rederived == Verdict::Agree {
            let oracle_obs = outcome.oracle.observation().expect("oracle observed");
            let rust_obs = outcome.rust.observation().expect("rust observed");
            assert_eq!(
                rust_obs.stdout, oracle_obs.stdout,
                "an Agree must be backed by identical stdout"
            );
            assert_eq!(
                rust_obs.exit_code, oracle_obs.exit_code,
                "an Agree must be backed by an identical exit status"
            );
        }
    }

    #[test]
    fn by_contract_rolls_up_every_verdict_per_contract() {
        let cases = vec![
            case(Verdict::Agree, vec![Contract::BeamAsm]),
            case(Verdict::Unsupported, vec![Contract::BeamAsm]),
            case(Verdict::Mismatch, vec![Contract::LanguageSemantics]),
            case(Verdict::Skipped, vec![Contract::OtpApps]),
        ];
        let report = RunReport::new(
            "suite",
            &spec(RuntimeId::Oracle, "/x/erl", RuntimeSource::Path),
            None,
            cases,
        );
        assert_eq!(report.by_contract.len(), 3);
        let a2 = report.by_contract["A.2"];
        assert_eq!(a2.total, 2);
        assert_eq!(a2.agree, 1);
        assert_eq!(a2.unsupported, 1);
        assert_eq!(a2.mismatch, 0);
        assert_eq!(a2.skipped, 0);
        assert_eq!(report.by_contract["A.1"].mismatch, 1);
        assert_eq!(report.by_contract["A.7"].skipped, 1);
        // The rollup is additive: the overall summary is untouched.
        assert_eq!(report.summary.total, 4);
        assert_eq!(report.summary.agree, 1);
    }

    #[test]
    fn a_case_contributes_to_every_contract_it_declares() {
        let cases = vec![case(Verdict::Agree, vec![Contract::BeamAsm, Contract::Cli])];
        let report = RunReport::new(
            "suite",
            &spec(RuntimeId::Oracle, "/x/erl", RuntimeSource::Path),
            None,
            cases,
        );
        assert_eq!(report.by_contract["A.2"].total, 1);
        assert_eq!(report.by_contract["A.6"].total, 1);
        // One case, two contract entries: the per-contract totals exceed the case count.
        assert_eq!(report.summary.total, 1);
    }
}
