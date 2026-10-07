//! Allocation instrumentation — the allocator's observability layer.
//!
//! This is what `erts_alloc:info/0` / `erlang:system_info(allocator)` report on. It has
//! three parts:
//!
//! * [`Counters`] — the mutable, non-atomic bookkeeping an allocator updates while
//!   holding its lock (`pub(crate)`, driven by `allocator.rs`). It mirrors the legacy
//!   `Allctr_t::calls` / `CarriersStats_t` counters plus the mseg segment cache.
//! * [`AllocStats`] — the public, `Copy` point-in-time snapshot of [`Counters`],
//!   including the per-allocation-*type* table and the [`MsegStats`] block.
//! * [`AllocatorInfo`] / [`AllocatorInfoItem`] / [`InfoValue`] — the report data types
//!   and the assembler that turns an [`AllocStats`] into the item list of the
//!   `=allocator:<name>` section of the `erts_alloc:info/0` report, reusing the legacy
//!   item names.
//!
//! Ground truth (read-only):
//! `erts_alcu_info` / `info_carriers` / `info_calls` in
//! `../../erts/emulator/beam/erl_alloc_util.c`, `erts_allocator_info` in
//! `../../erts/emulator/beam/erl_alloc.c` (the `=allocator:<name>` report shape) and
//! `erts_mseg_info` / `info_status` in
//! `../../erts/emulator/sys/common/erl_mseg.c` (the mseg counter names).
//!
//! # Isolation
//!
//! This module is deliberately self-contained: it references **no** other module of the
//! crate (no `types`, no `registry`, no allocator internals). The per-type table is
//! addressed by a plain `usize` slot index and the type *name* is supplied by the caller
//! as a `&'static str`, so this layer needs no knowledge of the type inventory. Safe
//! Rust only — no `unsafe`.

/// Per-allocation-*type* counters within one allocator.
///
/// One value is produced by [`AllocStats::type_stats`] for a given slot index.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct TypeStats {
    /// Blocks of this type currently live.
    pub blocks: u64,
    /// Bytes of this type currently live.
    pub bytes: usize,
    /// High-water mark of [`bytes`](TypeStats::bytes) for this type.
    pub peak_bytes: usize,
}

/// The counter slot stored in the fixed per-type table of [`Counters`] / [`AllocStats`].
///
/// It is the same value as [`TypeStats`]; the distinct name marks the *slot* role of the
/// type inside the fixed-size table (the table itself must stay `Copy`).
pub type TypeCounters = TypeStats;

/// The number of slots in the per-allocation-*type* counter table kept inside
/// [`AllocStats`] (and [`Counters`]).
///
/// **Invariant:** the crate's real allocation-type inventory (`types::TYPE_COUNT`, owned
/// by `src/types.rs`) must never exceed this capacity —
/// `types::TYPE_COUNT <= STATS_TYPE_SLOTS`. The table is a fixed array (not a `Vec`)
/// precisely so [`AllocStats`] stays `Copy`; a type index at or beyond this bound is
/// ignored rather than panicking (see [`AllocStats::type_stats`]).
pub const STATS_TYPE_SLOTS: usize = 512;

/// Segment-cache / `mseg` counters, shaped like `erts_mseg_info`'s `status` block
/// (`segments` / `segments_size` / `cache_hits` and the unmap count).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MsegStats {
    /// Segments currently allocated (`segments: <current>`).
    pub segments: u64,
    /// Bytes currently held in segments (`segments_size: <current>`).
    pub bytes: usize,
    /// High-water mark of [`bytes`](MsegStats::bytes).
    pub peak_bytes: usize,
    /// Segments served from the segment cache (`cache_hits`).
    pub reused: u64,
    /// Segments returned to the OS via `munmap`.
    pub unmaps: u64,
}

