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

//! Automatic mature-generation selection: the **measurement-derived rule** that chooses
//! between the copying and mark-region mature spaces, plus the resolved-choice type that
//! makes the decision observable.
//!
//! # Why a rule at all
//!
//! [`MatureKind::Immix`](crate::config::MatureKind::Immix) is not a drop-in default: as a
//! stop-the-world space it moves less but pauses longer (see DESIGN below). The crate's
//! decision — restated here so this module is self-contained — is that the mark-region
//! mature generation is worth selecting only for a **large, long-lived mature
//! generation**, and that everything else keeps the copying space. This module encodes that
//! decision as an explicit, inspectable rule instead of leaving `Immix` unreachable.
//!
//! # DESIGN — the measured break-even
//!
//! `tests/immix_break_even.rs` drives the *same* purely long-lived corpus (every object a
//! root, so the mark-region path's `AllMarked` pinning covers every whole block while the
//! copying path re-copies the whole live set) from a quarter of an Immix block up to 64
//! blocks, through both strategies, and reports the forced fullsweep's moved words, its
//! resident footprint and its pause. The measured shape of the trade (fastest of 9 runs,
//! which is stable across repetitions; the *counts* are exact):
//!
//! | live set | moved words (copying → immix) | footprint (copying → immix) | pause (immix/copying) | words saved per extra µs |
//! |---|---|---|---|---|
//! | 0.25 block | 64 → 64 (line-sized objects) | 1.0× | 2.0× | 0 |
//! | 0.5 block | 128 → 128 | 1.0× | 2.3× | 0 |
//! | 1 block | 256 → **0** | 1.0× | 1.2× | 264–370 |
//! | 2 blocks | 512 → **0** | 1.0× | 1.4× | 219–249 |
//! | 8 blocks | 2 048 → **0** | 1.0× | 2.3× | 265–274 |
//! | 32 blocks | 8 192 → **0** | 1.0× | 2.3× | 279–284 |
//! | 64 blocks | 16 384 → **0** | 1.0× | 2.3× | 281–284 |
//!
//! The same sweep over 3-word objects (one whole line for 3 useful words, i.e. the
//! mark-region space's worst case) shows the lower end more starkly: at 0.25 and 0.5 block
//! the mark-region path pins **nothing** and evacuates line-rounded copies — it moves
//! **5.3× more** words, occupies **5.3×** the footprint (0.19×) and pauses **2.0–2.7×**
//! longer, i.e. it is strictly dominated below one whole block; from one block up it pins the
//! whole live set (moved words → 0) at a 0.19× footprint, and its "words saved per extra µs
//! of pause" ratio climbs 69→75→85 at 1→2→8 blocks and then settles at 80–87 (plateau from
//! 8 blocks).
//!
//! Reading the two shapes together:
//!
//! * **Below one whole block the mark-region path is never better, and for small objects it
//!   is strictly dominated** — it has no full block to pin, so it moves 5.3× *more* words
//!   (line rounding), occupies 5.3× the footprint and pauses 2.0–2.7× longer.
//! * **From one block up it pins the whole live set** (moved words → 0) **but its relative
//!   cost is flat** (pause ≈ 2.2–2.3× in the selected regime, footprint 1.0×–5.3× depending
//!   on object size): there is no further size at which it becomes a *time* win.
//! * The worst-case (small-object) benefit/cost ratio — avoided copy words per extra µs of
//!   pause — **plateaus at 8 blocks**: 69→75→85 at 1→2→8 blocks, then flat (80–87 out to 64
//!   blocks). That is the point past which extra size buys no further ratio; it also sits 8×
//!   above the strict-domination regime and matches the earlier three-shape measurement in
//!   `tests/immix_workloads.rs` ("roughly ≥ 8 full blocks").
//!
//! The rule therefore switches at that measured plateau: [`IMMIX_BREAK_EVEN_WORDS`] =
//! **8 whole Immix blocks = 2 048 words** of mature generation. Below it the copying space
//! is kept — it is measured to be at least as good on every axis, and strictly better below
//! one block — and the default policy (233 words, *under one block*) therefore keeps
//! selecting the copying path, i.e. the rule changes nothing for a default-configured
//! collector.
//!
//! **Honest caveat.** The mark-region path's *pause* is worse in the regime where it is
//! selected (≈ 2.2–2.3×), so the rule trades latency for copy work; it is only worthwhile
//! where the copy volume dominates (a large, long-lived mature generation). The break-even is
//! a *copy-work* break-even, not a wall-clock one.
//! [`MatureSelection::Explicit`](crate::config::MatureSelection::Explicit) lets a caller opt
//! either way regardless of size.
//!
//! # The rule's input
//!
//! The rule reads the mature generation's **configured size** — the
//! [`HeapPolicy::initial_mature_words`](crate::policy::HeapPolicy::initial_mature_words) the
//! collector is constructed with. A collector built with an auto-selecting configuration
//! then keeps the strategy it was built with for its whole life: a heap that *grows* past
//! the break-even later is **not** re-evaluated (re-selecting would mean rebuilding the
//! mature space mid-life). A caller that knows its mature generation will grow large can pin
//! the strategy up front with [`GcConfig::immix`](crate::config::GcConfig::immix).

