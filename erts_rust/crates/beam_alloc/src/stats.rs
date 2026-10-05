//! Allocation instrumentation.
//!
//! [`AllocStats`] is the public, snapshot view of an allocator's counters — what
//! `system_info`/`erts_alloc` introspection reports. [`Counters`] is the mutable,
//! non-atomic bookkeeping kept behind each allocator's mutex; it is `pub(crate)` and
//! exactly mirrored by [`AllocStats`].

/// A point-in-time snapshot of an allocator's counters.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct AllocStats {
    /// Total successful `alloc` calls.
    pub alloc_calls: u64,
    /// Total `dealloc` calls.
    pub free_calls: u64,
    /// Total `realloc` calls.
    pub realloc_calls: u64,
    /// Blocks currently live.
    pub live_blocks: u64,
    /// Bytes currently live.
    pub live_bytes: usize,
    /// High-water mark of [`live_bytes`](AllocStats::live_bytes).
    pub peak_live_bytes: usize,
    /// Carriers currently mapped.
    pub carriers: u64,
    /// Bytes currently held in mapped carriers.
    pub carrier_bytes: usize,
    /// High-water mark of [`carrier_bytes`](AllocStats::carrier_bytes).
    pub peak_carrier_bytes: usize,
}

/// The mutable counters an allocator updates while holding its lock.
///
/// Not part of the public API: dependents observe state only through the
/// [`AllocStats`] snapshot returned by [`crate::allocator::Allocator::stats`].
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct Counters {
    alloc_calls: u64,
    free_calls: u64,
    realloc_calls: u64,
    live_blocks: u64,
    live_bytes: usize,
    peak_live_bytes: usize,
    carriers: u64,
    carrier_bytes: usize,
    peak_carrier_bytes: usize,
}

impl Counters {
    /// Records a successful allocation of `bytes`.
    pub(crate) fn note_alloc(&mut self, bytes: usize) {
        self.alloc_calls += 1;
        self.live_blocks += 1;
        self.live_bytes += bytes;
        self.peak_live_bytes = self.peak_live_bytes.max(self.live_bytes);
    }

    /// Records a free of `bytes`.
    pub(crate) fn note_free(&mut self, bytes: usize) {
        self.free_calls += 1;
        self.live_blocks = self.live_blocks.saturating_sub(1);
        self.live_bytes = self.live_bytes.saturating_sub(bytes);
    }

    /// Records a successful reallocation.
    pub(crate) fn note_realloc(&mut self) {
        self.realloc_calls += 1;
    }

    /// Records a newly mapped carrier of `bytes`.
    pub(crate) fn note_carrier_up(&mut self, bytes: usize) {
        self.carriers += 1;
        self.carrier_bytes += bytes;
        self.peak_carrier_bytes = self.peak_carrier_bytes.max(self.carrier_bytes);
    }

    /// Records a carrier of `bytes` returned to the OS.
    pub(crate) fn note_carrier_down(&mut self, bytes: usize) {
        self.carriers = self.carriers.saturating_sub(1);
        self.carrier_bytes = self.carrier_bytes.saturating_sub(bytes);
    }

    /// Freezes the counters into a public snapshot.
    pub(crate) fn snapshot(&self) -> AllocStats {
        AllocStats {
            alloc_calls: self.alloc_calls,
            free_calls: self.free_calls,
            realloc_calls: self.realloc_calls,
            live_blocks: self.live_blocks,
            live_bytes: self.live_bytes,
            peak_live_bytes: self.peak_live_bytes,
            carriers: self.carriers,
            carrier_bytes: self.carrier_bytes,
            peak_carrier_bytes: self.peak_carrier_bytes,
        }
    }
}
