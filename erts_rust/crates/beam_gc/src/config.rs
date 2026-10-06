//! GC-strategy configuration: [`GcConfig`] selects the mature-generation strategy on
//! top of the heap-size [`HeapPolicy`].
//!
//! **The default is [`MatureKind::Copying`]** — the existing barrier-free Cheney
//! copying mature space, i.e. exactly today's behaviour. [`MatureKind::Immix`] names
//! the measurement-gated **GenImmix** mark-region mature generation
//! (`../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D); it is an *option*, **OFF by default**,
//! and per §D/§H it may only be adopted once measurements justify it. This module
//! therefore changes no behaviour by itself: it is a purely additive seam that lets a
//! caller state which mature strategy it wants.
//!
//! Scope: safe Rust, no term-layout knowledge, no dependency on the collector — only
//! [`crate::policy::HeapPolicy`]. Nothing here is used by [`crate::Collector`] yet;
//! the copying path remains the live one.

use crate::policy::HeapPolicy;

/// Which mature-generation strategy a [`GcConfig`] selects.
///
/// The default is [`MatureKind::Copying`] (implemented explicitly, not via variant
/// order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatureKind {
    /// The **existing** barrier-free Cheney copying mature space: a minor collection
    /// copies every live young term straight into `mature`, a major (fullsweep)
    /// evacuates young + mature into one freshly-sized space. This is the current —
    /// and default — behaviour of [`crate::Collector`].
    Copying,
    /// The **measurement-gated** GenImmix mark-region mature generation
    /// (`../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D). Rejected from the critical path:
    /// OFF by default, and only ever selectable once measurement justifies it (§H).
    Immix,
}

impl Default for MatureKind {
    /// The mature-generation strategy used when none is requested: [`Self::Copying`].
    fn default() -> Self {
        MatureKind::Copying
    }
}

/// The GC strategy: the heap-size [`HeapPolicy`] plus the [`MatureKind`] to use for
/// the mature generation.
///
/// Defaults to `HeapPolicy::default()` + [`MatureKind::Copying`], so a
/// `GcConfig::default()` reproduces today's collector exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcConfig {
    /// The heap-size policy (growth/shrink bounds, fullsweep cadence, initial sizes).
    pub policy: HeapPolicy,
    /// The mature-generation strategy. Copying unless explicitly requested otherwise.
    pub mature: MatureKind,
}

impl GcConfig {
    /// A configuration using `policy` with the default (copying) mature generation.
    ///
    /// Same as [`GcConfig::copying`].
    pub fn new(policy: HeapPolicy) -> GcConfig {
        GcConfig::copying(policy)
    }

    /// A configuration using `policy` with the **copying** mature generation (the
    /// default, current behaviour).
    pub fn copying(policy: HeapPolicy) -> GcConfig {
        GcConfig {
            policy,
            mature: MatureKind::Copying,
        }
    }

    /// A configuration using `policy` with the **GenImmix** mature generation
    /// (measurement-gated; not on the default path).
    pub fn immix(policy: HeapPolicy) -> GcConfig {
        GcConfig {
            policy,
            mature: MatureKind::Immix,
        }
    }

    /// Overrides the mature-generation strategy.
    pub fn with_mature(self, kind: MatureKind) -> GcConfig {
        GcConfig {
            policy: self.policy,
            mature: kind,
        }
    }

    /// The configured heap-size policy.
    pub fn policy(&self) -> &HeapPolicy {
        &self.policy
    }

    /// The configured mature-generation strategy.
    pub fn mature(&self) -> MatureKind {
        self.mature
    }
}

impl Default for GcConfig {
    /// The default configuration: `HeapPolicy::default()` + [`MatureKind::Copying`].
    fn default() -> Self {
        GcConfig::copying(HeapPolicy::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mature_kind_default_is_copying_explicitly() {
        // An explicit `Default` impl, not a variant-order accident: `Immix` is declared
        // second but must never become the default.
        assert_eq!(MatureKind::default(), MatureKind::Copying);
        assert_ne!(MatureKind::default(), MatureKind::Immix);
        assert_eq!(MatureKind::Immix as u8, 1);
    }

    #[test]
    fn gc_config_default_is_copying_with_the_default_policy() {
        let config = GcConfig::default();
        assert_eq!(config.mature(), MatureKind::Copying);
        assert_eq!(config.policy(), &HeapPolicy::default());
        assert_eq!(config, GcConfig::new(HeapPolicy::default()));
    }

    #[test]
    fn new_and_copying_agree_and_select_copying() {
        let policy = HeapPolicy::from_min_max(233, 100_000);
        let new = GcConfig::new(policy.clone());
        let copying = GcConfig::copying(policy.clone());
        assert_eq!(new, copying);
        assert_eq!(new.mature(), MatureKind::Copying);
        assert_eq!(copying.mature, MatureKind::Copying);
        assert_eq!(new.policy, policy);
    }

    #[test]
    fn immix_and_with_mature_select_immix() {
        let policy = HeapPolicy::from_min_max(100, 1_000);
        let immix = GcConfig::immix(policy.clone());
        assert_eq!(immix.mature(), MatureKind::Immix);
        assert_eq!(immix.mature, MatureKind::Immix);

        // The builder converts either starting point, and only the strategy changes.
        let upgraded = GcConfig::copying(policy.clone()).with_mature(MatureKind::Immix);
        assert_eq!(upgraded, immix);
        let downgraded = immix.clone().with_mature(MatureKind::Copying);
        assert_eq!(downgraded, GcConfig::copying(policy.clone()));
        assert_eq!(downgraded.policy, policy);
    }

    #[test]
    fn policy_round_trips_through_the_config() {
        let policy = HeapPolicy {
            min_heap_words: 12,
            max_heap_words: 4_096,
            fullsweep_after: 7,
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
        mutated.mature = MatureKind::Copying;
        assert_ne!(config, mutated);
        assert_eq!(config.mature, MatureKind::Immix);
        assert_eq!(mutated.mature, MatureKind::Copying);

        // Differing only in the strategy is still an inequality.
        assert_ne!(
            GcConfig::copying(HeapPolicy::default()),
            GcConfig::immix(HeapPolicy::default())
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
    }
}