/// A point-in-time snapshot of an allocator's counters.
///
/// # The per-type table and `Copy`
///
/// The per-allocation-type table is a fixed `[TypeCounters; STATS_TYPE_SLOTS]` array
/// held *inline* rather than behind a `Box`/`Vec`, so a snapshot stays `Copy` and can be
/// taken (and cheaply passed around) without touching an allocator's lock. The cost is
/// the struct's size: at `STATS_TYPE_SLOTS == 512` and 24 bytes per slot the snapshot is
/// roughly 12 KiB. That is a deliberate trade-off — snapshots are taken by introspection
/// code (`system_info`/`erts_alloc:info/0`), not on the allocation fast path, and an
/// inline table keeps the type breakdown allocation-free. Use
/// [`type_stats`](AllocStats::type_stats) to read a single slot (the total is
/// dominated by the table, so this is the intended accessor).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// Carriers currently mapped (single-block carriers plus multi-block carriers).
    pub carriers: u64,
    /// Bytes currently held in mapped carriers.
    pub carrier_bytes: usize,
    /// High-water mark of [`carrier_bytes`](AllocStats::carrier_bytes).
    pub peak_carrier_bytes: usize,
    /// Single-block carriers (SBC) currently mapped.
    pub sbc_carriers: u64,
    /// Multi-block carriers (MBC) currently mapped.
    pub mbc_carriers: u64,
    /// Segment-cache (`mseg`) counters.
    pub mseg: MsegStats,
    /// Per-allocation-type counters, indexed by the type's slot index.
    pub types: [TypeCounters; STATS_TYPE_SLOTS],
}

impl Default for AllocStats {
    fn default() -> Self {
        AllocStats {
            alloc_calls: 0,
            free_calls: 0,
            realloc_calls: 0,
            live_blocks: 0,
            live_bytes: 0,
            peak_live_bytes: 0,
            carriers: 0,
            carrier_bytes: 0,
            peak_carrier_bytes: 0,
            sbc_carriers: 0,
            mbc_carriers: 0,
            mseg: MsegStats::default(),
            types: [TypeStats::default(); STATS_TYPE_SLOTS],
        }
    }
}

impl AllocStats {
    /// The counters of allocation type slot `ty_ix`.
    ///
    /// Returns the all-zero value for an out-of-range index (at or past
    /// [`STATS_TYPE_SLOTS`]) instead of panicking — an unknown/foreign type index is a
    /// normal condition for an instrumentation layer whose type inventory lives in a
    /// different module.
    pub fn type_stats(&self, ty_ix: usize) -> TypeStats {
        self.types.get(ty_ix).copied().unwrap_or_default()
    }
}

/// The mutable counters an allocator updates while holding its lock.
///
/// Not part of the public API: dependents observe state only through the
/// [`AllocStats`] snapshot returned by [`crate::allocator::Allocator::stats`].
///
/// The `note_*` methods never fail and never panic; a free/recycle that would drive a
/// counter below zero saturates at zero instead (a mismatched free/alloc pair must not
/// corrupt the instrumentation or abort the runtime).
#[derive(Clone, Copy, Debug)]
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
    sbc_carriers: u64,
    mbc_carriers: u64,
    mseg: MsegStats,
    types: [TypeCounters; STATS_TYPE_SLOTS],
}

impl Default for Counters {
    fn default() -> Self {
        Counters {
            alloc_calls: 0,
            free_calls: 0,
            realloc_calls: 0,
            live_blocks: 0,
            live_bytes: 0,
            peak_live_bytes: 0,
            carriers: 0,
            carrier_bytes: 0,
            peak_carrier_bytes: 0,
            sbc_carriers: 0,
            mbc_carriers: 0,
            mseg: MsegStats::default(),
            types: [TypeStats::default(); STATS_TYPE_SLOTS],
        }
    }
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

    // -- Typed attribution ---------------------------------------------------------
    //
    // The methods below are the *typed* (per-type / per-carrier-class / mseg) variants of
    // the aggregate counters above. Each records BOTH the class breakdown and the
    // aggregate, so a caller records an event exactly once: use either the typed method
    // or the untyped aggregate method for a given event, never both. They are added here
    // ahead of the allocator wiring that will call them (hence `allow(dead_code)`).

