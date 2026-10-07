//! Allocation instrumentation — the allocator's observability layer.
//!
//! This is what `erts_alloc:info/0` / `erlang:system_info(allocator)` report on. It has
//! three parts:
//!
//! * [`Counters`] — the mutable, non-atomic bookkeeping an allocator updates while
//!   holding its lock (`pub(crate)`, driven by `allocator.rs`). It mirrors the legacy
//!   `Allctr_t::calls` / `CarriersStats_t` counters.
//! * [`AllocStats`] — the public, `Copy` point-in-time snapshot of [`Counters`],
//!   including the per-allocation-*type* table and the [`MsegStats`] block.
//! * [`AllocatorInfo`] / [`AllocatorInfoItem`] / [`InfoValue`] — the report data types
//!   and the assembler that turns an [`AllocStats`] into the item list of the
//!   `=allocator:<name>` section of the `erts_alloc:info/0` report, reusing the legacy
//!   item names. It assembles the counters ([`from_stats`](AllocatorInfo::from_stats)),
//!   the legacy `+M<alloc><opt>` policy items ([`push_policy`](AllocatorInfo::push_policy))
//!   and the per-type rows ([`push_type`](AllocatorInfo::push_type)).
//!
//! Ground truth (read-only):
//! `erts_alcu_info` / `info_carriers` / `info_calls` / `info_options` in
//! `../../erts/emulator/beam/erl_alloc_util.c`, `erts_allocator_info` in
//! `../../erts/emulator/beam/erl_alloc.c` (the `=allocator:<name>` report shape) and
//! `erts_mseg_info` / `info_status` in
//! `../../erts/emulator/sys/common/erl_mseg.c` (the mseg counter names).
//!
//! # Isolation
//!
//! This module is deliberately close to self-contained and references no *allocator*
//! internals: no carriers, no blocks, no free lists, no `registry`. The per-type table
//! is addressed by a plain `usize` slot index and the type *name* is supplied by the
//! caller as a `&'static str`, so this layer needs no knowledge of the type inventory.
//!
//! Two references outside the module are deliberate and narrow, because the legacy
//! reports both of them through this same surface:
//!
//! * [`crate::carrier::mseg`] — the [`MsegStats`] block is a live view of the
//!   **process-global** segment allocator's counters (see [`MsegStats`]);
//! * [`crate::policy::CarrierPolicy`] — the `option <opt>: <value>` items of the
//!   `=allocator:<name>` report (`info_options`, `erl_alloc_util.c:5479-5640`).
//!
//! Safe Rust only — no `unsafe`.

use crate::mseg::{MsegCalls, MsegInfo};
use crate::policy::{CarrierPolicy, PolicyOptionKind, PolicyUnit};
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

