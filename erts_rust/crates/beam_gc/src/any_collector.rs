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

//! [`AnyCollector`] — the strategy-dispatching collector seam.
//!
//! A thin wrapper that lets a caller (and a test) drive the **same** corpus through either
//! mature-generation strategy behind one API: [`crate::Collector`] for
//! [`MatureKind::Copying`](crate::config::MatureKind::Copying) and [`GenImmixCollector`] for
//! [`MatureKind::Immix`](crate::config::MatureKind::Immix). [`AnyCollector::from_config`] is
//! the dispatcher, and it dispatches on the configuration's **resolved** choice: an
//! explicit pin, or the measurement-derived automatic rule of [`crate::selection`] (the
//! default). The remaining methods forward to whichever strategy was selected.
//!
//! This module owns no collection logic — it exists so the two strategies stay
//! interchangeable without either depending on the other.

use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::collector::{Collection, Collector};
use crate::config::{GcConfig, MatureKind};
use crate::heap_info::HeapObservable;
use crate::immix_collector::GenImmixCollector;
use crate::policy::HeapPolicy;
use crate::selection::MatureChoice;
use crate::space::HeapSpace;
use crate::stats::GcStats;
/// A collector whose mature-generation strategy is chosen at construction time.
///
/// Both variants share the [`Collection`]/[`crate::CollectionKind`] result type and the
/// young-nursery accessors, so a caller that only needs "collect and inspect the young
/// space" is strategy-agnostic.
pub enum AnyCollector {
    /// The copying mature space ([`crate::Collector`]).
    Copying(Collector),
    /// The mark-region mature space ([`GenImmixCollector`]).
    Immix(GenImmixCollector),
}

impl AnyCollector {
    /// Builds the collector `config` **resolves to**: [`GcConfig::mature`] picks
    /// [`MatureKind::Copying`] → [`Collector`], [`MatureKind::Immix`] →
    /// [`GenImmixCollector`]. That resolution is the caller's explicit pin when one was set
    /// and otherwise the automatic rule's verdict on the configured mature generation
    /// ([`crate::selection`]).
    pub fn from_config(config: &GcConfig) -> Result<AnyCollector> {
        match config.mature() {
            MatureKind::Copying => Ok(AnyCollector::Copying(Collector::new(config.policy().clone())?)),
            MatureKind::Immix => Ok(AnyCollector::Immix(GenImmixCollector::with_config(config)?)),
        }
    }

    /// The `MatureKind` this collector was built for.
    pub fn kind(&self) -> MatureKind {
        match self {
            AnyCollector::Copying(_) => MatureKind::Copying,
            AnyCollector::Immix(_) => MatureKind::Immix,
        }
    }

    /// The resolved mature-generation choice this collector runs, for inspection: the
    /// strategy, whether it came from the automatic rule or an explicit override, and the
    /// sizes the decision used.
    ///
    /// The `source` is recovered by comparing the resolved strategy with the rule's verdict
    /// on the mature generation — see [`crate::selection::choice_for`]. Use
    /// [`GcConfig::mature_choice`] when the exact source matters.
    pub fn mature_choice(&self) -> MatureChoice {
        crate::selection::choice_for(self.kind(), self.policy().initial_mature_words)
    }
    /// The collector's heap-size policy.
    pub fn policy(&self) -> &HeapPolicy {
        match self {
            AnyCollector::Copying(c) => c.policy(),
            AnyCollector::Immix(c) => c.policy(),
        }
    }

    /// The young (nursery) space.
    pub fn young(&self) -> &HeapSpace {
        match self {
            AnyCollector::Copying(c) => c.young(),
            AnyCollector::Immix(c) => c.young(),
        }
    }

    /// The young (nursery) space, mutably — where the mutator allocates.
    pub fn young_mut(&mut self) -> &mut HeapSpace {
        match self {
            AnyCollector::Copying(c) => c.young_mut(),
            AnyCollector::Immix(c) => c.young_mut(),
        }
    }

    /// The collector's accumulated statistics.
    pub fn stats(&self) -> GcStats {
        match self {
            AnyCollector::Copying(c) => c.stats(),
            AnyCollector::Immix(c) => c.stats(),
        }
    }

    /// The number of minor collections run since the last fullsweep (the C `GEN_GCS`),
    /// forwarded from whichever strategy was selected.
    pub fn minor_gcs(&self) -> u32 {
        match self {
            AnyCollector::Copying(c) => c.minor_gcs(),
            AnyCollector::Immix(c) => c.minor_gcs(),
        }
    }