    /// Records a successful allocation of `bytes` attributed to allocation type `ty_ix`.
    ///
    /// Performs the [`note_alloc`](Counters::note_alloc) aggregate accounting and updates
    /// the `ty_ix` slot of the per-type table. A `ty_ix` at or past
    /// [`STATS_TYPE_SLOTS`] is ignored (the aggregate accounting still happens).
    #[allow(dead_code)]
    pub(crate) fn note_alloc_typed(&mut self, ty_ix: usize, bytes: usize) {
        self.note_alloc(bytes);
        if let Some(slot) = self.types.get_mut(ty_ix) {
            slot.blocks = slot.blocks.saturating_add(1);
            slot.bytes = slot.bytes.saturating_add(bytes);
            slot.peak_bytes = slot.peak_bytes.max(slot.bytes);
        }
    }

    /// Records a free of `bytes` attributed to allocation type `ty_ix`.
    ///
    /// Performs the [`note_free`](Counters::note_free) aggregate accounting and updates
    /// the `ty_ix` slot of the per-type table (saturating at zero). A `ty_ix` at or past
    /// [`STATS_TYPE_SLOTS`] is ignored.
    #[allow(dead_code)]
    pub(crate) fn note_free_typed(&mut self, ty_ix: usize, bytes: usize) {
        self.note_free(bytes);
        if let Some(slot) = self.types.get_mut(ty_ix) {
            slot.blocks = slot.blocks.saturating_sub(1);
            slot.bytes = slot.bytes.saturating_sub(bytes);
        }
    }

    /// Records a newly mapped single-block carrier (SBC) of `bytes`.
    #[allow(dead_code)]
    pub(crate) fn note_sbc_carrier_up(&mut self, bytes: usize) {
        self.note_carrier_up(bytes);
        self.sbc_carriers = self.sbc_carriers.saturating_add(1);
    }

    /// Records a single-block carrier (SBC) of `bytes` returned to the OS.
    #[allow(dead_code)]
    pub(crate) fn note_sbc_carrier_down(&mut self, bytes: usize) {
        self.note_carrier_down(bytes);
        self.sbc_carriers = self.sbc_carriers.saturating_sub(1);
    }

    /// Records a newly mapped multi-block carrier (MBC) of `bytes`.
    #[allow(dead_code)]
    pub(crate) fn note_mbc_carrier_up(&mut self, bytes: usize) {
        self.note_carrier_up(bytes);
        self.mbc_carriers = self.mbc_carriers.saturating_add(1);
    }

    /// Records a multi-block carrier (MBC) of `bytes` returned to the OS.
    #[allow(dead_code)]
    pub(crate) fn note_mbc_carrier_down(&mut self, bytes: usize) {
        self.note_carrier_down(bytes);
        self.mbc_carriers = self.mbc_carriers.saturating_sub(1);
    }

    /// Records a segment of `bytes` obtained from `mseg` (`segments` / `segments_size`).
    #[allow(dead_code)]
    pub(crate) fn note_mseg_up(&mut self, bytes: usize) {
        self.mseg.segments = self.mseg.segments.saturating_add(1);
        self.mseg.bytes = self.mseg.bytes.saturating_add(bytes);
        self.mseg.peak_bytes = self.mseg.peak_bytes.max(self.mseg.bytes);
    }

    /// Records a segment of `bytes` unmapped by `mseg`.
    #[allow(dead_code)]
    pub(crate) fn note_mseg_down(&mut self, bytes: usize) {
        self.mseg.segments = self.mseg.segments.saturating_sub(1);
        self.mseg.bytes = self.mseg.bytes.saturating_sub(bytes);
    }

    /// Records a segment served from the `mseg` segment cache (a cache hit).
    #[allow(dead_code)]
    pub(crate) fn note_mseg_reuse(&mut self) {
        self.mseg.reused = self.mseg.reused.saturating_add(1);
    }