/// The process-global segment-allocator (`mseg`) counters, as they stand when the
/// enclosing snapshot was taken.
///
/// # Where these numbers come from
///
/// The legacy `mseg_alloc` counters live in the *segment* allocator, not in the carrier
/// allocator that asks it for memory: they are process-global, and the Rust rewrite
/// keeps that split. The values here are therefore a live view of
/// [`crate::carrier::mseg`] — the one segment allocator this process has — taken when
/// [`Counters::snapshot`] runs, **not** per-allocator counters: two allocators report
/// the same `mseg` block, and it moves whenever *any* allocator maps or releases a
/// carrier. (The legacy keeps one segment allocator per scheduler instance and reports
/// them as separate `=allocator:mseg_alloc[i]` sections; this crate has a single shared
/// one, so this block reports that one pool.)
///
/// Every field maps 1:1 onto [`crate::mseg::MsegInfo`], which stays the source of truth
/// for the meanings:
///
/// | `MsegStats` | `MsegInfo` | legacy name (`erts_mseg_info`) |
/// |---|---|---|
/// | [`segments`](MsegStats::segments) | `segments` | `segments: <current>` |
/// | [`peak_segments`](MsegStats::peak_segments) | `watermark` | `segments_watermark` |
/// | [`bytes`](MsegStats::bytes) | `bytes` | `segments_size: <current>` |
/// | [`peak_bytes`](MsegStats::peak_bytes) | `max_bytes_ever` | `segments_size: <max_ever>` |
/// | [`cached`](MsegStats::cached) | `cached_segments` | `cached_segments` |
/// | [`reused`](MsegStats::reused) | `cache_hits` | `cache_hits` |
/// | [`mapped_segments`](MsegStats::mapped_segments) | `mapped_segments` | `erts_mseg_no()` |
/// | [`mapped_bytes`](MsegStats::mapped_bytes) | `mapped_bytes` | mmap footprint |
/// | [`peak_mapped_bytes`](MsegStats::peak_mapped_bytes) | `peak_mapped_bytes` | footprint high-water mark |
/// | [`unmaps`](MsegStats::unmaps) | `calls.destroy` | the `mseg_destroy` call count |
/// | [`calls`](MsegStats::calls) | `calls` | the `ErtsMsegCalls` block |
///
/// The two `MsegInfo` fields with no counterpart here are its *period* maxima
/// (`max_segments`, `max_bytes`): [`crate::mseg::MsegInfo`] folds them into its monotone
/// `watermark`/`max_bytes_ever` on every snapshot, and a `MsegStats` **is** such a
/// snapshot, so carrying both would report one quantity twice.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MsegStats {
    /// Segments currently handed out by the segment allocator (`segments`).
    pub segments: u64,
    /// Monotone high-water mark of [`segments`](MsegStats::segments)
    /// (`segments_watermark`).
    pub peak_segments: u64,
    /// Bytes currently handed out in those segments (`segments_size`).
    pub bytes: usize,
    /// Monotone high-water mark of [`bytes`](MsegStats::bytes)
    /// (`segments_size: <max_ever>`).
    pub peak_bytes: usize,
    /// Segments currently held in the segment allocator's reuse cache
    /// (`cached_segments`).
    pub cached: u64,
    /// Segments served from that reuse cache (`cache_hits`).
    pub reused: u64,
    /// Segments currently mapped from the OS: handed out plus cached.
    pub mapped_segments: u64,
    /// Bytes of address space currently mapped for those segments.
    pub mapped_bytes: usize,
    /// High-water mark of [`mapped_bytes`](MsegStats::mapped_bytes).
    pub peak_mapped_bytes: usize,
    /// Segments returned to the OS with `munmap` (the `mseg_destroy` call count).
    pub unmaps: u64,
    /// The segment allocator's call counters ([`crate::mseg::MsegCalls`]).
    pub calls: MsegCalls,
}

impl From<MsegInfo> for MsegStats {
    fn from(info: MsegInfo) -> MsegStats {
        MsegStats {
            segments: info.segments,
            peak_segments: info.watermark,
            bytes: info.bytes,
            peak_bytes: info.max_bytes_ever,
            cached: info.cached_segments,
            reused: info.cache_hits,
            mapped_segments: info.mapped_segments,
            mapped_bytes: info.mapped_bytes,
            peak_mapped_bytes: info.peak_mapped_bytes,
            unmaps: info.calls.destroy,
            calls: info.calls,
        }
    }
}

