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

//! JSON serialization: the schema identifiers and the report writers.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};

use super::model::RunReport;

/// The report schema identifier (bump on any backwards-incompatible change).
pub const SCHEMA: &str = "differential/1";

/// The schema identifier of the `list` listing.
pub const LIST_SCHEMA: &str = "differential/list/1";

/// The default output directory: the crate's gitignored `out/`.
pub fn default_out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("out")
}

/// Write a run report as `<dir>/report.json`, creating `dir` if needed.
///
/// `out == None` writes to [`default_out_dir`]. Returns the file that was written.
pub fn write_json(report: &RunReport, out: Option<&Path>) -> Result<PathBuf> {
    write_json_value(report, out)
}

/// Write any serializable report as `<dir>/report.json` (the `list` command uses this).
pub fn write_json_value<T: Serialize>(value: &T, out: Option<&Path>) -> Result<PathBuf> {
    let dir = match out {
        Some(dir) => dir.to_path_buf(),
        None => default_out_dir(),
    };
    std::fs::create_dir_all(&dir).map_err(|source| {
        Error::io(
            format!("failed to create the report directory '{}'", dir.display()),
            source,
        )
    })?;
    let path = dir.join("report.json");
    let json = to_json_string(value)?;
    std::fs::write(&path, json).map_err(|source| {
        Error::io(
            format!("failed to write the report '{}'", path.display()),
            source,
        )
    })?;
    Ok(path)
}

