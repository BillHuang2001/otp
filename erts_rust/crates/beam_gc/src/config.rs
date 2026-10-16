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

//! GC-strategy configuration: [`GcConfig`] selects the mature-generation strategy on
//! top of the heap-size [`HeapPolicy`], and [`MatureSelection`] says *how* it is selected —
//! by the measurement-derived automatic rule (the default) or by an explicit user override.
//!
//! **The default selection is [`MatureSelection::Auto`]**, so a `GcConfig::default()` asks
//! the automatic rule ([`crate::selection`]) to pick between the copying mature space and
//! the measurement-gated **GenImmix** mark-region mature generation
//! (`../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D) from the mature generation's size. For the
//! default policy that rule resolves to [`MatureKind::Copying`](crate::config::MatureKind)
//! (233 words, well under the break-even), so `GcConfig::default()` still reproduces today's
//! collector exactly. [`MatureSelection::Explicit`] pins a strategy regardless of size.
//!
//! Scope: safe Rust, no term-layout knowledge, no dependency on the collector — only
//! [`crate::policy::HeapPolicy`] and the rule in [`crate::selection`]. The resolved decision
//! is inspectable through [`GcConfig::mature_choice`].

use crate::policy::HeapPolicy;
use crate::selection::{resolve_mature, MatureChoice};

/// Which mature-generation strategy a configuration can select.
///
/// The default is [`MatureKind::Copying`] (implemented explicitly, not via variant
/// order). This is the *strategy*; whether it is chosen by the rule or pinned by the caller
/// is [`MatureSelection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatureKind {
    /// The **existing** barrier-free Cheney copying mature space: a minor collection
    /// copies every live young term straight into `mature`, a major (fullsweep)
    /// evacuates young + mature into one freshly-sized space. This is the current —
    /// and default — behaviour of [`crate::Collector`].
    Copying,
    /// The **measurement-gated** GenImmix mark-region mature generation
    /// (`../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D). Selected automatically only for a
    /// large, long-lived mature generation (see [`crate::selection`]); OFF by default.
    Immix,
}

impl Default for MatureKind {
    /// The mature-generation strategy used when none is requested: [`Self::Copying`].
    fn default() -> Self {
        MatureKind::Copying
    }
}

/// How a [`GcConfig`] chooses between the two [`MatureKind`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatureSelection {
    /// Let the **automatic rule** decide from the mature generation's configured size
    /// ([`crate::selection::IMMIX_BREAK_EVEN_WORDS`]). The default.
    Auto,
    /// Use exactly this strategy whatever the heap size — an explicit user override that
    /// beats the automatic rule in both directions.
    Explicit(MatureKind),
}

impl MatureSelection {
    /// An explicit selection of `kind`.
    pub fn explicit(kind: MatureKind) -> MatureSelection {
        MatureSelection::Explicit(kind)
    }
}

impl Default for MatureSelection {
    /// The selection used when none is requested: the automatic rule ([`Self::Auto`]).
    fn default() -> Self {
        MatureSelection::Auto
    }
}

/// The GC strategy: the heap-size [`HeapPolicy`] plus the [`MatureSelection`] that decides
/// the mature-generation strategy.
///
/// The default is `HeapPolicy::default()` + [`MatureSelection::Auto`], which resolves to
/// [`MatureKind::Copying`] for that policy — i.e. a `GcConfig::default()` reproduces today's
/// collector exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcConfig {
    /// The heap-size policy (growth/shrink bounds, fullsweep cadence, initial sizes).
    pub policy: HeapPolicy,
    /// How the mature-generation strategy is chosen: the automatic rule (`Auto`, the
    /// default) or an explicit pin (`Explicit`).
    pub selection: MatureSelection,
}

impl GcConfig {
    /// A configuration using `policy` with the **default selection** — the automatic rule.
    ///
    /// Same as [`GcConfig::auto`].
    pub fn new(policy: HeapPolicy) -> GcConfig {
        GcConfig::auto(policy)
    }

    /// A configuration using `policy` with the **automatic rule** deciding the mature
    /// strategy from the mature generation's size.
    pub fn auto(policy: HeapPolicy) -> GcConfig {
        GcConfig {
            policy,
            selection: MatureSelection::Auto,
        }
    }

