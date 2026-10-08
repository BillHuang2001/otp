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

//! Size classes: the ascending ladder of block sizes a carrier is subdivided
//! into, split into an **sbc** (small-block) ladder and an **mbc**
//! (larger-block) ladder, plus the threshold above which a request gets a
//! dedicated single-block carrier.
//!
//! ## Legacy ground truth
//!
//! The legacy carrier framework (`../../erts/emulator/beam/erl_alloc_util.h`,
//! `.c`) configures each allocator through one `AllctrInit_t`. The fields this
//! module models, with the numeric defaults of `ERTS_DEFAULT_ALLCTR_INIT` (the
//! non-`SMALL_MEMORY` variant; the `SMALL_MEMORY` variant's values are listed
//! alongside):
//!
//! | `AllctrInit_t` field | meaning (C comment)                  | default                  | `SMALL_MEMORY` |
//! |----------------------|--------------------------------------|--------------------------|----------------|
//! | `sbct`               | sbc threshold (bytes)                | `512*1024` (512 KiB)     | `64*1024`      |
//! | `smbcs`              | smallest mbc size (bytes)            | `1024*1024` (1 MiB)      | `128*1024`     |
//! | `lmbcs`              | largest mbc size (bytes)             | `10*1024*1024` (10 MiB)  | `1024*1024`    |
//! | `mbcgs`              | mbc growth stages (amount)           | `10`                     | `10`           |
//! | `mmbcs`              | main multiblock carrier size (bytes) | `1024*1024` (1 MiB)      | `128*1024`     |
//! | `ycs`                | `sys_alloc` carrier size (bytes)     | `1024*1024` (1 MiB)      | `128*1024`     |
//! | `mmbc0`              | create main mbc in instance 0 (bool) | `1`                      | `1`            |
//! | `mmsbc`              | max mseg sbcs (amount)               | `256`                    | `256`          |
//! | `asbcst`             | abs sbc shrink threshold             | `2*1024*2024`            | `2*1024*2024`  |
//! | `rsbcst`             | rel sbc shrink threshold (%)         | `20`                     | `20`           |
//! | `rsbcmt`             | rel sbc move threshold (%)           | `80`                     | `80`           |
//! | `rmbcmt`             | rel mbc move threshold (%)           | `50`                     | `50`           |
//!
//! (The `asbcst` literal `2*1024*2024` is quoted verbatim from the header.)
//!
//! In the C, a request of `size >= allctr->sbc_threshold` is handed a dedicated
//! **singleblock** carrier (`create_carrier(allctr, size, CFLG_SBC)` in
//! `erl_alloc_util.c`), while a smaller request is served by a **multiblock**
//! carrier. `erts_alloc.md` states the same rule: "Blocks that are larger than
//! the value of the singleblock carrier threshold (`sbct`) parameter are placed
//! in singleblock carriers. Blocks that are smaller than the value of parameter
//! `sbct` are placed in multiblock carriers."
//!
//! ## What this module models
//!
//! The Rust allocator (`CarrierAllocator`) serves every request at or below
//! [`SizeClasses::large_threshold`] from a carrier dedicated to that request's
//! class (the segregated / slab design of the crate `CONTEXT.md`) and every
//! larger request from a dedicated single-block carrier. This module names the
//! two ladders of that table the way the legacy framework names its two block
//! populations:
//!
//! * the **sbc ladder** — the *small* classes, whose top is the legacy
//!   singleblock-carrier threshold `sbct` ([`SBC_THRESHOLD_DEFAULT`], 512 KiB).
//!   A class in this ladder is an [`SizeClasses::is_small`] class, and its
//!   largest member is [`SizeClasses::sbc_threshold`];
//! * the **mbc ladder** — the *larger* classes, running from the legacy smallest
//!   multiblock size `smbcs` ([`SMALLEST_MBC_SIZE_DEFAULT`], 1 MiB) up to the
//!   legacy largest multiblock size `lmbcs` ([`MBC_THRESHOLD_DEFAULT`], 10 MiB),
//!   which is also [`SizeClasses::mbc_threshold`].
//!
//! A request above the top of the mbc ladder (`lmbcs`) — i.e. above
//! [`SizeClasses::mbc_threshold`] — is served by a dedicated single-block
//! carrier: [`SizeClasses::class_for`] returns `None` for it, the same
//! dedicated-single-block-carrier outcome the legacy `size >= sbct` rule
//! produces.
//!
//! Only the ladder *endpoints* are the legacy numbers above; the class *spacing*
//! in between (a geometric doubling, the same rule as the pre-existing
//! [`SizeClasses::default_small_block`]) is this crate's choice.
//!
//! This is part of the frozen seam (dependents route here); see the crate
//! `CONTEXT.md`.

