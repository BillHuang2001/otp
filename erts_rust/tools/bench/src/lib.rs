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

//! `bench` — the Rust ERTS **benchmark driver**.
//!
//! `bench` builds and runs the external pure-Erlang/Elixir target workloads named in
//! `ERTS_RUST_REWRITE_TEST_PROJECTS.md` and **always records the system-ERTS
//! baseline**, capturing the success metrics mandated by
//! `ERTS_RUST_REWRITE_EVALUATION.md` §G.7/§I: **startup latency, p50/p99 tail latency,
//! throughput and peak memory**.
//!
//! It is a **library + binary** crate: the binary is what a human or CI drives; the
//! library exists so `cargo test -p bench` can assert the harness itself (statistics,
//! JSON-schema stability, probe logic, CLI parsing, report comparison).
//!
//! # Design contract
//!
//! * **Pure driver/observer.** It never modifies the Rust runtime and never writes to
//!   the external repos (it may build/run them).
//! * **Honest reporting.** A workload that cannot run is `skipped(<reason>)` — never
//!   `ok`, and metrics are never fabricated.
//! * **Drop-in runtimes.** Both runtimes are invoked with the *same argv as `erl`*
//!   (contract A.6); only the program path is chosen.
//! * **No new dependencies.** Just `serde`, `serde_json` and the `beam_common` logging
//!   facade; the CLI is hand-rolled and `unsafe` is forbidden.
//!
//! # Example
//!
//! ```no_run
//! use bench::cli::{parse, Command};
//!
//! let args: Vec<String> = vec!["bench".into(), "run".into(), "--quick".into()];
//! match parse(&args).unwrap() {
//!     Command::Run(run) => assert_eq!(run.profile, bench::Profile::Quick),
//!     _ => unreachable!(),
//! }
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod cli;
pub mod compare;
pub mod error;
pub mod http_load;
pub mod metrics;
pub mod proc;
pub mod report;
pub mod run;
pub mod runtime;
pub mod scratch;
pub mod stats;
pub mod workload;
pub mod workloads;

pub use error::{BenchError, Result};
pub use metrics::{Category, Metric, Probe, Runtimes, Status};
pub use report::{Profile, Report, WorkloadResult, SCHEMA_VERSION};
pub use runtime::{Runtime, RuntimeInfo, RuntimeKind};
pub use stats::Summary;
pub use workload::{RunContext, Workload};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, 1);
    }

    #[test]
    fn registry_is_reachable_through_the_crate_root() {
        assert!(!workload::registry().is_empty());
    }

    #[test]
    fn root_reexports_are_usable() {
        let m = Metric::scalar("x", "u", 1.0);
        assert_eq!(m.name, "x");
        let r = WorkloadResult::new("w", RuntimeKind::Sys, Category::Startup, "d");
        assert_eq!(r.workload, "w");
        assert_eq!(Profile::Full.as_str(), "full");
        assert_eq!(Status::Ok.as_str(), "ok");
        let _: Option<Report> = None;
        let _: Option<BenchError> = None;
    }
}
