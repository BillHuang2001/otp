//! Collection statistics: the [`GcStats`] counters accumulated across collections.

/// Accumulated garbage-collection counters for a process heap.
///
/// Cumulative counters (`minor_collections`, `major_collections`) span the whole
/// collector lifetime; the per-collection counters are reported for the most recent
/// collection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcStats {
    /// Number of term words copied into the to-space.
    pub words_copied: usize,
    /// Number of term words reclaimed (freed rather than copied).
    pub words_reclaimed: usize,
    /// Number of boxed objects and cons cells copied.
    pub objects_copied: usize,
    /// Number of minor (young-only) collections performed.
    pub minor_collections: u64,
    /// Number of major (fullsweep) collections performed.
    pub major_collections: u64,
}
