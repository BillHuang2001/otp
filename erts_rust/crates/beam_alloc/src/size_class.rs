//! Size classes: the ascending table of block sizes a carrier is subdivided into,
//! plus the large-block threshold above which a dedicated carrier is used.
//!
//! Mirrors the size-class notion of the legacy `erts_alloc` (the `SBC` / `MBC`
//! split): a request at or below [`SizeClasses::large_threshold`] maps to one of the
//! small classes and is served from a multi-block carrier; a larger request gets a
//! dedicated single-block carrier.
//!
//! This is part of the frozen seam (dependents route here); see the crate
//! `CONTEXT.md`.

use beam_common::{BeamError, Result};

use crate::block::{MAX_CLASS_COUNT, UNIT_SIZE};

/// An ascending table of block-size classes plus the large-block threshold.
///
/// A class size is the **payload capacity** of the blocks carved for it: a block of
/// class `ix` occupies `class_size(ix) + BLOCK_HEADER_SIZE` bytes of its carrier and
/// offers `class_size(ix)` bytes to the caller. Every class size is a non-zero
/// multiple of [`crate::block::UNIT_SIZE`]; the table is ascending and its last
/// entry is at least `large_threshold`.
#[derive(Clone, Debug)]
pub struct SizeClasses {
    classes: Vec<usize>,
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
        Ok(SizeClasses {
            classes,
            large_threshold,
        })
    }

    /// The Milestone-1 default table: 16 B … 1 KiB, geometric (each class twice the
    /// previous), with a 1 KiB large-block threshold.
    pub fn default_small_block() -> SizeClasses {
        SizeClasses {
            classes: vec![16, 32, 64, 128, 256, 512, 1024],
            large_threshold: 1024,
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
    pub fn large_threshold(&self) -> usize {
        self.large_threshold
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