use crate::config::{GcConfig, MatureKind, MatureSelection};
use crate::immix::BLOCK_WORDS;
use crate::policy::HeapPolicy;

/// The automatic rule's break-even, in mature-generation words: **8 whole Immix blocks
/// (2 048 words)**. At or above it the rule selects
/// [`MatureKind::Immix`](crate::config::MatureKind::Immix); below it the copying path.
///
/// Derived from `tests/immix_break_even.rs` — see the module docs for the measured table
/// and the reasoning (the point where the mark-region path's avoided-copy-work per extra µs
/// of pause plateaus).
pub const IMMIX_BREAK_EVEN_WORDS: usize = 8 * BLOCK_WORDS;

/// [`IMMIX_BREAK_EVEN_WORDS`] expressed in whole Immix blocks, for reporting.
pub const IMMIX_BREAK_EVEN_BLOCKS: usize = IMMIX_BREAK_EVEN_WORDS / BLOCK_WORDS;

/// The automatic rule: [`MatureKind::Immix`](crate::config::MatureKind::Immix) when the
/// mature generation holds at least [`IMMIX_BREAK_EVEN_WORDS`] words, the copying path
/// otherwise.
///
/// A pure function of the mature generation's size, so it is trivially inspectable and
/// testable.
pub fn select_mature(mature_words: usize) -> MatureKind {
    if mature_words >= IMMIX_BREAK_EVEN_WORDS {
        MatureKind::Immix
    } else {
        MatureKind::Copying
    }
}

/// The mature generation's *configured* size, in words, that the automatic rule reads from a
/// policy — the mature capacity the collector is constructed with.
pub fn mature_words_of(policy: &HeapPolicy) -> usize {
    policy.initial_mature_words
}

/// Why a mature-generation strategy was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatureSource {
    /// The automatic rule decided from the mature generation's size.
    Automatic,
    /// The caller pinned the strategy, whatever the size.
    Explicit,
}

/// The **resolved** mature-generation choice: which strategy is used, why, and the sizes the
/// decision used. Produced by [`GcConfig::mature_choice`](crate::config::GcConfig::mature_choice)
/// (exact) and by [`AnyCollector::mature_choice`](crate::any_collector::AnyCollector::mature_choice)
/// (derived from the live collector).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatureChoice {
    /// The strategy that is used.
    pub kind: MatureKind,
    /// Whether the automatic rule chose it or the caller pinned it.
    pub source: MatureSource,
    /// The mature generation's configured size, in words — the rule's input.
    pub mature_words: usize,
    /// The rule's break-even, in words ([`IMMIX_BREAK_EVEN_WORDS`]).
    pub break_even_words: usize,
}

