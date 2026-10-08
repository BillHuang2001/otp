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
mod tests;