    /// A configuration for **unbounded on-demand growth**: the automatic selection on top of
    /// an unbounded-growth policy ([`HeapPolicy::unbounded`], i.e. `max_heap_words == 0`).
    ///
    /// Use this when the heap must grow as far as the live set requires with no upper bound,
    /// and the caller drives that growth through
    /// [`Collector::ensure_capacity`](crate::Collector::ensure_capacity) /
    /// [`AnyCollector::ensure_capacity`](crate::AnyCollector::ensure_capacity) on heap
    /// exhaustion. It resolves to the same (copying) strategy as
    /// [`GcConfig::default`], because `max_heap_words == 0` is already the default policy's
    /// growth dimension.
    pub fn growable() -> GcConfig {
        GcConfig::auto(HeapPolicy::unbounded())
    }

    /// A configuration using `policy` with the **copying** mature generation **pinned**
    /// (an explicit override of the automatic rule).
    pub fn copying(policy: HeapPolicy) -> GcConfig {
        GcConfig {
            policy,
            selection: MatureSelection::Explicit(MatureKind::Copying),
        }
    }

    /// A configuration using `policy` with the **GenImmix** mature generation **pinned**
    /// (an explicit override of the automatic rule).
    pub fn immix(policy: HeapPolicy) -> GcConfig {
        GcConfig {
            policy,
            selection: MatureSelection::Explicit(MatureKind::Immix),
        }
    }

    /// Overrides how the mature-generation strategy is chosen.
    pub fn with_selection(self, selection: MatureSelection) -> GcConfig {
        GcConfig {
            policy: self.policy,
            selection,
        }
    }

    /// Pins the mature-generation strategy, overriding the automatic rule.
    pub fn with_mature(self, kind: MatureKind) -> GcConfig {
        self.with_selection(MatureSelection::Explicit(kind))
    }

    /// The configured heap-size policy.
    pub fn policy(&self) -> &HeapPolicy {
        &self.policy
    }

    /// How the mature-generation strategy is chosen (the raw setting).
    pub fn selection(&self) -> MatureSelection {
        self.selection
    }

    /// The strategy this configuration **resolves to** — the caller's explicit pin when one
    /// was set, otherwise the automatic rule's verdict on the configured mature generation.
    pub fn mature(&self) -> MatureKind {
        self.mature_choice().kind
    }

    /// The full resolved choice: the strategy, whether the rule or the caller chose it, the
    /// mature generation's size and the rule's break-even. The inspectable form of the
    /// decision.
    pub fn mature_choice(&self) -> MatureChoice {
        resolve_mature(self)
    }
}