    /// Records a segment returned to the OS by `mseg` (`munmap`).
    #[allow(dead_code)]
    pub(crate) fn note_mseg_unmap(&mut self) {
        self.mseg.unmaps = self.mseg.unmaps.saturating_add(1);
    }

    /// The current segment-cache counters.
    #[allow(dead_code)]
    pub(crate) fn mseg(&self) -> MsegStats {
        self.mseg
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
            sbc_carriers: self.sbc_carriers,
            mbc_carriers: self.mbc_carriers,
            mseg: self.mseg,
            types: self.types,
        }
    }
}

/// One `{Key, Value}` row of an [`AllocatorInfo`] report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AllocatorInfoItem {
    /// The legacy item name (e.g. `"carriers"`, `"carriers size"`, `"alloc calls"`).
    pub key: &'static str,
    /// The item's value.
    pub value: InfoValue,
}

/// The value of an [`AllocatorInfoItem`].
///
/// Three variants suffice for the whole `erts_alloc:info/0` item set: 64-bit call
/// counters, byte sizes / block counts, and static text (a type name, or an option).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InfoValue {
    /// A 64-bit counter.
    U64(u64),
    /// A byte size or block count.
    Usize(usize),
    /// A static textual value.
    Text(&'static str),
}

/// An `erts_alloc:info/0`-shaped report for a single allocator instance — the
/// `=allocator:<name>` section of `erl_alloc.c:2835` plus its `{Item, Value}` list.
///
/// The assembler ([`from_stats`](AllocatorInfo::from_stats) /
/// [`push_type`](AllocatorInfo::push_type)) needs no knowledge of the allocation-type
/// inventory: the type name is always supplied by the caller.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AllocatorInfo {
    /// The allocator's name (e.g. `"eheap_alloc"`, `"binary_alloc"`).
    pub name: &'static str,
    /// The report's items, in report order.
    pub items: Vec<AllocatorInfoItem>,
}

/// The private item constructor.
const fn item(key: &'static str, value: InfoValue) -> AllocatorInfoItem {
    AllocatorInfoItem { key, value }
}

impl AllocatorInfo {
    /// Builds the per-allocator items from an [`AllocStats`] snapshot.
    ///
    /// Item names are the legacy ones where they exist (`info_calls` / `info_carriers`
    /// in `erl_alloc_util.c`, `info_status` in `erl_mseg.c`):
    ///
    /// | key | value |
    /// |---|---|
    /// | `blocks` | [`AllocStats::live_blocks`] |
    /// | `bytes` | [`AllocStats::live_bytes`] |
    /// | `carriers` | [`AllocStats::carriers`] |
    /// | `carriers size` | [`AllocStats::carrier_bytes`] |
    /// | `sbcs carriers` | [`AllocStats::sbc_carriers`] |
    /// | `mbcs carriers` | [`AllocStats::mbc_carriers`] |
    /// | `alloc calls` | [`AllocStats::alloc_calls`] |
    /// | `free calls` | [`AllocStats::free_calls`] |
    /// | `realloc calls` | [`AllocStats::realloc_calls`] |
    /// | `mseg carriers` | [`MsegStats::segments`] |
    /// | `mseg carriers size` | [`MsegStats::bytes`] |
    /// | `mseg cache_hits` | [`MsegStats::reused`] |
    /// | `mseg unmaps` | [`MsegStats::unmaps`] |
    pub fn from_stats(name: &'static str, stats: &AllocStats) -> AllocatorInfo {
        let items = vec![
            item("blocks", InfoValue::U64(stats.live_blocks)),
            item("bytes", InfoValue::Usize(stats.live_bytes)),
            item("carriers", InfoValue::U64(stats.carriers)),
            item("carriers size", InfoValue::Usize(stats.carrier_bytes)),
            item("sbcs carriers", InfoValue::U64(stats.sbc_carriers)),
            item("mbcs carriers", InfoValue::U64(stats.mbc_carriers)),
            item("alloc calls", InfoValue::U64(stats.alloc_calls)),
            item("free calls", InfoValue::U64(stats.free_calls)),
            item("realloc calls", InfoValue::U64(stats.realloc_calls)),
            item("mseg carriers", InfoValue::U64(stats.mseg.segments)),
            item("mseg carriers size", InfoValue::Usize(stats.mseg.bytes)),
            item("mseg cache_hits", InfoValue::U64(stats.mseg.reused)),
            item("mseg unmaps", InfoValue::U64(stats.mseg.unmaps)),
        ];
        AllocatorInfo { name, items }
    }

