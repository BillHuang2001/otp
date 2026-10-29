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
//! Unit tests for the frontier's **pure policy** — the entitlement, the learned demand and the
//! growth arithmetic — plus the one depth-sensitive pair (`need`/`note_demand`) over a real runtime,
//! because those two read (and write) the per-depth figures through [`Runtime::bif_heaps_taken`].
//!
//! The end-to-end half (a real `length(lists:duplicate(1 bsl 21, gurka))` loop answering like the
//! oracle) is `beam_rt/tests/bif_frontier_reservation.rs`; this module pins the arithmetic so a
//! policy change cannot silently move the reservation without a red test.
use super::{
    entitlement, grown_capacity, need, note_demand, BIF_FRONTIER_HEADROOM_DIVISOR,
    BIF_FRONTIER_MAX_WORDS,
};
use crate::config::RuntimeConfig;
use crate::runtime::Runtime;

/// A runtime whose pooled region is the default one (128 MiB of *capacity*, lazily mapped), so the
/// entitlement arithmetic is the production one.
fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::new()).expect("the default runtime builds")
}

/// **The entitlement is a quarter of the region, absolutely capped.**
///
/// The cap is what keeps a large region from reserving a quarter of itself for one call, and the
/// fractional term is what keeps a *small* (or `+hmax`-capped, or test-sized) region from demanding
/// more free words than it can physically have — the property that makes this policy safe on the
/// 1 024/2 048-word regions the pool's own tests build.
#[test]
fn the_entitlement_is_a_quarter_of_the_region_capped_in_absolute_words() {
    assert_eq!(BIF_FRONTIER_HEADROOM_DIVISOR, 4);
    for capacity in [4, 1_024, 2_048, 65_536, 1 << 20, 1 << 24] {
        assert_eq!(
            entitlement(capacity),
            (capacity / BIF_FRONTIER_HEADROOM_DIVISOR).min(BIF_FRONTIER_MAX_WORDS),
            "capacity {capacity}"
        );
    }
    // A small region never asks for more than a quarter of itself: this is the property that a
    // fixed 64 MiB floor would violate (and that would make every small-region runtime grow its
    // region on its very first call).
    for capacity in [1_024, 2_048, 65_536] {
        assert!(
            entitlement(capacity) <= capacity,
            "capacity {capacity}: the reservation must be servable by the region itself"
        );
    }
    // The cap binds exactly at four times `BIF_FRONTIER_MAX_WORDS`.
    let cap_at = BIF_FRONTIER_MAX_WORDS * BIF_FRONTIER_HEADROOM_DIVISOR;
    assert_eq!(entitlement(cap_at), BIF_FRONTIER_MAX_WORDS);
    assert_eq!(entitlement(cap_at * 4), BIF_FRONTIER_MAX_WORDS);
}

/// **Growth is twice the region it replaces, the reservation a lower bound, and `+hmax` the cap.**
///
/// The refusal arm (`None`) is what keeps a configured `+hmax` a *configured* limit: the region is
/// kept as it is, so a call that really overflows it still raises `error:system_limit` — the one
/// place that error is legitimate.
#[test]
fn growth_doubles_the_region_takes_the_reservation_and_respects_the_ceiling() {
    // The ordinary arm: double, no cap.
    assert_eq!(grown_capacity(2_048, 512, 0), Some(4_096));
    // The reservation wins when it is larger than the doubling (a single call whose demand outgrew
    // the region's own capacity by more than 2x is still served in ONE growth).
    assert_eq!(grown_capacity(1_024, 60_000, 0), Some(60_000));
    // The ceiling is `+hmax`: growth is clamped to it, and only taken when it is wider.
    assert_eq!(grown_capacity(1_024, 8_192, 4_096), Some(4_096));
    assert_eq!(grown_capacity(1_024, 8_192, 2_048), Some(2_048));
    // No legal growth: the ceiling is the region's own size (the `+hmax`-pinned pool tests), or
    // below it. The region is kept and the call keeps today's `system_limit`.
    assert_eq!(grown_capacity(1_024, 8_192, 1_024), None);
    assert_eq!(grown_capacity(2_048, 512, 1_024), None);
    // Saturation never wraps into a bogus (smaller) frontier.
    assert_eq!(grown_capacity(usize::MAX, 0, 0), None);
}

/// **The learned demand is per depth, and a nested demand never inflates the outermost one (or
/// vice versa).**
///
/// The two figures are separate because a large outermost demand (`lists:duplicate/2` building a
/// 32 MiB list) must not make every *nested* call reserve the same, which is the retention the
/// depth-aware region sizing exists to avoid.
#[test]
fn the_learned_demand_is_kept_per_depth_and_never_shrinks() {
    let mut rt = runtime();
    let capacity = rt.bif_heaps.first().expect("the pool seeds one region").capacity();
    let floor = entitlement(capacity);

    // Nothing learned yet: the reservation is the entitlement at both depths.
    rt.bif_heaps_taken = 0;
    assert_eq!(need(&rt, capacity), floor);
    rt.bif_heaps_taken = 1;
    assert_eq!(need(&rt, capacity), floor, "depth 1 is still the outermost call");
    rt.bif_heaps_taken = 2;
    assert_eq!(need(&rt, capacity), floor);

    // An outermost demand above the entitlement raises the outermost reservation only.
    let big = floor * 3;
    rt.bif_heaps_taken = 1;
    note_demand(&mut rt, big);
    assert_eq!(need(&rt, capacity), big);
    rt.bif_heaps_taken = 2;
    assert_eq!(need(&rt, capacity), floor, "a nested call keeps its own figure");

    // ... and a nested demand raises the nested one only.
    let nested = floor * 5;
    note_demand(&mut rt, nested);
    assert_eq!(need(&rt, capacity), nested);
    rt.bif_heaps_taken = 1;
    assert_eq!(need(&rt, capacity), big);

    // A smaller demand never lowers a figure: the reservation is a high-water mark, so a workload
    // alternates big and small calls without ever re-learning the small one.
    note_demand(&mut rt, 1);
    rt.bif_heaps_taken = 2;
    note_demand(&mut rt, 1);
    assert_eq!(need(&rt, capacity), nested);
    rt.bif_heaps_taken = 1;
    assert_eq!(need(&rt, capacity), big);

    // `bif_heaps_taken == 0` (between calls, before the take counts this call) is the outermost
    // depth too, never a third bucket.
    rt.bif_heaps_taken = 0;
    note_demand(&mut rt, 2);
    assert_eq!(need(&rt, capacity), big);
}

/// **The production default is reserved for on the very first call** — the measured defect's shape
/// needs `2^22` words (`lists:duplicate(1 bsl 21, X)`), and the default region's entitlement must
/// cover it, or the first repetition of the shape would still raise `system_limit`.
#[test]
fn the_default_regions_entitlement_covers_the_measured_duplicate_shape() {
    let rt = runtime();
    let capacity = rt.bif_heaps.first().expect("the pool seeds one region").capacity();
    // `lists:duplicate(N, X)` is `2N` words: one cons per element.
    const MEASURED_SHAPE_WORDS: usize = 2 * (1 << 21);
    assert!(
        entitlement(capacity) >= MEASURED_SHAPE_WORDS,
        "the default region's entitlement ({}) must cover the measured shape ({MEASURED_SHAPE_WORDS} words)",
        entitlement(capacity)
    );
    assert!(
        entitlement(capacity) <= capacity,
        "and it must still be servable by the region itself"
    );
}
