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

//! Oracle-dependent smoke test.
//!
//! Runs the `baseline` workload against the **real** system Erlang. When the oracle
//! cannot be resolved (`$ERTS_ORACLE` unset and no `erl` on `PATH`) the test *skips*
//! silently, so `cargo test -p bench` stays green in any environment.

use std::path::Path;

use bench::metrics::Status;
use bench::report::Profile;
use bench::runtime::resolve_oracle;
use bench::workload::{RunContext, Workload};
use bench::workloads::baseline::Baseline;

#[test]
fn baseline_runs_against_the_real_oracle() {
    let Ok(runtime) = resolve_oracle() else {
        eprintln!("skipping: oracle not resolvable");
        return;
    };
    if !runtime.self_check() {
        eprintln!("skipping: oracle failed its self-check");
        return;
    }

    let out = std::env::temp_dir().join(format!("bench-oracle-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    let ctx = RunContext::new(&runtime, Profile::Quick, Path::new(&out));
    let result = Baseline.run(&ctx);

    assert_eq!(
        result.status,
        Status::Ok,
        "baseline should run on a real oracle; status={:?} reason={:?} notes={:?}",
        result.status,
        result.reason,
        result.notes
    );
    let startup = result
        .metrics
        .iter()
        .find(|m| m.name == "startup_ms")
        .expect("startup_ms metric");
    assert_eq!(startup.summary.as_ref().unwrap().count, 10);
    assert!(startup.summary.as_ref().unwrap().mean > 0.0);

    let ops = result
        .metrics
        .iter()
        .find(|m| m.name == "compute_ops_per_sec")
        .expect("compute_ops_per_sec metric");
    assert!(ops.value.unwrap() > 0.0);

    let _ = std::fs::remove_dir_all(&out);
}
