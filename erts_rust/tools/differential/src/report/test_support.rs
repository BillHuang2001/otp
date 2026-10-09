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

//! Shared fixtures for the `report` module's unit tests (compiled only under `cfg(test)`).

use std::path::PathBuf;

use crate::compare::{FieldDiff, GoldenCheck, SideResult, Verdict};
use crate::corpus::Contract;
use crate::harness::CaseOutcome;
use crate::ratchet::RatchetReport;
use crate::runtime::{Observation, RuntimeId, RuntimeSource, RuntimeSpec};

use super::model::{CaseReport, RunReport};

pub(crate) fn spec(id: RuntimeId, program: &str, source: RuntimeSource) -> RuntimeSpec {
    RuntimeSpec {
        id,
        program: PathBuf::from(program),
        source,
    }
}

fn observation(stdout: &[u8], exit_code: Option<i32>) -> Observation {
    Observation {
        runtime: RuntimeId::Rust,
        program: PathBuf::from("/x/beam_rt"),
        argv: vec!["-noshell".to_string()],
        stdout: stdout.to_vec(),
        stderr: Vec::new(),
        exit_code,
    }
}

pub(crate) fn report(verdict: Verdict, rust: SideResult, matched_golden: Option<bool>) -> RunReport {
    let oracle = spec(RuntimeId::Oracle, "/x/erl", RuntimeSource::Path);
    let outcome = CaseOutcome {
        case_id: "etf/term_to_binary_tuple".to_string(),
        description: "a case".to_string(),
        contracts: vec![Contract::BeamAsm],
        eval: "try ({ok,42}) of V -> ok end.".to_string(),
        oracle_argv: vec!["-noshell".to_string()],
        rust_argv: vec!["-noshell".to_string()],
        oracle: SideResult::observed(observation(b"ok <<131,97,42>>\n", Some(0))),
        rust,
        golden: GoldenCheck {
            matched: matched_golden.unwrap_or(true),
            diffs: Vec::new(),
        },
        golden_asserted: matched_golden.is_some(),
        verdict,
        diffs: vec![FieldDiff::new("stdout", "ok 1", "ok 2")],
        oracle_spec: oracle.clone(),
        rust_spec: None,
    };
    RunReport::new(
        "run",
        &oracle,
        None,
        vec![CaseReport::from_outcome(&outcome)],
    )
}

/// A synthetic case report with the given verdict and contracts (no runtime needed).
pub(crate) fn case(verdict: Verdict, contracts: Vec<Contract>) -> CaseReport {
    let outcome = CaseOutcome {
        case_id: "synthetic/case".to_string(),
        description: "a synthetic case".to_string(),
        contracts,
        eval: "try (ok) of V -> ok end.".to_string(),
        oracle_argv: vec!["-noshell".to_string()],
        rust_argv: vec!["-noshell".to_string()],
        oracle: SideResult::observed(observation(b"ok\n", Some(0))),
        rust: SideResult::unsupported("no beam_rt"),
        golden: GoldenCheck {
            matched: true,
            diffs: Vec::new(),
        },
        golden_asserted: true,
        verdict,
        diffs: Vec::new(),
        oracle_spec: spec(RuntimeId::Oracle, "/x/erl", RuntimeSource::Path),
        rust_spec: None,
    };
    CaseReport::from_outcome(&outcome)
}

pub(crate) fn ratchet_report() -> RatchetReport {
    RatchetReport {
        allowlist: Some("known-gaps.txt".to_string()),
        allowed: 9,
        unauthorized: 0,
        stale: 0,
    }
}