use beam_common::{BeamError, Result};

use crate::block::{MAX_CLASS_COUNT, UNIT_SIZE};

/// The legacy `AllctrInit_t.sbct`: the *singleblock carrier threshold* in bytes
/// (default `512*1024` in `ERTS_DEFAULT_ALLCTR_INIT`; `64*1024` under
/// `SMALL_MEMORY`).
///
/// In this module it is the top of the sbc ladder and therefore
/// [`SizeClasses::sbc_threshold`] for the legacy default table.
pub const SBC_THRESHOLD_DEFAULT: usize = 512 * 1024;

/// The legacy `AllctrInit_t.smbcs`: the *smallest mbc size* in bytes (default
/// `1024*1024` in `ERTS_DEFAULT_ALLCTR_INIT`; `128*1024` under `SMALL_MEMORY`).
///
/// In this module it is the bottom of the mbc ladder.
pub const SMALLEST_MBC_SIZE_DEFAULT: usize = 1024 * 1024;

/// The legacy `AllctrInit_t.lmbcs`: the *largest mbc size* in bytes (default
/// `10*1024*1024` in `ERTS_DEFAULT_ALLCTR_INIT`; `1024*1024` under
/// `SMALL_MEMORY`).
///
/// In this module it is the top of the mbc ladder and the
/// [`SizeClasses::mbc_threshold`] / [`SizeClasses::large_threshold`]: a request
/// above it gets a dedicated single-block carrier.
pub const MBC_THRESHOLD_DEFAULT: usize = 10 * 1024 * 1024;

/// An ascending table of block-size classes plus the large-block threshold.
///
/// A class size is the **payload capacity** of the blocks carved for it: a block
/// of class `ix` occupies `class_size(ix) + BLOCK_HEADER_SIZE` bytes of its
/// carrier and offers `class_size(ix)` bytes to the caller. Every class size is a
/// non-zero multiple of [`crate::block::UNIT_SIZE`]; the table is ascending and
/// its last entry is at least `large_threshold`.
///
/// The table is split into a leading **sbc** ladder (the *small* classes,
/// indices `0 .. small_count`) and a trailing **mbc** ladder (the larger
/// classes, indices `small_count ..`). See the module documentation for the
/// legacy `sbc`/`mbc` ground truth.
#[derive(Clone, Debug)]
pub struct SizeClasses {
    classes: Vec<usize>,
    /// Number of leading entries of `classes` that form the sbc ladder.
    small_count: usize,
    large_threshold: usize,
}

impl SizeClasses {
    /// Builds a size-class table after validating it.
    ///
    /// Validation rejects an empty table, a non-ascending table, a zero class, a
    /// class that is not a multiple of [`crate::block::UNIT_SIZE`], a
    /// `large_threshold` greater than the largest class, and a table with more than
    /// [`crate::block::MAX_CLASS_COUNT`] entries (the header encoding limit). A
    /// violation is reported as [`beam_common::BeamError::BadArg`].
    ///
    /// The sbc/mbc split is derived from `large_threshold`: every class whose
    /// size is at or below it is an [`is_small`](SizeClasses::is_small) class, so
    /// a table built here always satisfies
    /// `sbc_threshold() <= large_threshold() == mbc_threshold()`.
    pub fn new(
        classes: impl IntoIterator<Item = usize>,
        large_threshold: usize,
    ) -> Result<SizeClasses> {
        let classes: Vec<usize> = classes.into_iter().collect();
        if classes.is_empty() || classes.len() > MAX_CLASS_COUNT {
            return Err(BeamError::BadArg);
        }
        let mut previous = 0usize;
        for &class in &classes {
            if class == 0 || class % UNIT_SIZE != 0 || class <= previous {
                return Err(BeamError::BadArg);
            }
            previous = class;
        }
        if large_threshold > previous {
            return Err(BeamError::BadArg);
        }
        let small_count = classes
            .iter()
            .take_while(|&&class| class <= large_threshold)
            .count();
        Ok(SizeClasses {
            classes,
            small_count,
            large_threshold,
        })
    }