    /// Appends the items for one allocation type, given its name.
    ///
    /// The legacy text report prints per-type rows as `blocks[<name>] count:` /
    /// `blocks[<name>] size:`; because [`AllocatorInfoItem::key`] is a `&'static str` the
    /// name cannot be interpolated into the key, so it is emitted as a `type` header item
    /// followed by fixed-key items:
    ///
    /// * `type` → [`InfoValue::Text`] (the type name),
    /// * `type blocks` → [`TypeStats::blocks`],
    /// * `type bytes` → [`TypeStats::bytes`],
    /// * `type peak_bytes` → [`TypeStats::peak_bytes`].
    ///
    /// The caller obtains `stats` from [`AllocStats::type_stats`] and the name from the
    /// type inventory; this assembler stays ignorant of both.
    pub fn push_type(&mut self, name: &'static str, stats: TypeStats) {
        self.items.push(item("type", InfoValue::Text(name)));
        self.items
            .push(item("type blocks", InfoValue::U64(stats.blocks)));
        self.items
            .push(item("type bytes", InfoValue::Usize(stats.bytes)));
        self.items
            .push(item("type peak_bytes", InfoValue::Usize(stats.peak_bytes)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Finds an item's value by key (the report is a list, so a lookup helper mirrors how
    /// a consumer reads it).
    fn lookup(info: &AllocatorInfo, key: &str) -> Option<InfoValue> {
        info.items
            .iter()
            .find(|i| i.key == key)
            .map(|i| i.value)
    }

    #[test]
    fn a_default_snapshot_is_all_zero() {
        let stats = AllocStats::default();
        assert_eq!(stats, Counters::default().snapshot());
        assert_eq!(stats.alloc_calls, 0);
        assert_eq!(stats.live_bytes, 0);
        assert_eq!(stats.carriers, 0);
        assert_eq!(stats.sbc_carriers, 0);
        assert_eq!(stats.mbc_carriers, 0);
        assert_eq!(stats.mseg, MsegStats::default());
        assert!(stats.types.iter().all(|t| *t == TypeStats::default()));
    }

    #[test]
    fn alloc_and_free_track_live_bytes_blocks_and_peak() {
        let mut c = Counters::default();
        c.note_alloc(64);
        c.note_alloc(32);
        let s = c.snapshot();
        assert_eq!(s.alloc_calls, 2);
        assert_eq!(s.live_blocks, 2);
        assert_eq!(s.live_bytes, 96);
        assert_eq!(s.peak_live_bytes, 96);
        c.note_free(64);
        let s = c.snapshot();
        assert_eq!(s.free_calls, 1);
        assert_eq!(s.live_blocks, 1);
        assert_eq!(s.live_bytes, 32);
        // The high-water mark survives the free.
        assert_eq!(s.peak_live_bytes, 96);
    }

    #[test]
    fn a_surplus_free_saturates_instead_of_underflowing() {
        let mut c = Counters::default();
        c.note_alloc(16);
        c.note_free(64);
        c.note_free(64);
        let s = c.snapshot();
        assert_eq!(s.free_calls, 2);
        assert_eq!(s.live_blocks, 0, "no underflow on the block count");
        assert_eq!(s.live_bytes, 0, "no underflow on the byte count");
    }

    #[test]
    fn realloc_only_counts_calls() {
        let mut c = Counters::default();
        c.note_realloc();
        c.note_realloc();
        assert_eq!(c.snapshot().realloc_calls, 2);
        assert_eq!(c.snapshot().live_blocks, 0);
        assert_eq!(c.snapshot().live_bytes, 0);
    }

    #[test]
    fn typed_allocs_are_isolated_per_type() {
        let mut c = Counters::default();
        c.note_alloc_typed(3, 40);
        c.note_alloc_typed(3, 8);
        c.note_alloc_typed(4, 100);
        let s = c.snapshot();
        assert_eq!(s.type_stats(3), TypeStats { blocks: 2, bytes: 48, peak_bytes: 48 });
        assert_eq!(s.type_stats(4), TypeStats { blocks: 1, bytes: 100, peak_bytes: 100 });
        // Untouched neighbours stay zero.
        for ix in [0usize, 2, 5, 7, STATS_TYPE_SLOTS - 1] {
            assert_eq!(s.type_stats(ix), TypeStats::default(), "slot {ix} untouched");
        }
        // The typed accounting also feeds the aggregate counters.
        assert_eq!(s.alloc_calls, 3);
        assert_eq!(s.live_blocks, 3);
        assert_eq!(s.live_bytes, 148);
    }

    #[test]
    fn typed_peak_is_a_high_water_mark() {
        let mut c = Counters::default();
        c.note_alloc_typed(2, 100);
        c.note_alloc_typed(2, 100);
        c.note_free_typed(2, 150);
        let slot = c.snapshot().type_stats(2);
        assert_eq!(slot.blocks, 1);
        assert_eq!(slot.bytes, 50);
        assert_eq!(slot.peak_bytes, 200, "peak survives the free");
    }

    #[test]
    fn typed_free_saturates_per_type() {
        let mut c = Counters::default();
        c.note_alloc_typed(1, 16);
        c.note_free_typed(1, 512);
        let slot = c.snapshot().type_stats(1);
        assert_eq!(slot.blocks, 0);
        assert_eq!(slot.bytes, 0);
    }

    #[test]
    fn an_out_of_range_type_index_is_ignored_and_reads_as_zero() {
        let mut c = Counters::default();
        // Must not panic; the aggregate accounting still happens.
        c.note_alloc_typed(STATS_TYPE_SLOTS, 32);
        c.note_alloc_typed(usize::MAX, 32);
        c.note_free_typed(usize::MAX, 32);
        let s = c.snapshot();
        assert_eq!(s.alloc_calls, 2);
        assert_eq!(s.free_calls, 1);
        assert_eq!(s.live_bytes, 32);
        // Out-of-range reads return the zero value rather than panicking.
        assert_eq!(s.type_stats(STATS_TYPE_SLOTS), TypeStats::default());
        assert_eq!(s.type_stats(usize::MAX), TypeStats::default());
        // The last in-range slot is usable.
        assert!(s.types.get(STATS_TYPE_SLOTS - 1).is_some());
    }

    #[test]
    fn sbc_and_mbc_carriers_break_down_the_total() {
        let mut c = Counters::default();
        c.note_mbc_carrier_up(64 * 1024);
        c.note_sbc_carrier_up(4096);
        let s = c.snapshot();
        assert_eq!(s.carriers, 2, "typed carriers feed the aggregate count");
        assert_eq!(s.mbc_carriers, 1);
        assert_eq!(s.sbc_carriers, 1);
        assert_eq!(s.carrier_bytes, 64 * 1024 + 4096);
        assert_eq!(s.peak_carrier_bytes, 64 * 1024 + 4096);
        c.note_mbc_carrier_down(64 * 1024);
        let s = c.snapshot();
        assert_eq!(s.carriers, 1);
        assert_eq!(s.mbc_carriers, 0);
        assert_eq!(s.sbc_carriers, 1);
        assert_eq!(s.carrier_bytes, 4096);
        assert_eq!(s.peak_carrier_bytes, 64 * 1024 + 4096, "peak is retained");
    }

    #[test]
    fn carrier_downs_saturate_and_the_untyped_path_leaves_the_breakdown_alone() {
        let mut c = Counters::default();
        c.note_sbc_carrier_down(1024);
        c.note_mbc_carrier_down(1024);
        let s = c.snapshot();
        assert_eq!((s.sbc_carriers, s.mbc_carriers, s.carriers), (0, 0, 0));
        assert_eq!(s.carrier_bytes, 0);
        // The untyped aggregate path (what `allocator.rs` calls today) does not touch the
        // per-class breakdown, and its down path saturates too.
        c.note_carrier_up(2048);
        c.note_carrier_down(4096);
        let s = c.snapshot();
        assert_eq!(s.carriers, 0);
        assert_eq!(s.carrier_bytes, 0);
        assert_eq!((s.sbc_carriers, s.mbc_carriers), (0, 0));
    }

    #[test]
    fn mseg_counters_track_segments_peak_reuse_and_unmaps() {
        let mut c = Counters::default();
        c.note_mseg_up(1 << 20);
        c.note_mseg_up(1 << 20);
        c.note_mseg_reuse();
        c.note_mseg_reuse();
        c.note_mseg_unmap();
        assert_eq!(
            c.mseg(),
            MsegStats { segments: 2, bytes: 2 << 20, peak_bytes: 2 << 20, reused: 2, unmaps: 1 }
        );
        c.note_mseg_down(1 << 20);
        let s = c.snapshot();
        assert_eq!(s.mseg.segments, 1);
        assert_eq!(s.mseg.bytes, 1 << 20);
        assert_eq!(s.mseg.peak_bytes, 2 << 20, "peak is retained");
        // Recycle/shrink saturates at zero.
        c.note_mseg_down(64 << 20);
        let s = c.snapshot();
        assert_eq!((s.mseg.segments, s.mseg.bytes), (0, 0));
    }

    #[test]
    fn snapshot_round_trips_every_counter() {
        let mut c = Counters::default();
        c.note_alloc(8);
        c.note_alloc_typed(11, 24);
        c.note_free(8);
        c.note_free_typed(11, 4);
        c.note_realloc();
        c.note_mbc_carrier_up(512);
        c.note_sbc_carrier_up(256);
        c.note_mbc_carrier_down(512);
        c.note_mseg_up(4096);
        c.note_mseg_reuse();
        c.note_mseg_unmap();
        c.note_mseg_down(1024);

        let s1 = c.snapshot();
        let s2 = c.snapshot();
        assert_eq!(s1, s2, "snapshots are deterministic");
        assert_eq!(s1.alloc_calls, c.alloc_calls);
        assert_eq!(s1.free_calls, c.free_calls);
        assert_eq!(s1.realloc_calls, c.realloc_calls);
        assert_eq!(s1.live_blocks, c.live_blocks);
        assert_eq!(s1.live_bytes, c.live_bytes);
        assert_eq!(s1.peak_live_bytes, c.peak_live_bytes);
        assert_eq!(s1.carriers, c.carriers);
        assert_eq!(s1.carrier_bytes, c.carrier_bytes);
        assert_eq!(s1.peak_carrier_bytes, c.peak_carrier_bytes);
        assert_eq!(s1.sbc_carriers, c.sbc_carriers);
        assert_eq!(s1.mbc_carriers, c.mbc_carriers);
        assert_eq!(s1.mseg, c.mseg);
        assert_eq!(s1.types, c.types);
        assert_eq!(s1.type_stats(11), TypeStats { blocks: 0, bytes: 20, peak_bytes: 24 });
    }

    #[test]
    fn a_snapshot_is_copy_and_compares_by_value() {
        let mut c = Counters::default();
        c.note_alloc_typed(5, 16);
        let a = c.snapshot();
        let b = a; // `Copy`, not a move.
        assert_eq!(a, b);
        c.note_alloc(16);
        assert_ne!(a, c.snapshot());
    }

    #[test]
    fn the_report_names_the_allocator_and_carries_the_legacy_items() {
        let mut c = Counters::default();
        c.note_alloc_typed(2, 64);
        c.note_mbc_carrier_up(65536);
        c.note_sbc_carrier_up(4096);
        c.note_mseg_up(1 << 20);
        c.note_mseg_reuse();
        c.note_mseg_unmap();
        let info = AllocatorInfo::from_stats("eheap_alloc", &c.snapshot());

        assert_eq!(info.name, "eheap_alloc");
        // Exactly the documented item set, in the documented order.
        let keys: Vec<&str> = info.items.iter().map(|i| i.key).collect();
        assert_eq!(
            keys,
            vec![
                "blocks",
                "bytes",
                "carriers",
                "carriers size",
                "sbcs carriers",
                "mbcs carriers",
                "alloc calls",
                "free calls",
                "realloc calls",
                "mseg carriers",
                "mseg carriers size",
                "mseg cache_hits",
                "mseg unmaps",
            ]
        );

        assert_eq!(lookup(&info, "blocks"), Some(InfoValue::U64(1)));
        assert_eq!(lookup(&info, "bytes"), Some(InfoValue::Usize(64)));
        assert_eq!(lookup(&info, "carriers"), Some(InfoValue::U64(2)));
        assert_eq!(lookup(&info, "carriers size"), Some(InfoValue::Usize(65536 + 4096)));
        assert_eq!(lookup(&info, "sbcs carriers"), Some(InfoValue::U64(1)));
        assert_eq!(lookup(&info, "mbcs carriers"), Some(InfoValue::U64(1)));
        assert_eq!(lookup(&info, "alloc calls"), Some(InfoValue::U64(1)));
        assert_eq!(lookup(&info, "free calls"), Some(InfoValue::U64(0)));
        assert_eq!(lookup(&info, "realloc calls"), Some(InfoValue::U64(0)));
        assert_eq!(lookup(&info, "mseg carriers"), Some(InfoValue::U64(1)));
        assert_eq!(lookup(&info, "mseg carriers size"), Some(InfoValue::Usize(1 << 20)));
        assert_eq!(lookup(&info, "mseg cache_hits"), Some(InfoValue::U64(1)));
        assert_eq!(lookup(&info, "mseg unmaps"), Some(InfoValue::U64(1)));
        // No type items until the caller supplies a name.
        assert_eq!(lookup(&info, "type"), None);
    }

    #[test]
    fn the_report_defaults_to_the_zero_snapshot() {
        let info = AllocatorInfo::from_stats("binary_alloc", &AllocStats::default());
        assert_eq!(info.name, "binary_alloc");
        assert_eq!(info.items.len(), 13);
        assert!(info
            .items
            .iter()
            .all(|i| i.value == InfoValue::U64(0) || i.value == InfoValue::Usize(0)));
    }

    #[test]
    fn push_type_carries_the_caller_supplied_name_and_counters() {
        let mut c = Counters::default();
        c.note_alloc_typed(7, 32);
        c.note_alloc_typed(7, 64);
        c.note_free_typed(7, 96);

        let stats = c.snapshot();
        let mut info = AllocatorInfo::from_stats("eheap_alloc", &stats);
        let before = info.items.len();
        info.push_type("eheap", stats.type_stats(7));

        assert_eq!(info.items.len(), before + 4);
        assert_eq!(
            &info.items[before..],
            &[
                AllocatorInfoItem { key: "type", value: InfoValue::Text("eheap") },
                AllocatorInfoItem { key: "type blocks", value: InfoValue::U64(1) },
                AllocatorInfoItem { key: "type bytes", value: InfoValue::Usize(0) },
                AllocatorInfoItem { key: "type peak_bytes", value: InfoValue::Usize(96) },
            ]
        );
        // The assembler needs no type-table knowledge: a foreign slot reads as zero.
        info.push_type("unknown", stats.type_stats(STATS_TYPE_SLOTS + 3));
        assert_eq!(info.items.last().map(|i| i.key), Some("type peak_bytes"));
        assert_eq!(
            info.items.iter().rev().nth(3).map(|i| i.value),
            Some(InfoValue::Text("unknown"))
        );
    }
}