impl Default for GcConfig {
    /// The default configuration: `HeapPolicy::default()` + the automatic selection, which
    /// resolves to the copying path for that policy.
    fn default() -> GcConfig {
        GcConfig::new(HeapPolicy::default())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{MatureSource, IMMIX_BREAK_EVEN_WORDS};

    #[test]
    fn mature_kind_default_is_copying_explicitly() {
        // An explicit `Default` impl, not a variant-order accident: `Immix` is declared
        // second but must never become the default.
        assert_eq!(MatureKind::default(), MatureKind::Copying);
        assert_ne!(MatureKind::default(), MatureKind::Immix);
        assert_eq!(MatureKind::Immix as u8, 1);
    }

    #[test]
    fn mature_selection_defaults_to_the_automatic_rule() {
        assert_eq!(MatureSelection::default(), MatureSelection::Auto);
        assert_ne!(
            MatureSelection::default(),
            MatureSelection::Explicit(MatureKind::Copying)
        );
    }

    #[test]
    fn gc_config_default_is_automatic_and_resolves_to_copying() {
        let config = GcConfig::default();
        // The *selection* is the automatic rule ...
        assert_eq!(config.selection(), MatureSelection::Auto);
        assert_eq!(
            config.mature_choice().source,
            MatureSource::Automatic,
            "the default asks the rule"
        );
        // ... which picks the copying path for the default (233-word) mature generation.
        assert_eq!(config.mature(), MatureKind::Copying);
        assert_eq!(config.policy(), &HeapPolicy::default());
        assert_eq!(config, GcConfig::new(HeapPolicy::default()));
    }

    #[test]
    fn new_is_auto_and_copying_pins_copying() {
        let policy = HeapPolicy::from_min_max(233, 100_000);
        let new = GcConfig::new(policy.clone());
        let auto = GcConfig::auto(policy.clone());
        let copying = GcConfig::copying(policy.clone());
        assert_eq!(new, auto, "`new` is the automatic selection");
        assert_ne!(new, copying, "`copying` pins the strategy");
        assert_eq!(new.mature(), MatureKind::Copying);
        assert_eq!(copying.mature(), MatureKind::Copying);
        assert_eq!(new.selection(), MatureSelection::Auto);
        assert_eq!(
            copying.selection(),
            MatureSelection::Explicit(MatureKind::Copying)
        );
        assert_eq!(new.policy, policy);
    }

    #[test]
    fn immix_and_with_mature_select_immix() {
        let policy = HeapPolicy::from_min_max(100, 1_000);
        let immix = GcConfig::immix(policy.clone());
        assert_eq!(immix.mature(), MatureKind::Immix);
        assert_eq!(
            immix.selection(),
            MatureSelection::Explicit(MatureKind::Immix)
        );

        // The builder converts either starting point; only the selection changes.
        let upgraded = GcConfig::copying(policy.clone()).with_mature(MatureKind::Immix);
        assert_eq!(upgraded, immix);
        let downgraded = immix.clone().with_mature(MatureKind::Copying);
        assert_eq!(downgraded, GcConfig::copying(policy.clone()));
        assert_eq!(downgraded.policy, policy);
    }

    #[test]
    fn with_selection_overrides_the_automatic_rule() {
        let big = HeapPolicy {
            initial_mature_words: IMMIX_BREAK_EVEN_WORDS * 8,
            ..HeapPolicy::default()
        };
        // The rule would pick Immix; pinning Copying must win, and be reported as an override.
        let pinned =
            GcConfig::auto(big).with_selection(MatureSelection::Explicit(MatureKind::Copying));
        assert_eq!(pinned.mature(), MatureKind::Copying);
        assert!(pinned.mature_choice().is_override());
        // Going back to automatic restores the rule's verdict.
        let auto = pinned.with_selection(MatureSelection::Auto);
        assert_eq!(auto.mature(), MatureKind::Immix);
        assert!(!auto.mature_choice().is_override());
    }

    #[test]
    fn policy_round_trips_through_the_config() {
        let policy = HeapPolicy {
            min_heap_words: 12,
            max_heap_words: 4_096,
            fullsweep_after: 7,
            min_bin_vheap_words: 46_422,
            initial_young_words: 51,
            initial_mature_words: 90,
            need_reserve_words: 128,
        };
        let config = GcConfig::immix(policy.clone());
        assert_eq!(config.policy(), &policy);
        // Field by field, so a future field reordering is caught.
        assert_eq!(config.policy.min_heap_words, 12);
        assert_eq!(config.policy.max_heap_words, 4_096);
        assert_eq!(config.policy.fullsweep_after, 7);
        assert_eq!(config.policy.min_bin_vheap_words, 46_422);
        assert_eq!(config.policy.initial_young_words, 51);
        assert_eq!(config.policy.initial_mature_words, 90);
        assert_eq!(config.policy.need_reserve_words, 128);
    }

    #[test]
    fn clone_and_partial_eq_behave() {
        let config = GcConfig::immix(HeapPolicy::from_min_max(51, 0));
        let copy = config.clone();
        assert_eq!(config, copy);
        // The clone is independent: mutating it does not touch the original.
        let mut mutated = copy;
        mutated.selection = MatureSelection::Explicit(MatureKind::Copying);
        assert_ne!(config, mutated);
        assert_eq!(config.mature(), MatureKind::Immix);
        assert_eq!(mutated.mature(), MatureKind::Copying);

        // Differing only in the strategy is still an inequality.
        assert_ne!(
            GcConfig::copying(HeapPolicy::default()),
            GcConfig::immix(HeapPolicy::default())
        );
        // The automatic selection differs from either pin, at the same policy.
        assert_ne!(
            GcConfig::auto(HeapPolicy::default()),
            GcConfig::copying(HeapPolicy::default())
        );
        // As is differing only in the policy.
        assert_ne!(
            GcConfig::copying(HeapPolicy::from_min_max(12, 0)),
            GcConfig::copying(HeapPolicy::from_min_max(38, 0))
        );
    }

    #[test]
    fn mature_kind_derives_copy_and_eq() {
        let kind = MatureKind::Immix;
        assert_eq!(kind, kind);
        assert_eq!(kind.clone(), kind);
        assert_eq!(format!("{kind:?}"), "Immix");
        assert_eq!(format!("{:?}", MatureKind::Copying), "Copying");
        assert_eq!(format!("{:?}", MatureKind::default()), "Copying");
    }
}
