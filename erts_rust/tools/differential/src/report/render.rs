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

//! The human-readable rendering of the reports.

use std::io::Write;

use crate::corpus::Contract;

use super::model::{ListReport, RunReport, RuntimeReport, SideReport};

/// Render a run report for a human reader.
pub fn render_human(report: &RunReport, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(
        out,
        "differential report — schema {} — mode {}",
        report.schema, report.mode
    )?;
    render_runtime(out, "oracle", &report.oracle)?;
    render_runtime(out, "rust", &report.rust)?;
    let summary = &report.summary;
    writeln!(
        out,
        "summary: total={} agree={} mismatch={} unsupported={} skipped={}",
        summary.total, summary.agree, summary.mismatch, summary.unsupported, summary.skipped
    )?;
    if let Some(ratchet) = &report.ratchet {
        writeln!(
            out,
            "ratchet: allowlist={} allowed={} unauthorized={} stale={}",
            ratchet.allowlist.as_deref().unwrap_or("(none)"),
            ratchet.allowed,
            ratchet.unauthorized,
            ratchet.stale
        )?;
    }
    if !report.by_contract.is_empty() {
        writeln!(out, "by contract:")?;
        for (id, counts) in &report.by_contract {
            writeln!(
                out,
                "  {id} total={} agree={} mismatch={} unsupported={} skipped={}",
                counts.total, counts.agree, counts.mismatch, counts.unsupported, counts.skipped
            )?;
        }
    }
    if report.cases.is_empty() {
        writeln!(out, "(no cases)")?;
        return Ok(());
    }
    for case in &report.cases {
        let contracts: Vec<&str> = case.contracts.iter().map(Contract::id).collect();
        let contracts = if contracts.is_empty() {
            String::new()
        } else {
            format!(" ({})", contracts.join(","))
        };
        writeln!(
            out,
            "\n[{}] {}{}",
            case.verdict.as_str().to_uppercase(),
            case.case_id,
            contracts
        )?;
        writeln!(out, "    {}", case.description)?;
        render_side(out, "oracle", &case.oracle)?;
        render_side(out, "rust  ", &case.rust)?;
        for diff in &case.diffs {
            writeln!(
                out,
                "    diff {}: oracle={:?} rust={:?}",
                diff.field, diff.oracle, diff.rust
            )?;
        }
    }
    Ok(())
}

fn render_runtime(
    out: &mut impl Write,
    label: &str,
    runtime: &RuntimeReport,
) -> std::io::Result<()> {
    match (&runtime.program, runtime.source) {
        (Some(program), Some(source)) if runtime.available => {
            writeln!(out, "{label}: {program} ({source})")
        }
        _ => writeln!(out, "{label}: unavailable"),
    }
}

fn render_side(out: &mut impl Write, label: &str, side: &SideReport) -> std::io::Result<()> {
    if !side.available {
        return writeln!(
            out,
            "    {label}: unsupported: {}",
            side.reason.as_deref().unwrap_or("unknown reason")
        );
    }
    let stdout = side.stdout.as_deref().unwrap_or("");
    let golden = match side.matched_golden {
        Some(true) => "golden=ok",
        Some(false) => "golden=MISMATCH",
        None => "golden=not-asserted",
    };
    writeln!(
        out,
        "    {label}: exit {} {golden} stdout={stdout:?}",
        side.exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string())
    )
}

/// Render the grouped `list --by-contract` view for a human reader.
///
/// One block per contract (ordered by id), showing the contract id, its label, its case
/// count and the ids of the cases that cover it.
pub fn render_list_grouped(list: &ListReport, out: &mut impl Write) -> std::io::Result<()> {
    let groups = list.by_contract.as_deref().unwrap_or(&[]);
    if groups.is_empty() {
        return writeln!(out, "(no contracts)");
    }
    for group in groups {
        writeln!(
            out,
            "{} — {} — {} case(s)",
            group.id, group.label, group.total
        )?;
        for case_id in &group.case_ids {
            writeln!(out, "  {case_id}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::compare::{SideResult, Verdict};
    use crate::report::test_support::report;
    use crate::report::{render_human, render_list_grouped, ListReport};

    #[test]
    fn render_human_is_readable_without_a_rust_runtime() {
        let report = report(Verdict::Unsupported, SideResult::unsupported("no beam_rt"), Some(true));
        let mut buf = Vec::new();
        render_human(&report, &mut buf).expect("render");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("schema differential/1"));
        assert!(text.contains("oracle: /x/erl ($PATH)"));
        assert!(text.contains("rust: unavailable"));
        assert!(text.contains("total=1"));
        assert!(text.contains("[UNSUPPORTED] etf/term_to_binary_tuple (A.2)"));
        assert!(text.contains("unsupported: no beam_rt"));
        assert!(text.contains("golden=ok"));
    }

    #[test]
    fn render_human_shows_the_by_contract_table() {
        let report = report(Verdict::Unsupported, SideResult::unsupported("no beam_rt"), Some(true));
        let mut buf = Vec::new();
        render_human(&report, &mut buf).expect("render");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("by contract:"), "{text}");
        assert!(
            text.contains("A.2 total=1 agree=0 mismatch=0 unsupported=1 skipped=0"),
            "{text}"
        );
    }

    #[test]
    fn render_list_grouped_prints_one_block_per_contract() {
        let grouped = ListReport::from_corpus_grouped();
        let mut buf = Vec::new();
        render_list_grouped(&grouped, &mut buf).expect("render");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("A.2 — .beam binary/ETF bytes — "), "{text}");
        assert!(text.contains("  modules/add"), "{text}");
    }
}