    /// The `garbage_collection` observables of the selected strategy.
    pub fn heap_info(&self) -> crate::heap_info::HeapInfo {
        crate::heap_info::HeapInfo::of(self)
    }

    /// Runs a collection on the selected strategy.
    pub fn collect(&mut self, roots: &mut [Eterm]) -> Result<Collection> {
        match self {
            AnyCollector::Copying(c) => c.collect(roots),
            AnyCollector::Immix(c) => c.collect(roots),
        }
    }

    /// Forces a **minor** collection on the selected strategy (escalation is bypassed).
    pub fn collect_minor(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        match self {
            AnyCollector::Copying(c) => c.collect_minor(roots),
            AnyCollector::Immix(c) => c.collect_minor(roots),
        }
    }

    /// Forces a **major** (fullsweep) collection on the selected strategy.
    pub fn collect_major(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        match self {
            AnyCollector::Copying(c) => c.collect_major(roots),
            AnyCollector::Immix(c) => c.collect_major(roots),
        }
    }

    /// Grows the nursery **on demand** so that a `need_words`-word allocation can proceed,
    /// forwarded to whichever strategy is selected (both expose the identical seam):
    /// [`Collector::ensure_capacity`] / [`GenImmixCollector::ensure_capacity`].
    ///
    /// After it returns, `young().available_words() >= need_words`; if the policy's
    /// [`max_heap_words`](HeapPolicy::max_heap_words) makes the request unreachable it
    /// returns the typed heap-exhaustion error instead. This is the seam a runtime calls on
    /// heap exhaustion, and it is strategy-agnostic — the caller does not need to know which
    /// mature generation is in use.
    pub fn ensure_capacity(
        &mut self,
        need_words: usize,
        roots: &mut [Eterm],
    ) -> Result<Collection> {
        match self {
            AnyCollector::Copying(c) => c.ensure_capacity(need_words, roots),
            AnyCollector::Immix(c) => c.ensure_capacity(need_words, roots),
        }
    }
}

impl HeapObservable for AnyCollector {
    fn minor_gcs(&self) -> u32 {
        match self {
            AnyCollector::Copying(c) => c.minor_gcs(),
            AnyCollector::Immix(c) => c.minor_gcs(),
        }
    }

    fn heap_size_words(&self) -> usize {
        match self {
            AnyCollector::Copying(c) => c.heap_size_words(),
            AnyCollector::Immix(c) => c.heap_size_words(),
        }
    }

    fn total_heap_size_words(&self) -> usize {
        match self {
            AnyCollector::Copying(c) => c.total_heap_size_words(),
            AnyCollector::Immix(c) => c.total_heap_size_words(),
        }
    }

    fn heap_policy(&self) -> &HeapPolicy {
        self.policy()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_selects_the_copying_path() {
        let any = AnyCollector::from_config(&GcConfig::default()).expect("collector");
        assert_eq!(any.kind(), MatureKind::Copying);
        assert!(matches!(any, AnyCollector::Copying(_)));
        assert_eq!(any.policy(), &HeapPolicy::default());
        // The choice is observable on the built collector, and it names the rule.
        let choice = any.mature_choice();
        assert_eq!(choice.kind, MatureKind::Copying);
        assert_eq!(choice.source, crate::selection::MatureSource::Automatic);
        assert_eq!(choice.mature_words, 233);
    }

    #[test]
    fn immix_config_selects_the_immix_path() {
        let config = GcConfig::immix(HeapPolicy::from_min_max(12, 0));
        let any = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(any.kind(), MatureKind::Immix);
        assert!(matches!(any, AnyCollector::Immix(_)));
        assert_eq!(any.policy(), config.policy());
        // An explicit Immix pin under the break-even is reported as an override.
        assert!(any.mature_choice().is_override());
    }

    #[test]
    fn the_automatic_rule_selects_the_strategy_from_the_mature_size() {
        let big = HeapPolicy {
            initial_mature_words: crate::selection::IMMIX_BREAK_EVEN_WORDS,
            ..HeapPolicy::default()
        };
        let any = AnyCollector::from_config(&GcConfig::auto(big)).expect("collector");
        assert_eq!(any.kind(), MatureKind::Immix);
        assert!(matches!(any, AnyCollector::Immix(_)));
        assert_eq!(
            any.mature_choice().source,
            crate::selection::MatureSource::Automatic
        );
    }
}