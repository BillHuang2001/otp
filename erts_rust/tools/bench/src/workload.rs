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

//! The workload plugin contract and the registry.
//!
//! A [`Workload`] is a small, self-contained benchmark driver: it knows its id,
//! category, the compatibility contracts it exercises, which runtime(s) may host it,
//! how to *honestly* probe its prerequisites, and how to run itself. The runner
//! guarantees that every scheduled `(workload × runtime)` pair yields exactly one
//! [`WorkloadResult`].

use std::path::Path;

use crate::metrics::{Category, Probe, Runtimes};
use crate::report::{Profile, WorkloadResult};
use crate::runtime::Runtime;

/// Everything a workload needs to run: which runtime, which profile and where to write
/// scratch output.
///
/// External-repo overrides (`--repo <id>=<path>`) are applied to the process
/// environment before scheduling (see [`crate::workloads::set_repo_override`]) and read
/// back by each workload through [`crate::workloads::resolve_repo`], so this context
/// stays minimal and runtime-agnostic.
pub struct RunContext<'a> {
    /// The runtime to drive (a resolved, `erl`-compatible binary).
    pub runtime: &'a Runtime,
    /// The benchmark profile.
    pub profile: Profile,
    /// A directory the workload may use for scratch output (under `out/`).
    pub out_dir: &'a Path,
}

impl<'a> RunContext<'a> {
    /// Builds a run context.
    pub fn new(runtime: &'a Runtime, profile: Profile, out_dir: &'a Path) -> RunContext<'a> {
        RunContext {
            runtime,
            profile,
            out_dir,
        }
    }
}

/// A benchmark workload plugin.
pub trait Workload {
    /// The stable workload id (used on the CLI via `--workloads`).
    fn id(&self) -> &'static str;
    /// A one-line human description.
    fn description(&self) -> &'static str;
    /// The benchmark category.
    fn category(&self) -> Category;
    /// The compatibility contracts this workload exercises (e.g. `["A.1", "A.9"]`).
    fn contracts(&self) -> &'static [&'static str];
    /// Which runtime(s) may host this workload.
    fn runtimes(&self) -> Runtimes;
    /// The honest prerequisite check.
    fn probe(&self) -> Probe;
    /// Runs the workload. Must never panic; failures become `status: failed`.
    fn run(&self, ctx: &RunContext) -> WorkloadResult;
}

/// The full workload registry, in listing order.
pub fn registry() -> Vec<Box<dyn Workload>> {
    crate::workloads::registry()
}

/// Looks a workload up by id.
pub fn find(id: &str) -> Option<Box<dyn Workload>> {
    registry().into_iter().find(|w| w.id() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_non_empty_and_ids_are_unique() {
        let reg = registry();
        assert!(!reg.is_empty());
        let mut ids: Vec<&str> = reg.iter().map(|w| w.id()).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate workload ids");
    }

    #[test]
    fn find_resolves_every_registered_id() {
        for w in registry() {
            let found = find(w.id()).expect("registered id must be findable");
            assert_eq!(found.id(), w.id());
            assert_eq!(found.category(), w.category());
        }
        assert!(find("does-not-exist").is_none());
    }

    #[test]
    fn every_workload_declares_contracts_and_a_description() {
        for w in registry() {
            assert!(!w.contracts().is_empty(), "{} has no contracts", w.id());
            assert!(!w.description().is_empty(), "{} has no description", w.id());
        }
    }

    #[test]
    fn run_context_is_cheap_to_build() {
        let rt = crate::runtime::resolve_rust();
        let ctx = RunContext::new(&rt, Profile::Quick, Path::new("/tmp"));
        assert_eq!(ctx.profile, Profile::Quick);
        assert_eq!(ctx.out_dir, Path::new("/tmp"));
        assert_eq!(ctx.runtime.kind, crate::runtime::RuntimeKind::Rust);
    }
}
