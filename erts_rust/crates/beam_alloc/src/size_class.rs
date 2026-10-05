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

use beam_common::Result;

/// An ascending table of block-size classes plus the large-block threshold.
///
/// Every class size is a non-zero multiple of [`crate::block::UNIT_SIZE`]; the table
/// is ascending and its last entry is at least `large_threshold`.
#[derive(Clone, Debug)]
pub struct SizeClasses {
    classes: Vec<usize>,
    large_threshold: usize,
}

impl SizeClasses {
    /// Builds a size-class table after validating it.
    ///
    /// Validation rejects an empty table, a non-ascending table, a zero class, a
    /// class that is not a multiple of [`crate::block::UNIT_SIZE`], and a
    /// `large_threshold` greater than the largest class. A violation is reported as
    /// [`beam_common::BeamError::BadArg`].
    pub fn new(
        classes: impl IntoIterator<Item = usize>,
        large_threshold: usize,
    ) -> Result<SizeClasses> {
        let _ = (classes, large_threshold);
        unimplemented!("SizeClasses::new: validation + construction (allocator implementation pass)")
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
        let _ = size;
        unimplemented!("SizeClasses::class_for (allocator implementation pass)")
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