    /// The Milestone-1 default table: 16 B … 1 KiB, geometric (each class twice the
    /// previous), with a 1 KiB large-block threshold. Every class is an sbc class.
    pub fn default_small_block() -> SizeClasses {
        SizeClasses {
            classes: vec![16, 32, 64, 128, 256, 512, 1024],
            small_count: 7,
            large_threshold: 1024,
        }
    }

    /// The legacy **sbc ladder** alone: [`UNIT_SIZE`] doubled up to the legacy
    /// `sbct` ([`SBC_THRESHOLD_DEFAULT`], 512 KiB), with that threshold as the
    /// large-block threshold (so the top of the ladder is the last sbc class).
    ///
    /// This is the `sbct` boundary of `AllctrInit_t`: a request above it would be
    /// served by a dedicated single-block carrier in the legacy framework.
    pub fn default_sbcs() -> SizeClasses {
        let classes = sbc_ladder(SBC_THRESHOLD_DEFAULT);
        let small_count = classes.len();
        SizeClasses {
            classes,
            small_count,
            large_threshold: SBC_THRESHOLD_DEFAULT,
        }
    }

    /// The legacy **mbc ladder** alone: the legacy `smbcs`
    /// ([`SMALLEST_MBC_SIZE_DEFAULT`], 1 MiB) doubled up to the legacy `lmbcs`
    /// ([`MBC_THRESHOLD_DEFAULT`], 10 MiB), with `lmbcs` as the large-block
    /// threshold.
    ///
    /// The table has no sbc classes, so [`is_small`](SizeClasses::is_small) is
    /// `false` for every index and [`sbc_threshold`](SizeClasses::sbc_threshold)
    /// is `0`.
    pub fn default_mbcs() -> SizeClasses {
        SizeClasses {
            classes: mbc_ladder(SMALLEST_MBC_SIZE_DEFAULT, MBC_THRESHOLD_DEFAULT),
            small_count: 0,
            large_threshold: MBC_THRESHOLD_DEFAULT,
        }
    }

    /// The combined legacy table: the [`default_sbcs`](SizeClasses::default_sbcs)
    /// ladder followed by the [`default_mbcs`](SizeClasses::default_mbcs) ladder,
    /// with the legacy `lmbcs` ([`MBC_THRESHOLD_DEFAULT`], 10 MiB) as the
    /// large-block threshold.
    ///
    /// This is the full `sbc` / `mbc` model: the sbc/mbc boundary is
    /// [`sbc_threshold`](SizeClasses::sbc_threshold) (512 KiB) and a request above
    /// [`mbc_threshold`](SizeClasses::mbc_threshold) (10 MiB) needs a dedicated
    /// single-block carrier.
    pub fn default_legacy() -> SizeClasses {
        let mut classes = sbc_ladder(SBC_THRESHOLD_DEFAULT);
        let small_count = classes.len();
        classes.extend(mbc_ladder(SMALLEST_MBC_SIZE_DEFAULT, MBC_THRESHOLD_DEFAULT));
        SizeClasses {
            classes,
            small_count,
            large_threshold: MBC_THRESHOLD_DEFAULT,
        }
    }

    /// The index of the smallest class that can hold `size` bytes.
    ///
    /// Returns `Some(ix)` for `size <= large_threshold()` and `None` otherwise
    /// (`None` means "dedicated carrier").
    pub fn class_for(&self, size: usize) -> Option<usize> {
        if size > self.large_threshold || self.classes.is_empty() {
            return None;
        }
        // The first class whose payload capacity is at least `size`. Terminates
        // inside the table because the last class is >= `large_threshold` >= `size`.
        let ix = self.classes.partition_point(|&class| class < size);
        if ix < self.classes.len() {
            Some(ix)
        } else {
            None
        }
    }

    /// The byte size of the class at `ix`, or `None` when out of range.
    pub fn class_size(&self, ix: usize) -> Option<usize> {
        self.classes.get(ix).copied()
    }

    /// The largest byte size in the table (0 when the table is empty).
    pub fn max_class(&self) -> usize {
        self.classes.last().copied().unwrap_or(0)
    }

    /// The byte size above which a request uses a dedicated carrier.
    ///
    /// This is the top of the mbc ladder; see also
    /// [`mbc_threshold`](SizeClasses::mbc_threshold).
    pub fn large_threshold(&self) -> usize {
        self.large_threshold
    }