/// Serialize a value as pretty JSON, mapping failures onto a tool error.
pub fn to_json_string<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(|source| {
        Error::io(
            "failed to serialize the report as JSON",
            std::io::Error::new(std::io::ErrorKind::Other, source),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::compare::{SideResult, Verdict};
    use crate::corpus::Contract;
    use crate::report::test_support::{case, ratchet_report, report, spec};
    use crate::report::{render_human, ListReport, RunReport};
    use crate::runtime::{RuntimeId, RuntimeSource};

    use super::{default_out_dir, to_json_string, write_json_value, SCHEMA};

    #[test]
    fn the_schema_is_stable_and_complete() {
        let report = report(Verdict::Unsupported, SideResult::unsupported("no beam_rt"), Some(true));
        assert_eq!(report.schema, SCHEMA);
        let json: serde_json::Value = serde_json::from_str(&report.to_json_string().unwrap()).unwrap();
        assert_eq!(json["schema"], "differential/1");
        assert_eq!(json["mode"], "run");
        assert_eq!(json["oracle"]["available"], true);
        assert_eq!(json["oracle"]["program"], "/x/erl");
        assert_eq!(json["oracle"]["source"], "$PATH");
        assert_eq!(json["rust"]["available"], false);
        assert_eq!(json["rust"]["program"], serde_json::Value::Null);
        assert_eq!(json["summary"]["total"], 1);
        assert_eq!(json["summary"]["unsupported"], 1);
        let case = &json["cases"][0];
        assert_eq!(case["case_id"], "etf/term_to_binary_tuple");
        assert_eq!(case["contracts"][0], "A.2");
        assert_eq!(case["verdict"], "unsupported");
        assert!(case["eval"].as_str().unwrap().starts_with("try ({ok,42})"));
        assert_eq!(case["oracle"]["available"], true);
        assert_eq!(case["oracle"]["matched_golden"], true);
        assert_eq!(case["oracle"]["exit_code"], 0);
        assert_eq!(case["oracle"]["stdout_hex"], "6f6b203c3c3133312c39372c34323e3e0a");
        assert_eq!(case["rust"]["available"], false);
        assert!(case["rust"]["reason"].as_str().unwrap().contains("beam_rt"));
        assert_eq!(case["diffs"][0]["field"], "stdout");
    }

    #[test]
    fn optional_side_keys_are_omitted_rather_than_guessed() {
        let report = report(Verdict::Unsupported, SideResult::unsupported("no beam_rt"), None);
        let json: serde_json::Value = serde_json::from_str(&report.to_json_string().unwrap()).unwrap();
        let oracle = &json["cases"][0]["oracle"];
        assert!(
            oracle.get("matched_golden").is_none(),
            "no golden was asserted: {oracle}"
        );
        assert!(oracle.get("reason").is_none(), "the oracle ran: {oracle}");
        let rust = &json["cases"][0]["rust"];
        assert_eq!(rust["available"], false);
        assert!(rust["reason"].as_str().unwrap().contains("beam_rt"));
        assert!(
            rust.get("matched_golden").is_none(),
            "the Rust side never checks a golden: {rust}"
        );
        assert!(rust["stdout"].is_null(), "{rust}");
        assert!(rust["exit_code"].is_null(), "{rust}");
    }

    #[test]
    fn list_report_describes_the_corpus() {
        let list = ListReport::from_corpus();
        assert_eq!(list.schema, "differential/list/1");
        assert_eq!(list.total, crate::corpus::cases().len());
        assert!(list.total > 0);
        let json: serde_json::Value = serde_json::from_str(&to_json_string(&list).unwrap()).unwrap();
        assert_eq!(json["total"], list.total);
        assert_eq!(json["cases"][0]["contracts"][0]["id"], "A.2");
        assert_eq!(json["cases"][0]["contracts"][0]["label"], ".beam binary/ETF bytes");
        assert!(!json["cases"][0]["probe"].as_str().unwrap().is_empty());
    }

    #[test]
    fn write_json_defaults_to_the_gitignored_out_dir() {
        let expected = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out");
        assert_eq!(default_out_dir(), expected);
    }

    #[test]
    fn write_json_writes_a_file_in_the_requested_directory() {
        let dir = std::env::temp_dir().join(format!(
            "differential-report-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let list = ListReport::from_corpus();
        let path = write_json_value(&list, Some(&dir)).expect("write");
        assert_eq!(path, dir.join("report.json"));
        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(written.contains("\"schema\": \"differential/list/1\""));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn by_contract_is_an_additive_top_level_json_key() {
        let cases = vec![
            case(Verdict::Agree, vec![Contract::BeamAsm]),
            case(Verdict::Unsupported, vec![Contract::BeamAsm]),
        ];
        let report = RunReport::new(
            "suite",
            &spec(RuntimeId::Oracle, "/x/erl", RuntimeSource::Path),
            None,
            cases,
        );
        let json: serde_json::Value = serde_json::from_str(&report.to_json_string().unwrap()).unwrap();
        // The existing schema id and summary are unchanged.
        assert_eq!(json["schema"], "differential/1");
        assert_eq!(json["summary"]["total"], 2);
        assert_eq!(json["summary"]["agree"], 1);
        // The new key carries the same Summary shape, keyed by contract id.
        assert_eq!(json["by_contract"]["A.2"]["total"], 2);
        assert_eq!(json["by_contract"]["A.2"]["agree"], 1);
        assert_eq!(json["by_contract"]["A.2"]["mismatch"], 0);
        assert_eq!(json["by_contract"]["A.2"]["unsupported"], 1);
        assert_eq!(json["by_contract"]["A.2"]["skipped"], 0);
    }

    #[test]
    fn a_contract_free_run_has_an_empty_rollup() {
        let cases = vec![case(Verdict::Unsupported, Vec::new())];
        let report = RunReport::new(
            "compare",
            &spec(RuntimeId::Oracle, "/x/erl", RuntimeSource::Path),
            None,
            cases,
        );
        assert!(report.by_contract.is_empty());
        let json: serde_json::Value = serde_json::from_str(&report.to_json_string().unwrap()).unwrap();
        assert!(json["by_contract"].as_object().expect("object").is_empty());
    }

    #[test]
    fn list_grouped_adds_the_grouping_without_changing_the_plain_listing() {
        // The plain listing never carries `by_contract`, so `list` output is unchanged.
        let plain = ListReport::from_corpus();
        assert!(plain.by_contract.is_none());
        let plain_json = to_json_string(&plain).unwrap();
        assert!(
            !plain_json.contains("by_contract"),
            "the plain listing must not carry the grouping"
        );

        // The grouped listing keeps the schema/total/cases and ADDS the grouping.
        let grouped = ListReport::from_corpus_grouped();
        assert_eq!(grouped.schema, "differential/list/1");
        assert_eq!(grouped.total, plain.total);
        assert_eq!(grouped.cases.len(), plain.cases.len());
        let json: serde_json::Value = serde_json::from_str(&to_json_string(&grouped).unwrap()).unwrap();
        let groups = json["by_contract"].as_array().expect("by_contract array");
        assert!(!groups.is_empty());

        // Groups are ordered by contract id and unique, and each `total` matches its ids.
        let mut previous: Option<&str> = None;
        for group in groups {
            let id = group["id"].as_str().expect("id");
            let label = group["label"].as_str().expect("label");
            assert!(!label.is_empty(), "contract {id} has no label");
            let ids = group["case_ids"].as_array().expect("case_ids");
            assert_eq!(group["total"].as_u64().unwrap() as usize, ids.len());
            assert!(ids.iter().all(|value| value.as_str().is_some()));
            if let Some(previous) = previous {
                assert!(previous < id, "contracts are not ordered: {previous} then {id}");
            }
            previous = Some(id);
        }

        // A.2 covers the corpus ETF/beam cases including the two module-run agreement cases.
        let a2 = groups
            .iter()
            .find(|group| group["id"] == "A.2")
            .expect("A.2 is present");
        let ids: Vec<&str> = a2["case_ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert!(ids.contains(&"modules/add"), "{ids:?}");
        assert!(ids.contains(&"modules/fib"), "{ids:?}");
    }

    /// The `ratchet` key is purely additive: omitted without an allowlist (so a plain run is
    /// byte-for-byte unchanged) and rendered in both the JSON and the human output when
    /// present. `report_from_suite` (the allowlisted path) is covered in `ratchet.rs`.
    #[test]
    fn the_ratchet_key_is_additive_and_rendered_only_when_present() {
        let plain = report(Verdict::Unsupported, SideResult::unsupported("no beam_rt"), None);
        let json: serde_json::Value =
            serde_json::from_str(&plain.to_json_string().unwrap()).unwrap();
        assert!(json.get("ratchet").is_none(), "{json}");
        assert_eq!(json["schema"], SCHEMA, "an additive key never bumps the schema");
        let mut buf = Vec::new();
        render_human(&plain, &mut buf).expect("render");
        assert!(!String::from_utf8(buf).expect("utf8").contains("ratchet:"));

        let with = RunReport::new_with_ratchet(
            "suite",
            &spec(RuntimeId::Oracle, "/x/erl", RuntimeSource::Path),
            None,
            report(Verdict::Unsupported, SideResult::unsupported("no beam_rt"), None).cases,
            Some(ratchet_report()),
        );
        let json: serde_json::Value =
            serde_json::from_str(&with.to_json_string().unwrap()).unwrap();
        assert_eq!(json["ratchet"]["allowlist"], "known-gaps.txt");
        assert_eq!(json["ratchet"]["allowed"], 9);
        assert_eq!(json["ratchet"]["unauthorized"], 0);
        assert_eq!(json["ratchet"]["stale"], 0);
        let mut buf = Vec::new();
        render_human(&with, &mut buf).expect("render");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(
            text.contains("ratchet: allowlist=known-gaps.txt allowed=9 unauthorized=0 stale=0"),
            "{text}"
        );
    }
}
