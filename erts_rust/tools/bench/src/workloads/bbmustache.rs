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

//! `bbmustache` — the pure-Erlang Mustache template library's own `eunit` suite, run
//! under both runtimes.
//!
//! A thin [`Workload`] over the shared [`eunit_project`](super::eunit_project) machinery:
//! the [project table](super::eunit_project::BBMUSTACHE) carries everything specific (the
//! checkout, the rebar3 `test`-profile flags `+export_all -Dnamespaced_types
//! -Dunicode_supported`, the one suite module, the intended **79** tests). The library's
//! `jsone`/`mustache_spec` rebar deps are referenced only from `ct/`, never from the eunit
//! suite, so staging `src/` + `test/` is self-contained — no dependency fetching.
//!
//! The suite is real application-level Erlang — template parsing (binary/string/unicode
//! handling, `try`/`catch`, map and proplist contexts) and rendering — so it is a `compute`
//! workload on the language-semantics (A.1) and OTP app/eunit (A.7) contracts. See the
//! shared module doc for why that is the honest category and why A.9 is deliberately not
//! claimed.

use crate::metrics::{Category, Probe, Runtimes};
use crate::report::WorkloadResult;
use crate::workload::{RunContext, Workload};
use crate::workloads::eunit_project::{self, Project, SharedLedger};

/// The `bbmustache` workload.
pub struct Bbmustache {
    /// The cross-side test-count record (one per run, shared with this run's other side).
    ledger: SharedLedger,
}

impl Bbmustache {
    /// A workload instance with a fresh, empty side ledger.
    pub fn new() -> Bbmustache {
        Bbmustache {
            ledger: eunit_project::shared_ledger(),
        }
    }

    /// The project table this workload runs.
    fn project() -> &'static Project {
        &eunit_project::BBMUSTACHE
    }
}

impl Default for Bbmustache {
    fn default() -> Self {
        Bbmustache::new()
    }
}

impl Workload for Bbmustache {
    fn id(&self) -> &'static str {
        Self::project().id
    }

    fn description(&self) -> &'static str {
        Self::project().description
    }

    fn category(&self) -> Category {
        Self::project().category
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.1", "A.7"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::Both
    }

    fn probe(&self) -> Probe {
        eunit_project::probe(Self::project())
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        eunit_project::run(Self::project(), ctx, &self.ledger)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = Bbmustache::new();
        assert_eq!(w.id(), "bbmustache");
        assert_eq!(w.category(), Category::Compute);
        assert_eq!(w.runtimes(), Runtimes::Both);
        assert_eq!(w.contracts(), &["A.1", "A.7"]);
        // A.9 is deliberately not claimed: nothing here builds or boots the runtime.
        assert!(!w.contracts().contains(&"A.9"));
        assert!(!w.description().is_empty());
    }

    /// The workload's identity must come from the project table, so the two cannot drift.
    #[test]
    fn the_project_table_is_the_single_source_of_identity() {
        let w = Bbmustache::new();
        assert_eq!(w.id(), eunit_project::BBMUSTACHE.id);
        assert_eq!(w.description(), eunit_project::BBMUSTACHE.description);
        assert_eq!(w.category(), eunit_project::BBMUSTACHE.category);
        assert_eq!(eunit_project::BBMUSTACHE.expected_tests, 79);
    }

    /// A missing checkout (and a missing `erlc`) is an honest skip, never a fabricated run.
    #[test]
    fn probe_never_reports_ready_without_its_prerequisites() {
        let p = Bbmustache::new().probe();
        if !eunit_project::repo(&eunit_project::BBMUSTACHE).is_dir() {
            assert_eq!(p.reason(), Some("repo-missing"));
            assert!(!p.is_ready());
        }
    }
}