/// A live snapshot of the process-global segment allocator (`crate::carrier::mseg`), in
/// the [`MsegStats`] shape — what [`Counters::snapshot`] embeds as [`AllocStats::mseg`].
fn mseg_snapshot() -> MsegStats {
    crate::carrier::mseg().stats().into()
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
    /// The **process-global** segment-allocator counters as of this snapshot (see
    /// [`MsegStats`]) — shared with every other allocator in the process, not this
    /// allocator's own view.
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
    // The methods below are the *typed* (per-type / per-carrier-class) variants of the
    // aggregate counters above. Each records BOTH the class breakdown and the aggregate,
    // so a caller records an event exactly once: use either the typed method or the
    // untyped aggregate method for a given event, never both. They are added here ahead
    // of the allocator wiring that will call them (hence `allow(dead_code)`).
    //
    // There is deliberately no `note_mseg_*` family: the segment allocator keeps its own
    // counters ([`crate::mseg`]) and the snapshot reads them from there, so a local copy
    // could only ever drift (it used to be dead plumbing that never moved).

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

    /// Freezes the counters into a public snapshot.
    ///
    /// The [`AllocStats::mseg`] block is the exception to "this allocator's counters":
    /// the segment allocator is process-global, so the snapshot embeds a *live* view of
    /// it ([`mseg_snapshot`]) rather than a copy of anything local — see [`MsegStats`].
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
            mseg: mseg_snapshot(),
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
    /// A 64-bit scalar: a call counter, or a policy value that is not a byte size (a
    /// percentage or an amount — see [`AllocatorInfo::push_policy`]).
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
/// [`push_policy`](AllocatorInfo::push_policy) / [`push_type`](AllocatorInfo::push_type))
/// needs no knowledge of the allocation-type inventory: the type name is always
/// supplied by the caller.
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

/// The order [`AllocatorInfo::push_policy`] reports the `+M<alloc><opt>` items in: the
/// order the oracle (`erlang:system_info({allocator, A})`, OTP 29 / erts-17.1) prints the
/// `options` proplist in, restricted to the twelve options [`CarrierPolicy`] models.
///
/// The C assembles that proplist by **prepending** each item (`add_2tup` in
/// `info_options`, `erl_alloc_util.c:5595-5638`), so the reported order is the *reverse*
/// of the C statement order — which is why `mmmbc` precedes `mmsbc`, `acful` precedes
/// `acul`, and `lmbcs` precedes `smbcs` here. The full oracle order, with the items
/// `policy.rs` deliberately does not model bracketed, is
///
/// ```text
///   [e t atags ramv] sbct asbcst rsbcst rsbcmt [rmbcmt] [mmbcs] mmmbc mmsbc
///   lmbcs smbcs [mbcgs] acful acul acnl acfml [cp]
/// ```
///
/// The bracketed items are omitted rather than reported with invented values: `e`/`t`/
/// `atags`/`ramv`/`mbcgs` are not modelled by [`CarrierPolicy`] at all, `mmbcs` is
/// [`AllocatorConfig::carrier_size`](crate::allocator::AllocatorConfig::carrier_size)
/// (owned by the allocator seam) and `cp` is the carrier-pool identity of the *calling*
/// allocator (this crate has no carrier pool).
const POLICY_REPORT_ORDER: [PolicyOptionKind; 12] = [
    PolicyOptionKind::Sbct,
    PolicyOptionKind::Asbcst,
    PolicyOptionKind::Rsbcst,
    PolicyOptionKind::Rsbcmt,
    PolicyOptionKind::Mmmbc,
    PolicyOptionKind::Mmsbc,
    PolicyOptionKind::Lmbcs,
    PolicyOptionKind::Smbcs,
    PolicyOptionKind::Acful,
    PolicyOptionKind::Acul,
    PolicyOptionKind::Acnl,
    PolicyOptionKind::Acfml,
];

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
    ///
    /// The four `mseg ...` items are the process-global segment-allocator counters of the
    /// enclosing snapshot (see [`MsegStats`]), not this allocator's own. The policy items
    /// (`option sbct: ...`) and the per-type rows are **not** part of this set: they come
    /// from [`push_policy`](AllocatorInfo::push_policy) and
    /// [`push_type`](AllocatorInfo::push_type) respectively.
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

    /// Appends the legacy `+M<alloc><opt>` carrier-policy items for `policy` — the
    /// `option <opt>: <value>` lines of the text report, i.e. the `options` proplist of
    /// `erlang:system_info({allocator, A})` (`info_options`, `erl_alloc_util.c:5479-5640`).
    ///
    /// One item per option of [`CarrierPolicy`], keyed by the option's legacy name
    /// ([`PolicyOptionKind::option_name`], e.g. `"sbct"`) and emitted in the oracle's
    /// order ([`POLICY_REPORT_ORDER`]). Values are reported in the unit the policy
    /// stores them in, which is the unit the oracle prints:
    ///
    /// * a **byte**-valued option (`sbct`, `asbcst`, `smbcs`, `lmbcs`, `acfml`) as
    ///   [`InfoValue::Usize`] — bytes, even though the CLI gives the first four in
    ///   kilobytes (`PolicyUnit::Kilobytes` is a CLI unit, the field is bytes);
    /// * a **percentage** (`rsbcst`, `rsbcmt`, `acul`, `acful`) or an **amount**
    ///   (`mmsbc`, `mmmbc`, `acnl`) as [`InfoValue::U64`] — a scalar, not a byte size.
    ///
    /// # Configured versus honoured
    ///
    /// Like the legacy, this reports the allocator's **configured** policy. Two groups of
    /// items must not be read as a description of what this implementation *does*:
    ///
    /// | option | this allocator's runtime behaviour |
    /// |---|---|
    /// | `sbct` | honoured — a request at or above it gets a dedicated single-block carrier (`allocator.rs`'s `alloc_impl`) |
    /// | `smbcs`, `lmbcs` | honoured — they clamp the preferred variable-size carrier size (`allocator.rs`'s `mbc_size`) |
    /// | `mmmbc` | **partially honoured** — it caps the pool of variable-size multi-block carriers (`alloc_mid`); the slab carriers are *not* counted against it and the legacy's `sys_alloc` fallback is not modelled |
    /// | `mmsbc` | **reported as configured only** — this allocator's dedicated carriers are not counted against the cap (the process-global segment allocator applies its own `MsegConfig` defaults, which the policy does not feed) |
    /// | `asbcst`, `rsbcst` | **reported as configured only** — a dedicated carrier is released whole on free, never shrunk |
    /// | `rsbcmt` | **reported as configured only** — a shrunk block never moves from a single-block to a multi-block carrier |
    /// | `acul`, `acful`, `acnl`, `acfml` | **reported as configured only** — there is no carrier pool and no carrier migration (a documented M1 gap) |
    pub fn push_policy(&mut self, policy: &CarrierPolicy) {
        for kind in POLICY_REPORT_ORDER {
            let value = match kind.unit() {
                PolicyUnit::Kilobytes | PolicyUnit::Bytes => InfoValue::Usize(policy.get(kind)),
                PolicyUnit::Percent | PolicyUnit::Amount => InfoValue::U64(policy.get(kind) as u64),
            };
            self.items.push(item(kind.option_name(), value));
        }
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
    fn a_default_snapshot_has_zero_counters_and_the_live_process_global_mseg_view() {
        // `AllocStats::default()` is the all-zero snapshot, `mseg` block included: it is
        // what a value-shaped consumer (and `AllocStats`' hand-written `Default`) uses.
        let stats = AllocStats::default();
        assert_eq!(stats.alloc_calls, 0);
        assert_eq!(stats.live_bytes, 0);
        assert_eq!(stats.carriers, 0);
        assert_eq!(stats.sbc_carriers, 0);
        assert_eq!(stats.mbc_carriers, 0);
        assert_eq!(stats.mseg, MsegStats::default());
        assert!(stats.types.iter().all(|t| *t == TypeStats::default()));

        // `Counters::default().snapshot()` is the same, with exactly one documented
        // difference: the `mseg` block is a live view of the **process-global** segment
        // allocator, so it is whatever the other tests in this binary have left there.
        // Copy that value into the expected struct instead of comparing two independent
        // snapshots — a second `snapshot()` takes a *new* global reading, which other
        // tests running in parallel may well have moved.
        let live = Counters::default().snapshot();
        let expected = AllocStats {
            mseg: live.mseg,
            ..AllocStats::default()
        };
        assert_eq!(live, expected);
    }

    #[test]
    fn the_mseg_block_is_a_live_view_of_the_process_global_segment_allocator() {
        // This test never allocates, so it perturbs nothing; it only pins the *shape* of
        // the relation between a snapshot and the segment allocator. The mseg counters
        // are process-global and shared with every test in this binary (the harness runs
        // them in parallel), so two independent global readings must never be compared
        // for equality — the monotone counters are *bracketed* between two readings taken
        // around the snapshot instead.
        let before = crate::carrier::mseg().stats();
        let snapshot = Counters::default().snapshot().mseg;
        let after = crate::carrier::mseg().stats();

        // `MsegStats` field <- `MsegInfo` field, for the monotone ones: a counter that
        // only grows is never below a reading taken before it nor above one taken after.
        assert!(before.watermark <= snapshot.peak_segments);
        assert!(snapshot.peak_segments <= after.watermark);
        assert!(before.max_bytes_ever <= snapshot.peak_bytes);
        assert!(snapshot.peak_bytes <= after.max_bytes_ever);
        assert!(before.peak_mapped_bytes <= snapshot.peak_mapped_bytes);
        assert!(snapshot.peak_mapped_bytes <= after.peak_mapped_bytes);
        assert!(before.cache_hits <= snapshot.reused);
        assert!(snapshot.reused <= after.cache_hits);
        assert!(before.calls.destroy <= snapshot.unmaps);
        assert!(snapshot.unmaps <= after.calls.destroy);
        assert!(before.calls.create <= snapshot.calls.create);
        assert!(snapshot.calls.create <= after.calls.create);
        assert!(before.calls.alloc <= snapshot.calls.alloc);
        assert!(snapshot.calls.alloc <= after.calls.alloc);
        // The live (non-monotone) counters have only a lower bound of zero; their exact
        // value is whatever the other tests in this binary happen to be doing.
        let live = (snapshot.segments, snapshot.bytes, snapshot.cached, snapshot.mapped_segments);
        let _ = live;
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
    fn a_snapshot_round_trips_every_counter_it_owns() {
        let mut c = Counters::default();
        c.note_alloc(8);
        c.note_alloc_typed(11, 24);
        c.note_free(8);
        c.note_free_typed(11, 4);
        c.note_realloc();
        c.note_mbc_carrier_up(512);
        c.note_sbc_carrier_up(256);
        c.note_mbc_carrier_down(512);

        let mut s1 = c.snapshot();
        let s2 = c.snapshot();
        // Every counter but one is this allocator's own, so two snapshots of an
        // unchanged `Counters` are equal. The `mseg` block is the exception: each
        // snapshot reads the **process-global** segment allocator afresh, and the other
        // tests in this binary move it. Copy `s2`'s reading into `s1` so the comparison
        // covers everything this type actually owns.
        s1.mseg = s2.mseg;
        assert_eq!(s1, s2, "snapshots are deterministic outside the mseg block");
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
        let stats = c.snapshot();
        let info = AllocatorInfo::from_stats("eheap_alloc", &stats);

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
        // The four mseg items report the *process-global* segment allocator, so they are
        // compared against the very snapshot they were built from (not against a second,
        // independently taken global reading — other tests in this binary move it).
        assert_eq!(lookup(&info, "mseg carriers"), Some(InfoValue::U64(stats.mseg.segments)));
        assert_eq!(
            lookup(&info, "mseg carriers size"),
            Some(InfoValue::Usize(stats.mseg.bytes))
        );
        assert_eq!(lookup(&info, "mseg cache_hits"), Some(InfoValue::U64(stats.mseg.reused)));
        assert_eq!(lookup(&info, "mseg unmaps"), Some(InfoValue::U64(stats.mseg.unmaps)));
        // No option or type items until the caller supplies them.
        assert_eq!(lookup(&info, "type"), None);
        assert_eq!(lookup(&info, "sbct"), None);
    }

    #[test]
    fn push_policy_reports_every_option_in_the_oracles_order_and_unit() {
        // A policy with a distinct, non-default value for every option; `get(kind)` is the
        // single source of truth for what the report owes, so nothing here hard-codes a
        // default that a later change to `CarrierPolicy` could move.
        let policy = CarrierPolicy::default()
            .with_option("sbct", 256)
            .expect("sbct in KiB")
            .with_option("asbcst", 1024)
            .expect("asbcst in KiB")
            .with_option("rsbcst", 11)
            .expect("a percentage")
            .with_option("rsbcmt", 22)
            .expect("a percentage")
            .with_option("mmmbc", 33)
            .expect("an amount")
            .with_option("mmsbc", 44)
            .expect("an amount")
            .with_option("lmbcs", 5 * 1024)
            .expect("lmbcs in KiB")
            .with_option("smbcs", 64)
            .expect("smbcs in KiB")
            .with_option("acful", 55)
            .expect("a percentage")
            .with_option("acul", 66)
            .expect("a percentage")
            .with_option("acnl", 77)
            .expect("an amount")
            .with_option("acfml", 88)
            .expect("bytes");

        let mut info = AllocatorInfo::from_stats("test_alloc", &AllocStats::default());
        let counters = info.items.len();
        info.push_policy(&policy);

        // The order the oracle prints (`erlang:system_info({allocator, test_alloc})`, OTP
        // 29 / erts-17.1): `mmmbc` before `mmsbc`, `acful` before `acul`, `lmbcs` before
        // `smbcs` — the reverse of the C's prepending `add_2tup` sequence.
        let keys: Vec<&str> = info.items[counters..].iter().map(|i| i.key).collect();
        assert_eq!(
            keys,
            vec![
                "sbct", "asbcst", "rsbcst", "rsbcmt", "mmmbc", "mmsbc", "lmbcs", "smbcs", "acful",
                "acul", "acnl", "acfml",
            ]
        );

        // Every option of the policy is reported exactly once, as its own value.
        let mut expected = Vec::new();
        for kind in PolicyOptionKind::ALL {
            let value = match kind.unit() {
                PolicyUnit::Kilobytes | PolicyUnit::Bytes => InfoValue::Usize(policy.get(kind)),
                PolicyUnit::Percent | PolicyUnit::Amount => InfoValue::U64(policy.get(kind) as u64),
            };
            assert_eq!(lookup(&info, kind.option_name()), Some(value), "{kind:?}");
            expected.push((kind.option_name(), value));
        }
        assert_eq!(info.items[counters..].len(), expected.len());

        // The units are the oracle's: bytes for a byte-valued option, the bare number for
        // a percentage or an amount.
        assert_eq!(lookup(&info, "sbct"), Some(InfoValue::Usize(256 * 1024)));
        assert_eq!(lookup(&info, "asbcst"), Some(InfoValue::Usize(1024 * 1024)));
        assert_eq!(lookup(&info, "smbcs"), Some(InfoValue::Usize(64 * 1024)));
        assert_eq!(lookup(&info, "lmbcs"), Some(InfoValue::Usize(5 * 1024 * 1024)));
        assert_eq!(lookup(&info, "acfml"), Some(InfoValue::Usize(88)));
        assert_eq!(lookup(&info, "rsbcst"), Some(InfoValue::U64(11)));
        assert_eq!(lookup(&info, "acul"), Some(InfoValue::U64(66)));
        assert_eq!(lookup(&info, "mmsbc"), Some(InfoValue::U64(44)));
        assert_eq!(lookup(&info, "acnl"), Some(InfoValue::U64(77)));
        // ... and they are the *configured* values, not the defaults.
        assert_ne!(
            lookup(&info, "sbct"),
            Some(InfoValue::Usize(CarrierPolicy::default().sbct))
        );
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