    /// Whether the class at `ix` belongs to the sbc (small-block) ladder.
    ///
    /// `true` exactly for the leading classes, i.e. for `ix < sbc_classes().len()`;
    /// an out-of-range index is not a small class.
    pub fn is_small(&self, ix: usize) -> bool {
        ix < self.small_count
    }

    /// The largest byte size of the sbc ladder, i.e. the sbc/mbc boundary
    /// ([`is_small`](SizeClasses::is_small) is `false` from the next class on).
    ///
    /// `0` when the table has no sbc class.
    pub fn sbc_threshold(&self) -> usize {
        match self.small_count {
            0 => 0,
            n => self.classes[n - 1],
        }
    }

    /// The byte size above which a request uses a dedicated single-block carrier:
    /// the top of the mbc ladder (the legacy `lmbcs`).
    ///
    /// Identical to [`large_threshold`](SizeClasses::large_threshold) by
    /// construction.
    pub fn mbc_threshold(&self) -> usize {
        self.large_threshold
    }

    /// The sbc ladder: the leading, *small* classes in ascending order.
    pub fn sbc_classes(&self) -> &[usize] {
        &self.classes[..self.small_count]
    }

    /// The mbc ladder: the trailing, *larger* classes in ascending order.
    pub fn mbc_classes(&self) -> &[usize] {
        &self.classes[self.small_count..]
    }

    /// The number of classes in the table.
    pub fn len(&self) -> usize {
        self.classes.len()
    }

    /// Whether the table has no classes.
    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// Iterates the class byte sizes in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.classes.iter().copied()
    }
}

/// The sbc ladder: `UNIT_SIZE` doubled until it reaches `sbc_threshold`, which is
/// the last entry. `sbc_threshold` is a `UNIT_SIZE` multiple (the legacy `sbct`
/// defaults are), so the result is `UNIT_SIZE`-granular and strictly ascending.
fn sbc_ladder(sbc_threshold: usize) -> Vec<usize> {
    let mut ladder = Vec::new();
    let mut size = UNIT_SIZE;
    while size < sbc_threshold {
        ladder.push(size);
        size *= 2;
    }
    ladder.push(sbc_threshold);
    ladder
}