impl MatureChoice {
    /// The strategy the automatic rule would pick at [`Self::mature_words`].
    pub fn rule_kind(&self) -> MatureKind {
        select_mature(self.mature_words)
    }

    /// True when the caller's explicit choice **differs** from what the automatic rule would
    /// have picked — i.e. an actual override of the rule.
    pub fn is_override(&self) -> bool {
        self.source == MatureSource::Explicit && self.kind != self.rule_kind()
    }

    /// A one-line human-readable rendering of the decision, for logs / `system_info`-style
    /// inspection.
    pub fn describe(&self) -> String {
        match self.source {
            MatureSource::Automatic => format!(
                "{:?} (automatic rule: mature generation {} words {} break-even {} words)",
                self.kind,
                self.mature_words,
                if self.kind == MatureKind::Immix { ">=" } else { "<" },
                self.break_even_words
            ),
            MatureSource::Explicit if self.kind == self.rule_kind() => format!(
                "{:?} (explicit; the automatic rule agrees at {} words)",
                self.kind, self.mature_words
            ),
            MatureSource::Explicit => format!(
                "{:?} (explicit override; the automatic rule would pick {:?} at {} words)",
                self.kind,
                self.rule_kind(),
                self.mature_words
            ),
        }
    }
}

/// The resolved choice for a configuration: the caller's explicit strategy when one was
/// pinned, otherwise the automatic rule's verdict on the configured mature generation.
pub fn resolve_mature(config: &GcConfig) -> MatureChoice {
    let mature_words = mature_words_of(config.policy());
    match config.selection() {
        MatureSelection::Explicit(kind) => MatureChoice {
            kind,
            source: MatureSource::Explicit,
            mature_words,
            break_even_words: IMMIX_BREAK_EVEN_WORDS,
        },
        MatureSelection::Auto => MatureChoice {
            kind: select_mature(mature_words),
            source: MatureSource::Automatic,
            mature_words,
            break_even_words: IMMIX_BREAK_EVEN_WORDS,
        },
    }
}

