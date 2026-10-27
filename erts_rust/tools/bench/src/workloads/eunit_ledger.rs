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

//! The cross-side **test-count agreement record** shared by the `eunit`-suite workloads.
//!
//! The runner builds the workload boxes **once** and iterates the runtimes over them, so a
//! box sees both sides of a run. [`SideLedger`] is how "both sides report the same test
//! count" is *enforced* rather than hoped for: the first side records what its suite
//! reported, and the second side is compared with it — a disagreement fails the later side
//! (naming both counts and both summary lines), so a runtime that silently skips or
//! double-counts tests cannot pass as agreement.
//!
//! The ledger lives in the workload instance (`SharedLedger`), never in a process global, so
//! two workloads — or two runs — can never observe each other's counts. A same-side re-run
//! replaces that side's entry and never compares with itself, which is why a retry (or a
//! binary-vs-binary A/B of one runtime) can neither deadlock nor report a false mismatch.
//!
//! Everything here is pure `std` (a `Vec` behind a mutex) and `unsafe`-free.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::runtime::RuntimeKind;

/// One side's observation of the suite, recorded so the other side can be compared to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SideObservation {
    /// Which runtime produced it.
    pub kind: RuntimeKind,
    /// The verified test count.
    pub count: usize,
    /// The summary line it printed.
    pub summary: String,
}

/// The cross-side comparison's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LedgerVerdict {
    /// This side matches the other side (or is the first side recorded).
    Agrees,
    /// This side disagrees with the other side; the caller fails *this* (the later) side.
    Disagrees {
        /// This side's observation.
        this: SideObservation,
        /// The other side's observation.
        other: SideObservation,
    },
}

/// The per-workload record of what each runtime's suite run reported.
///
/// The runner builds the workload boxes **once** and iterates the runtimes over them, so a
/// box sees both sides. The ledger compares the second side's count with the first's —
/// required because "both sides report the same count" is the property this workload
/// exists to measure, and it must be enforced rather than hoped for.
///
/// A re-run of the **same** side replaces that side's entry and never compares with
/// itself, so re-running one side (a retry, an A/B of the same runtime) can neither
/// deadlock nor report a false disagreement.
#[derive(Debug, Default)]
pub(crate) struct SideLedger {
    sides: Vec<SideObservation>,
}

impl SideLedger {
    /// An empty ledger.
    pub(crate) fn new() -> SideLedger {
        SideLedger::default()
    }

    /// Records `observation`, comparing it with the *other* side when one is already there.
    pub(crate) fn record(&mut self, observation: SideObservation) -> LedgerVerdict {
        if let Some(slot) = self.sides.iter_mut().find(|side| side.kind == observation.kind) {
            *slot = observation;
            return LedgerVerdict::Agrees;
        }
        let verdict = match self.sides.first() {
            Some(other) if other.count != observation.count => LedgerVerdict::Disagrees {
                this: observation.clone(),
                other: other.clone(),
            },
            _ => LedgerVerdict::Agrees,
        };
        self.sides.push(observation);
        verdict
    }

    /// The sides recorded so far, oldest first (used by the machinery's own tests).
    #[cfg(test)]
    pub(crate) fn sides(&self) -> &[SideObservation] {
        &self.sides
    }
}

/// The ledger a workload instance carries across the runtimes of one run.
pub(crate) type SharedLedger = Arc<Mutex<SideLedger>>;

/// Creates an empty shared ledger.
pub(crate) fn shared_ledger() -> SharedLedger {
    Arc::new(Mutex::new(SideLedger::new()))
}

/// Locks the ledger, recovering from poisoning.
///
/// Poisoning only means another thread panicked while holding the lock; the ledger's own
/// value is still a complete record (a single `Vec` push or replace), so recovering it
/// keeps `run` panic-free without hiding any measurement error.
pub(crate) fn lock(ledger: &Mutex<SideLedger>) -> MutexGuard<'_, SideLedger> {
    match ledger.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_side_ledger_agrees_and_fails_the_later_side_on_a_mismatch() {
        let observation = |kind, count: usize, summary: &str| SideObservation {
            kind,
            count,
            summary: summary.to_string(),
        };

        // First side records, second agrees.
        let mut ledger = SideLedger::new();
        assert_eq!(
            ledger.record(observation(RuntimeKind::Sys, 90, "All 90 tests passed.")),
            LedgerVerdict::Agrees
        );
        assert_eq!(
            ledger.record(observation(RuntimeKind::Rust, 90, "All 90 tests passed.")),
            LedgerVerdict::Agrees
        );
        assert_eq!(ledger.sides().len(), 2);

        // A mismatch fails the *later* side, naming both rows.
        let mut ledger = SideLedger::new();
        ledger.record(observation(RuntimeKind::Sys, 90, "All 90 tests passed."));
        match ledger.record(observation(RuntimeKind::Rust, 89, "All 89 tests passed.")) {
            LedgerVerdict::Disagrees { this, other } => {
                assert_eq!(this.kind, RuntimeKind::Rust);
                assert_eq!(this.count, 89);
                assert_eq!(other.kind, RuntimeKind::Sys);
                assert_eq!(other.count, 90);
            }
            verdict => panic!("expected a disagreement, got {verdict:?}"),
        }

        // A same-side re-run replaces its own entry and never self-compares.
        let mut ledger = SideLedger::new();
        assert_eq!(
            ledger.record(observation(RuntimeKind::Rust, 90, "All 90 tests passed.")),
            LedgerVerdict::Agrees
        );
        assert_eq!(
            ledger.record(observation(RuntimeKind::Rust, 90, "All 90 tests passed.")),
            LedgerVerdict::Agrees
        );
        assert_eq!(ledger.sides().len(), 1);
        // ...and after a re-run with a different count, the *other* side is still compared.
        assert_eq!(
            ledger.record(observation(RuntimeKind::Rust, 12, "All 12 tests passed.")),
            LedgerVerdict::Agrees
        );
        assert!(matches!(
            ledger.record(observation(RuntimeKind::Sys, 90, "All 90 tests passed.")),
            LedgerVerdict::Disagrees { .. }
        ));
    }
}