/// The mbc ladder: `smallest` doubled until it reaches or passes `largest`, which
/// is appended as the final entry when the doubling undershoots it. Both
/// endpoints are `UNIT_SIZE` multiples (the legacy `smbcs`/`lmbcs` defaults are),
/// so the result is `UNIT_SIZE`-granular and strictly ascending.
fn mbc_ladder(smallest: usize, largest: usize) -> Vec<usize> {
    let mut ladder = Vec::new();
    let mut size = smallest;
    while size < largest {
        ladder.push(size);
        size *= 2;
    }
    if ladder.last().copied() != Some(largest) {
        ladder.push(largest);
    }
    ladder
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts that `ladder` is strictly ascending and `UNIT_SIZE`-granular.
    fn assert_ascending_unit_multiple(ladder: &[usize]) {
        let mut previous = 0usize;
        for &class in ladder {
            assert!(class > previous, "classes must be strictly ascending");
            assert_eq!(class % UNIT_SIZE, 0, "classes must be UNIT_SIZE multiples");
            previous = class;
        }
    }

    #[test]
    fn default_table_is_the_documented_geometric_ladder() {
        let classes = SizeClasses::default_small_block();
        assert_eq!(classes.len(), 7);
        assert_eq!(classes.max_class(), 1024);
        assert_eq!(classes.large_threshold(), 1024);
        assert!(!classes.is_empty());
        let table: Vec<usize> = classes.iter().collect();
        assert_eq!(table, vec![16, 32, 64, 128, 256, 512, 1024]);
        assert_eq!(classes.class_size(0), Some(16));
        assert_eq!(classes.class_size(6), Some(1024));
        assert_eq!(classes.class_size(7), None);
    }

    #[test]
    fn new_accepts_a_valid_table() {
        let classes = SizeClasses::new([16, 32, 64], 64).expect("valid table");
        assert_eq!(classes.len(), 3);
        assert_eq!(classes.max_class(), 64);
        assert_eq!(classes.large_threshold(), 64);
        // A threshold below the largest class is fine too.
        let classes = SizeClasses::new([32, 64], 16).expect("valid table");
        assert_eq!(classes.large_threshold(), 16);
        // And a single-class table.
        let classes = SizeClasses::new([16], 0).expect("valid table");
        assert_eq!(classes.len(), 1);
    }

    #[test]
    fn new_rejects_an_empty_table() {
        assert!(matches!(SizeClasses::new([], 0), Err(BeamError::BadArg)));
        assert!(matches!(SizeClasses::new([], 64), Err(BeamError::BadArg)));
    }

    #[test]
    fn new_rejects_a_non_ascending_table() {
        assert!(matches!(
            SizeClasses::new([64, 32], 32),
            Err(BeamError::BadArg)
        ));
        // Equal neighbours are not strictly ascending.
        assert!(matches!(
            SizeClasses::new([32, 32], 32),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn new_rejects_a_zero_class() {
        assert!(matches!(SizeClasses::new([0], 0), Err(BeamError::BadArg)));
        assert!(matches!(
            SizeClasses::new([16, 0], 0),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn new_rejects_a_class_that_is_not_a_unit_multiple() {
        assert!(matches!(SizeClasses::new([24], 16), Err(BeamError::BadArg)));
        assert!(matches!(
            SizeClasses::new([16, 40], 32),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn new_rejects_a_threshold_above_the_largest_class() {
        assert!(matches!(
            SizeClasses::new([16, 32], 64),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            SizeClasses::new([16, 32], 33),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn new_rejects_more_classes_than_the_header_can_encode() {
        let too_many: Vec<usize> = (1..=MAX_CLASS_COUNT + 1).map(|i| i * UNIT_SIZE).collect();
        assert!(matches!(
            SizeClasses::new(too_many, UNIT_SIZE),
            Err(BeamError::BadArg)
        ));
        let exactly_max: Vec<usize> = (1..=MAX_CLASS_COUNT).map(|i| i * UNIT_SIZE).collect();
        assert!(SizeClasses::new(exactly_max, UNIT_SIZE).is_ok());
    }

    #[test]
    fn class_for_boundaries() {
        // 16 / 32 / 64, threshold 64.
        let classes = SizeClasses::new([16, 32, 64], 64).expect("valid table");
        assert_eq!(
            classes.class_for(0),
            Some(0),
            "zero needs the smallest class"
        );
        assert_eq!(classes.class_for(1), Some(0));
        assert_eq!(
            classes.class_for(16),
            Some(0),
            "exact fit uses the class itself"
        );
        assert_eq!(classes.class_for(17), Some(1));
        assert_eq!(classes.class_for(32), Some(1));
        assert_eq!(classes.class_for(33), Some(2));
        assert_eq!(classes.class_for(63), Some(2));
        assert_eq!(
            classes.class_for(64),
            Some(2),
            "the threshold is still a small block"
        );
        assert_eq!(
            classes.class_for(65),
            None,
            "above the threshold: dedicated carrier"
        );
        assert_eq!(classes.class_for(usize::MAX), None);
    }

    #[test]
    fn class_for_is_monotonic_and_within_the_table() {
        let classes = SizeClasses::default_small_block();
        let mut previous = 0usize;
        for size in 0..=classes.large_threshold() {
            let index = classes.class_for(size).expect("in-table size");
            assert!(index < classes.len());
            assert!(classes.class_size(index).expect("size") >= size);
            assert!(index >= previous, "class index must not go backwards");
            previous = index;
        }
        // Exactly one size above the threshold has no class.
        for size in classes.large_threshold() + 1..classes.large_threshold() + 8 {
            assert_eq!(classes.class_for(size), None);
        }
    }

    // ---- sbc / mbc ladders (extended model) -------------------------------

    #[test]
    fn legacy_endpoints_are_the_documented_c_defaults() {
        // `ERTS_DEFAULT_ALLCTR_INIT` of `erl_alloc_util.h`.
        assert_eq!(SBC_THRESHOLD_DEFAULT, 512 * 1024);
        assert_eq!(SMALLEST_MBC_SIZE_DEFAULT, 1024 * 1024);
        assert_eq!(MBC_THRESHOLD_DEFAULT, 10 * 1024 * 1024);
        // All are exact multiples of the unit, as the doubling ladder requires.
        for endpoint in [
            SBC_THRESHOLD_DEFAULT,
            SMALLEST_MBC_SIZE_DEFAULT,
            MBC_THRESHOLD_DEFAULT,
        ] {
            assert_eq!(endpoint % UNIT_SIZE, 0);
        }
    }

    #[test]
    fn sbc_ladder_is_the_unit_doubled_to_sbct() {
        let classes = SizeClasses::default_sbcs();
        assert_eq!(classes.sbc_classes(), classes.iter().collect::<Vec<_>>());
        assert!(classes.mbc_classes().is_empty());
        assert_ascending_unit_multiple(classes.sbc_classes());

        let ladder: Vec<usize> = classes.sbc_classes().to_vec();
        assert_eq!(ladder.first().copied(), Some(UNIT_SIZE));
        assert_eq!(ladder.last().copied(), Some(SBC_THRESHOLD_DEFAULT));
        // Geometric: every entry is twice its predecessor.
        for pair in ladder.windows(2) {
            assert_eq!(pair[1], pair[0] * 2);
        }
        assert_eq!(ladder.len(), 16); // 16 B .. 512 KiB

        // Every class is small; the boundary is exactly `sbct`.
        assert_eq!(classes.sbc_threshold(), SBC_THRESHOLD_DEFAULT);
        for ix in 0..classes.len() {
            assert!(classes.is_small(ix));
        }
        assert!(!classes.is_small(classes.len()));
        assert_eq!(classes.mbc_threshold(), SBC_THRESHOLD_DEFAULT);
        assert_eq!(classes.large_threshold(), SBC_THRESHOLD_DEFAULT);
    }

    #[test]
    fn mbc_ladder_is_smbcs_doubled_to_lmbcs() {
        let classes = SizeClasses::default_mbcs();
        assert!(classes.sbc_classes().is_empty());
        assert_eq!(classes.mbc_classes(), classes.iter().collect::<Vec<_>>());
        assert_ascending_unit_multiple(classes.mbc_classes());

        let ladder: Vec<usize> = classes.mbc_classes().to_vec();
        assert_eq!(ladder.first().copied(), Some(SMALLEST_MBC_SIZE_DEFAULT));
        assert_eq!(ladder.last().copied(), Some(MBC_THRESHOLD_DEFAULT));
        assert_eq!(
            ladder,
            vec![
                1024 * 1024,
                2 * 1024 * 1024,
                4 * 1024 * 1024,
                8 * 1024 * 1024,
                10 * 1024 * 1024,
            ]
        );

        // No sbc classes at all.
        assert_eq!(classes.sbc_threshold(), 0);
        assert_eq!(classes.mbc_threshold(), MBC_THRESHOLD_DEFAULT);
        for ix in 0..classes.len() {
            assert!(!classes.is_small(ix));
        }
    }

    #[test]
    fn combined_legacy_table_splits_at_sbct_and_ends_at_lmbcs() {
        let classes = SizeClasses::default_legacy();
        assert_eq!(classes.len(), 21);
        assert_ascending_unit_multiple(&classes.iter().collect::<Vec<_>>());
        assert_eq!(classes.sbc_classes().len(), 16);
        assert_eq!(classes.mbc_classes().len(), 5);
        assert_eq!(classes.max_class(), MBC_THRESHOLD_DEFAULT);
        assert_eq!(classes.large_threshold(), MBC_THRESHOLD_DEFAULT);
        assert_eq!(classes.mbc_threshold(), MBC_THRESHOLD_DEFAULT);
        assert_eq!(classes.sbc_threshold(), SBC_THRESHOLD_DEFAULT);

        // The two ladders are contiguous and non-overlapping.
        assert_eq!(
            classes.sbc_classes().last().copied(),
            Some(SBC_THRESHOLD_DEFAULT)
        );
        assert_eq!(
            classes.mbc_classes().first().copied(),
            Some(SMALLEST_MBC_SIZE_DEFAULT)
        );
        assert!(classes.mbc_classes()[0] > classes.sbc_classes()[15]);
    }

    #[test]
    fn is_small_flips_exactly_at_the_sbc_threshold() {
        let classes = SizeClasses::default_legacy();
        let boundary = classes.sbc_classes().len();
        // Small up to and including the sbc threshold class...
        for ix in 0..boundary {
            assert!(classes.is_small(ix), "class {ix} must be small");
            assert!(classes.class_size(ix).expect("in range") <= classes.sbc_threshold());
        }
        // ... and large from the first mbc class onwards.
        for ix in boundary..classes.len() {
            assert!(!classes.is_small(ix), "class {ix} must not be small");
            assert!(classes.class_size(ix).expect("in range") > classes.sbc_threshold());
        }
        assert!(!classes.is_small(classes.len()));
        assert!(!classes.is_small(usize::MAX));
    }

    #[test]
    fn class_for_crosses_the_sbc_mbc_boundary_and_the_dedicated_carrier_line() {
        let classes = SizeClasses::default_legacy();
        let sbc_top_ix = classes.sbc_classes().len() - 1;
        // The sbc threshold is the last small class ...
        assert_eq!(classes.class_for(SBC_THRESHOLD_DEFAULT), Some(sbc_top_ix));
        assert!(classes.is_small(sbc_top_ix));
        // ... one byte more lands on the first mbc class.
        let mbc_first_ix = sbc_top_ix + 1;
        assert_eq!(classes.class_for(SBC_THRESHOLD_DEFAULT + 1), Some(mbc_first_ix));
        assert!(!classes.is_small(mbc_first_ix));
        assert_eq!(
            classes.class_size(mbc_first_ix),
            Some(SMALLEST_MBC_SIZE_DEFAULT)
        );
        // The top of the mbc ladder is still served ...
        assert_eq!(classes.class_for(MBC_THRESHOLD_DEFAULT), Some(20));
        // ... and above it a dedicated single-block carrier is needed.
        assert_eq!(classes.class_for(MBC_THRESHOLD_DEFAULT + 1), None);

        // Monotonic across the whole combined ladder.
        let mut previous = 0usize;
        for size in 0..=classes.large_threshold() {
            let ix = classes.class_for(size).expect("in-table size");
            assert!(ix >= previous, "class index must not go backwards");
            assert!(classes.class_size(ix).expect("size") >= size);
            previous = ix;
        }
    }

    #[test]
    fn zero_sized_request_uses_the_smallest_sbc_class() {
        let classes = SizeClasses::default_legacy();
        assert_eq!(classes.class_for(0), Some(0));
        assert!(classes.is_small(0));
        assert_eq!(classes.class_size(0), Some(UNIT_SIZE));
    }

    #[test]
    fn new_derives_the_sbc_mbc_split_from_the_threshold() {
        // small_count = number of classes <= the threshold.
        let classes = SizeClasses::new([16, 32, 64, 128], 64).expect("valid table");
        assert_eq!(classes.sbc_classes(), &[16, 32, 64]);
        assert_eq!(classes.mbc_classes(), &[128]);
        assert_eq!(classes.sbc_threshold(), 64);
        assert_eq!(classes.mbc_threshold(), 64);
        assert_eq!(classes.large_threshold(), 64);
        assert!(classes.is_small(2));
        assert!(!classes.is_small(3));

        // A threshold below every class leaves an empty sbc ladder.
        let classes = SizeClasses::new([32, 64], 16).expect("valid table");
        assert!(classes.sbc_classes().is_empty());
        assert_eq!(classes.sbc_threshold(), 0);
        assert_eq!(classes.mbc_classes(), &[32, 64]);
        assert!(!classes.is_small(0));

        // A threshold at or above every class leaves an empty mbc ladder.
        let classes = SizeClasses::new([16, 32, 64, 1024], 1024).expect("valid table");
        assert!(classes.mbc_classes().is_empty());
        assert_eq!(classes.sbc_threshold(), 1024);
        assert_eq!(classes.mbc_threshold(), 1024);
        assert!(classes.is_small(3));

        // The 1 KiB default is entirely sbc, as documented.
        let classes = SizeClasses::default_small_block();
        assert_eq!(classes.sbc_classes().len(), 7);
        assert!(classes.mbc_classes().is_empty());
        assert_eq!(classes.sbc_threshold(), 1024);
    }

    #[test]
    fn sbc_and_mbc_ladders_are_each_ordered_and_disjoint() {
        for classes in [
            SizeClasses::default_sbcs(),
            SizeClasses::default_mbcs(),
            SizeClasses::default_legacy(),
        ] {
            assert_ascending_unit_multiple(classes.sbc_classes());
            assert_ascending_unit_multiple(classes.mbc_classes());
            assert_eq!(
                classes.sbc_classes().len() + classes.mbc_classes().len(),
                classes.len()
            );
            if let (Some(&sbc_top), Some(&mbc_bottom)) = (
                classes.sbc_classes().last(),
                classes.mbc_classes().first(),
            ) {
                assert!(sbc_top < mbc_bottom, "ladders must not overlap");
            }
            // Out-of-range indices are never "small".
            assert!(!classes.is_small(classes.len()));
        }
    }
}