/// The choice a live collector with resolved strategy `kind` and mature generation
/// `mature_words` reports.
///
/// A collector no longer remembers whether its strategy was pinned or derived, so the
/// `source` is recovered by comparing the resolved `kind` with the rule's verdict: they
/// disagree only when the caller overrode the rule. A caller that explicitly pinned the
/// rule's *own* pick is therefore reported as [`MatureSource::Automatic`] — the resolved
/// strategy is identical either way. Use
/// [`GcConfig::mature_choice`](crate::config::GcConfig::mature_choice) when the exact source
/// matters.
pub fn choice_for(kind: MatureKind, mature_words: usize) -> MatureChoice {
    let automatic = select_mature(mature_words);
    MatureChoice {
        kind,
        source: if kind == automatic {
            MatureSource::Automatic
        } else {
            MatureSource::Explicit
        },
        mature_words,
        break_even_words: IMMIX_BREAK_EVEN_WORDS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with_mature_words(words: usize) -> HeapPolicy {
        HeapPolicy {
            initial_mature_words: words,
            ..HeapPolicy::default()
        }
    }

    #[test]
    fn break_even_is_eight_whole_blocks() {
        assert_eq!(BLOCK_WORDS, 256);
        assert_eq!(IMMIX_BREAK_EVEN_BLOCKS, 8);
        assert_eq!(IMMIX_BREAK_EVEN_WORDS, 2_048);
        // Whole blocks, so the rule's unit is the space's own allocation unit.
        assert_eq!(IMMIX_BREAK_EVEN_WORDS % BLOCK_WORDS, 0);
    }

    #[test]
    fn the_rule_selects_copying_below_the_break_even_and_immix_at_or_above() {
        assert_eq!(select_mature(0), MatureKind::Copying);
        assert_eq!(select_mature(1), MatureKind::Copying);
        assert_eq!(select_mature(BLOCK_WORDS), MatureKind::Copying);
        assert_eq!(select_mature(IMMIX_BREAK_EVEN_WORDS - 1), MatureKind::Copying);
        assert_eq!(select_mature(IMMIX_BREAK_EVEN_WORDS), MatureKind::Immix);
        assert_eq!(select_mature(IMMIX_BREAK_EVEN_WORDS + 1), MatureKind::Immix);
        assert_eq!(select_mature(usize::MAX), MatureKind::Immix);
    }

    #[test]
    fn the_default_policy_keeps_the_copying_path() {
        // 233 words is under one block, let alone eight: the rule changes nothing for a
        // default-configured collector.
        let policy = HeapPolicy::default();
        assert_eq!(mature_words_of(&policy), 233);
        assert_eq!(select_mature(mature_words_of(&policy)), MatureKind::Copying);
    }

    #[test]
    fn the_automatic_config_resolves_by_size_in_both_directions() {
        let small = GcConfig::auto(policy_with_mature_words(IMMIX_BREAK_EVEN_WORDS - 1));
        let big = GcConfig::auto(policy_with_mature_words(IMMIX_BREAK_EVEN_WORDS));
        assert_eq!(small.mature(), MatureKind::Copying);
        assert_eq!(big.mature(), MatureKind::Immix);
        assert_eq!(small.mature_choice().source, MatureSource::Automatic);
        assert_eq!(big.mature_choice().source, MatureSource::Automatic);
        assert!(!small.mature_choice().is_override());
        assert!(!big.mature_choice().is_override());
    }

    #[test]
    fn an_explicit_selection_beats_the_rule_in_both_directions() {
        let big = policy_with_mature_words(IMMIX_BREAK_EVEN_WORDS * 4);
        let small = policy_with_mature_words(10);

        let pinned_copying = GcConfig::auto(big.clone()).with_mature(MatureKind::Copying);
        assert_eq!(pinned_copying.mature(), MatureKind::Copying);
        assert_eq!(pinned_copying.mature_choice().source, MatureSource::Explicit);
        assert!(pinned_copying.mature_choice().is_override());

        let pinned_immix = GcConfig::auto(small.clone()).with_mature(MatureKind::Immix);
        assert_eq!(pinned_immix.mature(), MatureKind::Immix);
        assert_eq!(pinned_immix.mature_choice().source, MatureSource::Explicit);
        assert!(pinned_immix.mature_choice().is_override());
    }

    #[test]
    fn choice_for_recovers_the_source_from_the_resolved_strategy() {
        // A collector whose strategy differs from the rule's verdict must have been pinned.
        let overridden = choice_for(MatureKind::Immix, 10);
        assert_eq!(overridden.source, MatureSource::Explicit);
        assert!(overridden.is_override());

        // One that agrees cannot be distinguished, and is reported as automatic.
        let agreed = choice_for(MatureKind::Copying, 10);
        assert_eq!(agreed.source, MatureSource::Automatic);
        assert!(!agreed.is_override());
    }

    #[test]
    fn describe_renders_every_decision_shape() {
        let automatic_immix = resolve_mature(&GcConfig::auto(policy_with_mature_words(4_096)));
        assert_eq!(
            automatic_immix.describe(),
            "Immix (automatic rule: mature generation 4096 words >= break-even 2048 words)"
        );

        let automatic_copying = resolve_mature(&GcConfig::auto(policy_with_mature_words(233)));
        assert_eq!(
            automatic_copying.describe(),
            "Copying (automatic rule: mature generation 233 words < break-even 2048 words)"
        );

        let agreed = resolve_mature(&GcConfig::copying(policy_with_mature_words(233)));
        assert_eq!(
            agreed.describe(),
            "Copying (explicit; the automatic rule agrees at 233 words)"
        );

        let override_ = resolve_mature(&GcConfig::immix(policy_with_mature_words(233)));
        assert_eq!(
            override_.describe(),
            "Immix (explicit override; the automatic rule would pick Copying at 233 words)"
        );
    }
}
