//! The `mseg` allocator: the mmap-backed memory-segment manager.
//!
//! `mseg` sits *below* the carrier allocator: it hands out page-granular memory
//! segments obtained straight from the operating system, takes them back when they
//! are no longer needed and keeps a few of them in a small **reuse cache** so that a
//! churn of carriers does not turn into a churn of `mmap`/`munmap` syscalls. It is
//! the Rust counterpart of the legacy segment layer
//! `../../erts/emulator/sys/common/erl_mseg.c` together with the OS half of
//! `../../erts/emulator/sys/common/erl_mmap.c` (`os_mmap` / `os_munmap` /
//! `os_mmap_aligned`). The legacy *super carrier* machinery (a large pre-reserved
//! virtual range with a free-segment descriptor map) is deliberately not modelled:
//! every segment is one plain anonymous mapping, which is what the legacy falls back
//! to whenever no super carrier is configured.
//!
//! # What a segment is
//!
//! A segment is one `mmap`-ed range whose base is [`MSEG_HEADER_SIZE`] bytes in front
//! of the *payload* pointer handed to the caller. The payload is aligned to the
//! alignment the caller asked for. Requests at or below the page size are satisfied
//! directly by `mmap` (whose base is page-aligned anyway); larger — *super-aligned* —
//! requests (`ERTS_HAVE_MSEG_SUPER_ALIGNED` / `ERTS_SUPER_ALIGN_BITS` in the legacy)
//! are satisfied by over-mapping and trimming the leading and trailing slack, exactly
//! like the legacy `os_mmap_aligned()`. After trimming, the retained mapping starts at
//! a page boundary at most one page (plus the header) in front of the payload and ends
//! at a page boundary at most one page past the last usable byte.
//!
//! # Segment header
//!
//! The legacy caller keeps the segment size itself and passes it back to
//! `erts_mseg_dealloc`. This implementation additionally records a small control
//! header in the [`MSEG_HEADER_SIZE`] bytes immediately *before* the payload, so a
//! segment can be described — and released — from a bare payload pointer
//! ([`MsegAllocator::free_ptr`], [`MsegAllocator::from_raw`]). The layout is private
//! to this module (it never crosses a C ABI or the ETF wire); it is
//! `#[repr(C)]` and is written with unaligned stores, so the header address itself
//! carries no alignment requirement.
//!
//! # Errors and `unsafe`
//!
//! Every failure is a typed `beam_common::{Result, BeamError}`: a bad alignment is
//! [`BeamError::BadArg`], an exhausted address space or an exceeded `+MM` limit is
//! `BeamError::Alloc(AllocFailure { kind: Carrier, .. })`, and a failed `munmap` while
//! a live segment shrinks is [`BeamError::Io`]. A `munmap` that fails while a segment
//! is being handed back cannot be reported to anyone — the caller has already given the
//! segment up — so it is logged and the counters are kept consistent. Nothing panics
//! on runtime data.
//!
//! `unsafe` is confined to this module's mapping primitives ([`os_map`] and
//! [`os_unmap`], plus the header stores/loads and the bare-pointer entry points);
//! every `unsafe fn` carries a `# Safety` section and every `unsafe` block a
//! preceding `// SAFETY:` rationale (`../beam_common/src/safety.rs` convention).

use core::ffi::c_void;
use core::ptr;
use core::slice;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use parking_lot::Mutex;

use crate::carrier::os_page_size;
use crate::error;

/// The version string the legacy segment allocator reports (`ERTS_MSEG_VSN_STR`).
pub const MSEG_VERSION: &str = "0.9";

/// The legacy super-alignment granularity in bits (`ERTS_SUPER_ALIGN_BITS`,
/// which is `ERTS_MMAP_SUPERALIGNED_BITS`: 14 on 64-bit hosts, 18 on 32-bit).
pub const SUPER_ALIGN_BITS: u32 = if usize::BITS == 64 { 14 } else { 18 };

/// The legacy super-alignment granularity in bytes (`ERTS_SUPERALIGNED_SIZE`).
pub const SUPER_ALIGNED_SIZE: usize = 1 << SUPER_ALIGN_BITS;

/// The number of bytes reserved in front of every segment payload for the segment
/// control header ([`MsegHeader`]).
///
/// It is a multiple of 8 (so a header placed below a payload is word-aligned) and
/// large enough for the whole header struct.
pub const MSEG_HEADER_SIZE: usize = 64;

/// The largest number of segments the reuse cache may hold (`MAX_CACHE_SIZE` in the
/// legacy; a larger `mcs` is clamped to it exactly like `erts_mseg_init` does).
pub const MAX_CACHE_SIZE: usize = 30;

/// The alignment a cached/fresh segment of a given size always has.
///
/// Mirrors the legacy `ERTS_IS_SUPERALIGNED(X)` predicate: an address that is a whole
/// multiple of [`SUPER_ALIGNED_SIZE`].
pub fn is_super_aligned(address: usize) -> bool {
    address % SUPER_ALIGNED_SIZE == 0
}

/// The OS page size, cached (shared with the carrier layer's `mmap` handling).
pub fn page_size() -> usize {
    os_page_size()
}

/// Rounds `value` up to the next multiple of `align` (`align` must be a power of two);
/// `None` on overflow.
///
/// The caller has to turn `None` into a typed error: a request whose page-rounded
/// extent overflows `usize` cannot be mapped.
fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

/// The kind of carrier a segment was obtained for.
///
/// The legacy limits `mmsbc` / `mmmbc` (`+M<alloc>mmsbc` / `+M<alloc>mmmbc`) are
/// enforced in the carrier layer (`erl_alloc_util.c`), where a *single-block carrier*
/// is a carrier holding exactly one block and a *multi-block carrier* one holding
/// several. Here the same limits are modelled at the segment layer — the layer that
/// owns the resource they bound — and the caller declares which kind it is obtaining.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SegmentKind {
    /// A segment for a carrier that will hold a single block (`sbc`).
    SingleBlock,
    /// A segment for a carrier that will be subdivided into several blocks (`mbc`).
    MultiBlock,
}

impl SegmentKind {
    /// The legacy mnemonic: `"sbc"` or `"mbc"`.
    pub fn as_str(&self) -> &'static str {
        match self {
            SegmentKind::SingleBlock => "sbc",
            SegmentKind::MultiBlock => "mbc",
        }
    }

    fn to_header(self) -> u64 {
        match self {
            SegmentKind::MultiBlock => 0,
            SegmentKind::SingleBlock => 1,
        }
    }

    fn from_header(value: u64) -> Option<SegmentKind> {
        match value {
            0 => Some(SegmentKind::MultiBlock),
            1 => Some(SegmentKind::SingleBlock),
            _ => None,
        }
    }
}

/// Configuration of a [`MsegAllocator`], with the legacy `+MM` / `+M` defaults.
///
/// Every field names the legacy option it mirrors; the defaults are the ones
/// `ERTS_MSEG_INIT_DEFAULT_INITIALIZER` (`erl_mseg.h`), `ERTS_DEFAULT_ALCU_INIT` /
/// `ERTS_DEFAULT_ALLCTR_INIT` (`erl_alloc_util.h`) and `erts_mseg_default_opt`
/// (`erl_mseg.c`) install.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MsegConfig {
    /// Maximum number of segments mapped from the OS at any one time, cached segments
    /// included (`+Mummc`, `AllcUInit_t::mmc`; the legacy default is `~0`, i.e.
    /// unlimited).
    pub max_carriers: usize,
    /// Maximum number of segments mapped for single-block carriers
    /// (`+M<alloc>mmsbc`, `AllctrInit_t::mmsbc`; legacy default 256).
    pub max_sbc_carriers: usize,
    /// Maximum number of segments mapped for multi-block carriers
    /// (`+M<alloc>mmmbc`, `AllctrInit_t::mmmbc`; legacy default `~0`).
    pub max_mbc_carriers: usize,
    /// Whether released segments are kept in the reuse cache
    /// (`ErtsMsegOpt_t::cache`; legacy default `true`).
    pub cache: bool,
    /// Maximum number of segments held in the reuse cache (`+MMmcs`,
    /// `ErtsMsegInit_t::mcs` / `MAX_CACHE_SIZE`; legacy default 10, clamped to
    /// [`MAX_CACHE_SIZE`]).
    pub max_cache_size: usize,
    /// Absolute "cache bad fit" bound in bytes (`+MMamcbf`, `ErtsMsegInit_t::amcbf`;
    /// legacy default 4 MiB).
    pub abs_max_cache_bad_fit: usize,
    /// Relative "cache bad fit" bound in percent (`+MMrmcbf`, `ErtsMsegInit_t::rmcbf`;
    /// legacy default 20).
    pub rel_max_cache_bad_fit: usize,
    /// Absolute shrink threshold in bytes (`ErtsMsegOpt_t::abs_shrink_th`,
    /// `+M<alloc>rsbcst`; legacy default 0).
    pub abs_shrink_threshold: usize,
    /// Relative shrink threshold in percent (`ErtsMsegOpt_t::rel_shrink_th`,
    /// `+M<alloc>rsbcst`; legacy default 0).
    pub rel_shrink_threshold: usize,
}

impl Default for MsegConfig {
    fn default() -> MsegConfig {
        MsegConfig {
            max_carriers: usize::MAX,
            max_sbc_carriers: 256,
            max_mbc_carriers: usize::MAX,
            cache: true,
            max_cache_size: 10,
            abs_max_cache_bad_fit: 4 * 1024 * 1024,
            rel_max_cache_bad_fit: 20,
            abs_shrink_threshold: 0,
            rel_shrink_threshold: 0,
        }
    }
}

impl MsegConfig {
    /// Starts building a configuration from the legacy defaults.
    pub fn builder() -> MsegConfigBuilder {
        MsegConfigBuilder {
            config: MsegConfig::default(),
        }
    }

    /// Applies the normalisations `erts_mseg_init` applies: `mcs` is clamped to
    /// [`MAX_CACHE_SIZE`].
    fn normalised(mut self) -> MsegConfig {
        if self.max_cache_size > MAX_CACHE_SIZE {
            self.max_cache_size = MAX_CACHE_SIZE;
        }
        self
    }
}

/// Builder for [`MsegConfig`]; obtain it from [`MsegConfig::builder`].
#[derive(Clone, Debug)]
pub struct MsegConfigBuilder {
    config: MsegConfig,
}

impl MsegConfigBuilder {
    /// Sets [`MsegConfig::max_carriers`].
    pub fn max_carriers(mut self, segments: usize) -> Self {
        self.config.max_carriers = segments;
        self
    }

    /// Sets [`MsegConfig::max_sbc_carriers`].
    pub fn max_sbc_carriers(mut self, segments: usize) -> Self {
        self.config.max_sbc_carriers = segments;
        self
    }

    /// Sets [`MsegConfig::max_mbc_carriers`].
    pub fn max_mbc_carriers(mut self, segments: usize) -> Self {
        self.config.max_mbc_carriers = segments;
        self
    }

    /// Enables or disables the reuse cache (see [`MsegConfig::cache`]).
    pub fn cache(mut self, enabled: bool) -> Self {
        self.config.cache = enabled;
        self
    }

    /// Sets [`MsegConfig::max_cache_size`].
    pub fn max_cache_size(mut self, segments: usize) -> Self {
        self.config.max_cache_size = segments;
        self
    }

    /// Sets the absolute and relative "cache bad fit" bounds
    /// ([`MsegConfig::abs_max_cache_bad_fit`], [`MsegConfig::rel_max_cache_bad_fit`]).
    pub fn bad_fit_thresholds(mut self, absolute: usize, relative: usize) -> Self {
        self.config.abs_max_cache_bad_fit = absolute;
        self.config.rel_max_cache_bad_fit = relative;
        self
    }

    /// Sets the absolute and relative shrink thresholds
    /// ([`MsegConfig::abs_shrink_threshold`], [`MsegConfig::rel_shrink_threshold`]).
    pub fn shrink_thresholds(mut self, absolute: usize, relative: usize) -> Self {
        self.config.abs_shrink_threshold = absolute;
        self.config.rel_shrink_threshold = relative;
        self
    }

    /// Finishes the configuration.
    pub fn build(self) -> MsegConfig {
        self.config
    }
}

/// The segment call counters, named after the legacy `ErtsMsegCalls` block
/// (`erl_mseg.c`) and reported by `erts_mseg:info/0` / `erlang:system_info(allocator)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsegCalls {
    /// `mseg_alloc` calls (cache hits included).
    pub alloc: u64,
    /// `mseg_dealloc` calls.
    pub dealloc: u64,
    /// `mseg_realloc` calls.
    pub realloc: u64,
    /// Segments obtained from the OS with `mmap` (`mseg_create`).
    pub create: u64,
    /// Segments returned to the OS with `munmap` (`mseg_destroy`).
    pub destroy: u64,
    /// Reallocations that had to map a fresh segment and copy (`mseg_recreate`).
    pub recreate: u64,
    /// `clear_cache` calls.
    pub clear_cache: u64,
    /// Cache checks that dropped a segment (`mseg_check_cache`).
    pub check_cache: u64,
}

/// A point-in-time snapshot of a [`MsegAllocator`]'s counters.
///
/// The field set mirrors the `status` block of the legacy `erts_mseg_info`
/// (`segments`, `segments_size`, `segments_watermark`, `cached_segments`,
/// `cache_hits`) and adds the address-space footprint (`mapped_segments`,
/// `mapped_bytes`, `peak_mapped_bytes`), which the legacy reports through the
/// separate `erts_mmap_info()` call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsegInfo {
    /// Segments currently handed out (`segments: <current>`).
    pub segments: u64,
    /// High-water mark of [`segments`](MsegInfo::segments) within the current period
    /// (`segments: <max>`).
    pub max_segments: u64,
    /// High-water mark of [`segments`](MsegInfo::segments) since the allocator was
    /// created (`segments: <max_ever>`).
    pub max_segments_ever: u64,
    /// Bytes currently handed out (`segments_size: <current>`).
    pub bytes: usize,
    /// High-water mark of [`bytes`](MsegInfo::bytes) within the current period
    /// (`segments_size: <max>`).
    pub max_bytes: usize,
    /// High-water mark of [`bytes`](MsegInfo::bytes) since the allocator was created
    /// (`segments_size: <max_ever>`).
    pub max_bytes_ever: usize,
    /// The monotone high-water mark of [`segments`](MsegInfo::segments)
    /// (`segments_watermark`).
    pub watermark: u64,
    /// Segments in the reuse cache (`cached_segments`).
    pub cached_segments: u64,
    /// Segments served from the reuse cache (`cache_hits`).
    pub cache_hits: u64,
    /// Segments currently mapped from the OS: handed-out plus cached.
    pub mapped_segments: u64,
    /// Bytes of address space currently mapped for those segments.
    pub mapped_bytes: usize,
    /// High-water mark of [`mapped_bytes`](MsegInfo::mapped_bytes).
    pub peak_mapped_bytes: usize,
    /// The call counters (`calls`).
    pub calls: MsegCalls,
}

/// The private control header stored immediately in front of every segment payload.
///
/// It records everything needed to release a segment from its payload pointer alone:
/// the retained mapping base/length, the payload, the usable size, the requested
/// alignment and the segment kind. The struct is smaller than [`MSEG_HEADER_SIZE`]
/// so the reserved area has room to grow.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MsegHeader {
    magic: u64,
    base: usize,
    map_len: usize,
    payload: usize,
    size: usize,
    align: usize,
    kind: u64,
}

/// The magic word of a segment header (`"MSEG\0\0\0|1"`); a mismatch means the pointer
/// handed to a bare-pointer entry point is not an mseg payload.
const MSEG_MAGIC: u64 = 0x4D53_4547_0000_0001;

// The header must fit in the space reserved for it in front of every payload.
const _: () = assert!(core::mem::size_of::<MsegHeader>() <= MSEG_HEADER_SIZE);

impl MsegHeader {
    fn new(entry: &CacheEntry) -> MsegHeader {
        MsegHeader {
            magic: MSEG_MAGIC,
            base: entry.base,
            map_len: entry.map_len,
            payload: entry.payload,
            size: entry.size,
            align: entry.align,
            kind: entry.kind.to_header(),
        }
    }

    /// Writes this header into the [`MSEG_HEADER_SIZE`] bytes in front of
    /// [`MsegHeader::payload`].
    fn store(self, payload: usize) {
        debug_assert_eq!(self.payload, payload);
        // SAFETY: `payload` is a live segment payload, so `payload - MSEG_HEADER_SIZE`
        // is inside that segment's retained mapping and is reserved for exactly this
        // header; the store is unaligned-safe and stays inside the reserved bytes.
        unsafe { ptr::write_unaligned((payload - MSEG_HEADER_SIZE) as *mut MsegHeader, self) };
    }

    /// Reads the header recorded in front of the payload at `payload`.
    ///
    /// # Safety
    ///
    /// `payload` must point at (or just past) a live mapping owned by a
    /// [`MsegAllocator`]; the header bytes are dereferenced.
    unsafe fn load(payload: usize) -> Result<MsegHeader> {
        // SAFETY: the caller guarantees `payload` points into a live mseg segment,
        // hence the header bytes in front of it are readable; the load is
        // unaligned-safe.
        let header = unsafe { ptr::read_unaligned((payload - MSEG_HEADER_SIZE) as *const MsegHeader) };
        if header.magic != MSEG_MAGIC || header.payload != payload {
            beam_common::beam_debug!("beam_alloc: mseg header magic mismatch at {payload:#x}");
            return Err(BeamError::BadArg);
        }
        Ok(header)
    }
}

/// One mapping held by the allocator: a live segment, or a cached one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CacheEntry {
    /// The retained mapping base (page-aligned); the address handed to `munmap`.
    base: usize,
    /// The retained mapping length; a multiple of the page size.
    map_len: usize,
    /// The aligned pointer handed to the caller.
    payload: usize,
    /// The usable byte count of the segment.
    size: usize,
    /// The alignment the caller requested.
    align: usize,
    /// The kind of carrier the segment was obtained for.
    kind: SegmentKind,
}

impl CacheEntry {
    /// Bytes of usable memory between the payload and the end of the mapping; always
    /// at least [`size`](CacheEntry::size).
    fn capacity(&self) -> usize {
        self.map_len - (self.payload - self.base)
    }

    /// The page-aligned tail of this mapping that lies past `size` usable bytes and
    /// can be returned to the OS, as `(address, length)`; `None` when nothing (a whole
    /// number of pages) can be trimmed.
    fn tail_trim(&self, size: usize) -> Option<(usize, usize)> {
        let usable_end = self.payload.checked_add(size)?;
        let keep_end = align_up(usable_end.checked_sub(self.base)?, page_size())?;
        if keep_end >= self.map_len {
            return None;
        }
        Some((self.base + keep_end, self.map_len - keep_end))
    }
}

/// A mapped memory segment: an owned, page-backed range of `size()` usable bytes.
///
/// Dropping the segment returns it to the allocator (and from there to the reuse
/// cache or to the OS), so a segment can never be leaked by an early return. The
/// segment is `Send`: it owns its mapping exclusively and the allocator serialises
/// cache/bookkeeping behind a mutex.
#[derive(Clone)]
pub struct Segment {
    state: Arc<MsegState>,
    entry: CacheEntry,
}

// The `Clone` above is deliberately not a public API: see `Segment::clone_for_check`.
impl core::fmt::Debug for Segment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Segment")
            .field("payload", &format_args!("{:#x}", self.entry.payload))
            .field("size", &self.entry.size)
            .field("capacity", &self.entry.capacity())
            .field("align", &self.entry.align)
            .field("kind", &self.entry.kind)
            .finish()
    }
}

impl Segment {
    fn new(state: Arc<MsegState>, entry: CacheEntry) -> Segment {
        Segment { state, entry }
    }

    /// The aligned start of the segment: the pointer the caller uses.
    pub fn as_ptr(&self) -> *mut u8 {
        self.entry.payload as *mut u8
    }

    /// The number of usable bytes in the segment (exactly what was requested, except
    /// that a request of 0 is reported as 0 while the mapping still has capacity).
    ///
    /// The legacy `erts_mseg_alloc` reports the *cached* segment's full size when it
    /// reuses one that fits "for free" (`*size_p = csize`); this module always reports
    /// the requested size and leaves the unused remainder visible through
    /// [`capacity`](Segment::capacity) — the caller decides how much of it to use.
    pub fn size(&self) -> usize {
        self.entry.size
    }

    /// The bytes actually available from [`as_ptr`](Segment::as_ptr) to the end of the
    /// retained mapping.
    ///
    /// It is at least [`size`](Segment::size) (page rounding keeps one partial page)
    /// and is what [`MsegAllocator::realloc`] can grow into without remapping.
    pub fn capacity(&self) -> usize {
        self.entry.capacity()
    }

    /// The alignment the segment's payload honours: [`as_ptr`](Segment::as_ptr) is a
    /// multiple of it.
    ///
    /// It is the alignment requested when the segment was obtained, so a segment that
    /// was mapped with a weaker alignment and reused for a stronger one reports the
    /// stronger one (the payload satisfies it — that is the reuse condition).
    pub fn align(&self) -> usize {
        self.entry.align
    }

    /// The kind of carrier this segment was obtained for (see [`SegmentKind`]).
    pub fn kind(&self) -> SegmentKind {
        self.entry.kind
    }

    /// The base address of the retained mapping that backs this segment.
    pub fn mapping_base(&self) -> usize {
        self.entry.base
    }

    /// The length in bytes of the retained mapping that backs this segment.
    pub fn mapping_len(&self) -> usize {
        self.entry.map_len
    }

    /// The number of bytes reserved in front of every payload for the segment header.
    pub const fn header_size() -> usize {
        MSEG_HEADER_SIZE
    }

    /// The first [`size`](Segment::size) bytes of the segment.
    ///
    /// The bytes are readable for the whole lifetime of the segment: `mmap` hands out
    /// zeroed pages and only this segment's owner can write them.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `entry.payload` starts a live mapping of at least `entry.capacity()`
        // bytes that this segment owns exclusively, and `entry.size <= capacity`, so
        // the described range is in-bounds and fully initialised (`mmap` zeroes it).
        unsafe { slice::from_raw_parts(self.entry.payload as *const u8, self.entry.size) }
    }

    /// The first [`size`](Segment::size) bytes of the segment, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as `as_slice`, and `&mut self` proves there is no other borrow of
        // the segment.
        unsafe { slice::from_raw_parts_mut(self.entry.payload as *mut u8, self.entry.size) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        self.state.inner.lock().release(self.entry);
    }
}

/// The shared state of a [`MsegAllocator`].
///
/// It is reference counted because every live [`Segment`] needs it to be able to
/// release itself: the last handle to go away releases the mappings still held in the
/// reuse cache, so dropping an allocator never leaks address space.
struct MsegState {
    inner: Mutex<MsegInner>,
}

impl Drop for MsegState {
    fn drop(&mut self) {
        // Exclusive access: the `Arc` count reached zero.
        self.inner.get_mut().purge_cache();
    }
}

/// The mutable bookkeeping of a [`MsegAllocator`], guarded by [`MsegState::inner`].
struct MsegInner {
    config: MsegConfig,
    /// Cached segments, oldest first.
    cache: Vec<CacheEntry>,
    /// Segments currently handed out.
    live: u64,
    live_bytes: usize,
    max_live: u64,
    max_live_ever: u64,
    max_bytes: usize,
    max_bytes_ever: usize,
    /// Monotone high-water mark of the handed-out segment count.
    watermark: u64,
    /// Segments currently mapped from the OS (handed-out plus cached).
    mapped: u64,
    mapped_bytes: usize,
    peak_mapped_bytes: usize,
    mapped_sbc: u64,
    mapped_mbc: u64,
    cache_hits: u64,
    calls: MsegCalls,
}

impl MsegInner {
    fn new(config: MsegConfig) -> MsegInner {
        MsegInner {
            config,
            cache: Vec::new(),
            live: 0,
            live_bytes: 0,
            max_live: 0,
            max_live_ever: 0,
            max_bytes: 0,
            max_bytes_ever: 0,
            watermark: 0,
            mapped: 0,
            mapped_bytes: 0,
            peak_mapped_bytes: 0,
            mapped_sbc: 0,
            mapped_mbc: 0,
            cache_hits: 0,
            calls: MsegCalls::default(),
        }
    }

    /// Records a segment handed to the caller (`ERTS_MSEG_ALLOC_STAT`).
    fn note_alloc(&mut self, size: usize) {
        self.live += 1;
        self.live_bytes += size;
        self.max_live = self.max_live.max(self.live);
        self.max_bytes = self.max_bytes.max(self.live_bytes);
        self.watermark = self.watermark.max(self.live);
    }

    /// Records a fresh mapping (`ERTS_MMAP_SIZE_*_INC`, `mseg_create`).
    fn note_mapping(&mut self, map_len: usize, kind: SegmentKind) {
        self.mapped += 1;
        self.mapped_bytes += map_len;
        self.peak_mapped_bytes = self.peak_mapped_bytes.max(self.mapped_bytes);
        match kind {
            SegmentKind::SingleBlock => self.mapped_sbc += 1,
            SegmentKind::MultiBlock => self.mapped_mbc += 1,
        }
        self.calls.create += 1;
    }

    /// Returns a segment to the OS (`mseg_destroy`).
    fn destroy(&mut self, entry: CacheEntry) {
        // SAFETY: `entry.base`/`entry.map_len` are exactly the address and length of a
        // mapping this allocator owns, and the segment is no longer reachable (it is
        // either being evicted from the cache or was just handed back). The range is
        // page-aligned by construction.
        if unsafe { os_unmap(entry.base, entry.map_len) } {
            self.calls.destroy += 1;
        } else {
            beam_common::beam_warn!(
                "beam_alloc: mseg could not munmap {:#x}+{}",
                entry.base,
                entry.map_len
            );
        }
        self.mapped = self.mapped.saturating_sub(1);
        self.mapped_bytes = self.mapped_bytes.saturating_sub(entry.map_len);
        match entry.kind {
            SegmentKind::SingleBlock => self.mapped_sbc = self.mapped_sbc.saturating_sub(1),
            SegmentKind::MultiBlock => self.mapped_mbc = self.mapped_mbc.saturating_sub(1),
        }
    }

    /// `mseg_dealloc`: hand a segment back, caching it if there is room.
    fn release(&mut self, entry: CacheEntry) {
        self.calls.dealloc += 1;
        debug_assert!(self.live > 0, "releasing a segment that was never handed out");
        self.live = self.live.saturating_sub(1);
        self.live_bytes = self.live_bytes.saturating_sub(entry.size);
        if self.config.cache && self.config.max_cache_size > 0 {
            while self.cache.len() >= self.config.max_cache_size {
                let victim = self.cache.remove(0);
                self.destroy(victim);
            }
            self.cache.push(entry);
            return;
        }
        self.destroy(entry);
    }

    /// `cache_get_segment`: take the smallest cached segment that fits.
    ///
    /// The returned entry is *retargeted* to the request: its usable size becomes
    /// exactly `size` (so the caller is never told it owns more than it asked for; the
    /// mapping's remainder stays visible through [`CacheEntry::capacity`]) and its
    /// alignment becomes exactly `align` (the payload is already a multiple of it —
    /// that is the reuse condition).
    fn take_cached(&mut self, size: usize, align: usize, kind: SegmentKind) -> Option<CacheEntry> {
        let mut best: Option<usize> = None;
        for (index, entry) in self.cache.iter().enumerate() {
            if entry.kind != kind || entry.payload % align != 0 || entry.capacity() < size {
                continue;
            }
            match best {
                Some(current) if self.cache[current].capacity() <= entry.capacity() => {}
                _ => best = Some(index),
            }
        }
        let mut entry = self.cache.remove(best?);

        // `+MMamcbf` / `+MMrmcbf`: a cached segment that is a bad fit is trimmed down
        // before it is reused rather than being kept as-is.
        let slack = entry.capacity() - size;
        let relative = self.config.rel_max_cache_bad_fit as u128 * size as u128;
        let good_fit =
            slack < self.config.abs_max_cache_bad_fit && (slack as u128) * 100 < relative;
        if !good_fit {
            if let Some((address, length)) = entry.tail_trim(size) {
                // SAFETY: `(address, length)` is a page-aligned tail of this cached
                // segment's retained mapping, past the usable area; the segment is not
                // handed out, so nothing points into the trimmed range.
                if unsafe { os_unmap(address, length) } {
                    entry.map_len -= length;
                    self.mapped_bytes -= length;
                    self.calls.destroy += 1;
                } else {
                    beam_common::beam_warn!(
                        "beam_alloc: mseg could not trim a reused segment at {address:#x}+{length}"
                    );
                }
            }
        }
        self.cache_hits += 1;
        entry.size = size;
        entry.align = align;
        Some(entry)
    }

    /// Returns every cached segment to the OS (`mseg_clear_cache`).
    fn purge_cache(&mut self) {
        while let Some(entry) = self.cache.pop() {
            self.destroy(entry);
        }
    }

    /// Returns the oldest cached segment to the OS (`mseg_check_cache`); returns the
    /// number of segments still cached.
    fn drop_one_cached(&mut self) -> u64 {
        if !self.cache.is_empty() {
            let victim = self.cache.remove(0);
            self.destroy(victim);
        }
        self.cache.len() as u64
    }

    /// Refuses a mapping the configured `+MM` limits do not allow.
    fn reserve(&mut self, kind: SegmentKind, size: usize) -> Result<()> {
        if self.mapped >= self.config.max_carriers as u64 {
            return Err(error::carrier_failure(size));
        }
        let (limit, used) = match kind {
            SegmentKind::SingleBlock => (self.config.max_sbc_carriers, self.mapped_sbc),
            SegmentKind::MultiBlock => (self.config.max_mbc_carriers, self.mapped_mbc),
        };
        if used >= limit as u64 {
            return Err(error::carrier_failure(size));
        }
        Ok(())
    }

    /// Resizes a live segment in place (`seg.capacity()` already covers `new_size`),
    /// optionally returning the freed tail to the OS.
    fn retarget(&mut self, seg: &mut Segment, new_size: usize, trim: bool) -> Result<()> {
        if trim {
            if let Some((address, length)) = seg.entry.tail_trim(new_size) {
                // SAFETY: `(address, length)` is a page-aligned tail of the segment's
                // retained mapping past the bytes that stay usable; the segment is
                // live and exclusively owned, so nothing points into that range.
                if !unsafe { os_unmap(address, length) } {
                    return Err(BeamError::Io(std::io::Error::last_os_error()));
                }
                seg.entry.map_len -= length;
                self.mapped_bytes -= length;
                self.calls.destroy += 1;
            }
        }
        if new_size >= seg.entry.size {
            self.live_bytes += new_size - seg.entry.size;
        } else {
            self.live_bytes -= seg.entry.size - new_size;
        }
        seg.entry.size = new_size;
        self.max_bytes = self.max_bytes.max(self.live_bytes);
        MsegHeader::new(&seg.entry).store(seg.entry.payload);
        Ok(())
    }

    /// The report snapshot; also folds the period maxima into the "ever" maxima,
    /// exactly like the legacy `info_status`.
    fn snapshot(&mut self) -> MsegInfo {
        if self.max_live_ever < self.max_live {
            self.max_live_ever = self.max_live;
        }
        if self.max_bytes_ever < self.max_bytes {
            self.max_bytes_ever = self.max_bytes;
        }
        MsegInfo {
            segments: self.live,
            max_segments: self.max_live,
            max_segments_ever: self.max_live_ever,
            bytes: self.live_bytes,
            max_bytes: self.max_bytes,
            max_bytes_ever: self.max_bytes_ever,
            watermark: self.watermark,
            cached_segments: self.cache.len() as u64,
            cache_hits: self.cache_hits,
            mapped_segments: self.mapped,
            mapped_bytes: self.mapped_bytes,
            peak_mapped_bytes: self.peak_mapped_bytes,
            calls: self.calls,
        }
    }
}

/// Maps `len` bytes of anonymous, private, read/write memory.
///
/// Returns the base address of a fresh, exclusively-owned, page-aligned mapping, or
/// `BeamError::Alloc`-classified [`AllocKind::Carrier`](beam_common::AllocKind).
fn os_map(len: usize) -> Result<usize> {
    debug_assert!(len > 0);
    // SAFETY: an anonymous, private mapping has no preconditions beyond valid flags
    // and a non-zero length; the result is either a fresh mapping owned by this
    // module or `MAP_FAILED`, which is handled below.
    let mapped = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(error::carrier_failure(len));
    }
    Ok(mapped as usize)
}

/// Returns `[address, address + len)` to the OS; `true` on success.
///
/// # Safety
///
/// The range must be a page-aligned sub-range of a *live* mapping created by
/// [`os_map`], must not be used afterwards, and no live pointer may point into it.
unsafe fn os_unmap(address: usize, len: usize) -> bool {
    debug_assert_eq!(address % page_size(), 0);
    debug_assert_eq!(len % page_size(), 0);
    // SAFETY: the caller guarantees the range is a page-aligned part of one of this
    // module's live anonymous mappings and is not used afterwards.
    unsafe { libc::munmap(address as *mut c_void, len) == 0 }
}

/// A freshly mapped, already trimmed segment.
struct MappedSegment {
    base: usize,
    map_len: usize,
    payload: usize,
}

/// Maps `size` usable bytes whose payload is `align`-aligned, over-mapping and then
/// trimming the leading and trailing slack.
///
/// This is the Rust counterpart of the legacy `os_mmap_aligned()`: one `mmap` of
/// `size + align + page + MSEG_HEADER_SIZE` bytes guarantees that an `align`-aligned
/// address with room for the segment header in front of it and `size` usable bytes
/// behind it exists inside the mapping; the slack before and after that window is
/// then `munmap`-ed again so the segment does not pin `align` bytes of address space.
/// Trimming is best-effort: if the OS refuses, the slack simply stays mapped (the
/// segment stays correct, only its footprint is larger).
fn map_aligned(size: usize, align: usize) -> Result<MappedSegment> {
    let page = page_size();
    let over = size
        .checked_add(align)
        .and_then(|value| value.checked_add(page))
        .and_then(|value| value.checked_add(MSEG_HEADER_SIZE))
        .ok_or_else(|| error::carrier_failure(size))?;
    let map_len = align_up(over, page).ok_or_else(|| error::carrier_failure(size))?;
    let base = os_map(map_len)?;

    let payload = align_up(base + MSEG_HEADER_SIZE, align)
        .filter(|payload| *payload + size.max(1) <= base + map_len)
        .ok_or_else(|| error::carrier_failure(size))?;
    debug_assert_eq!(payload % align, 0, "payload honours the requested alignment");

    // Leading slack: everything before the payload except the header's own bytes. Only
    // whole pages can be unmapped, so at most one page (and the header) is retained.
    let front_bytes = payload - MSEG_HEADER_SIZE - base;
    let front_trim = front_bytes - (front_bytes % page);
    let mut base = base;
    let mut map_len = map_len;
    if front_trim > 0 && unsafe { os_unmap(base, front_trim) } {
        base += front_trim;
        map_len -= front_trim;
    }

    // Trailing slack: everything past the last page that still contains usable bytes.
    let usable_end = payload + size.max(1);
    let keep_end = align_up(usable_end - base, page).unwrap_or(map_len).min(map_len);
    let back_trim = map_len - keep_end;
    if back_trim > 0 && unsafe { os_unmap(base + keep_end, back_trim) } {
        map_len = keep_end;
    }

    Ok(MappedSegment {
        base,
        map_len,
        payload,
    })
}

/// Reconstructs the cache entry of a live segment from its payload pointer.
///
/// # Safety
///
/// `payload` must point at a live segment payload owned by a [`MsegAllocator`];
/// the header bytes in front of it are dereferenced.
unsafe fn entry_for_payload(payload: usize) -> Result<CacheEntry> {
    let header = unsafe { MsegHeader::load(payload)? };
    let kind = SegmentKind::from_header(header.kind).ok_or(BeamError::BadArg)?;
    Ok(CacheEntry {
        base: header.base,
        map_len: header.map_len,
        payload: header.payload,
        size: header.size,
        align: header.align,
        kind,
    })
}

/// The mmap-backed memory-segment allocator (legacy `erts_mseg`).
///
/// Cheap to clone: clones share one set of counters, one reuse cache and one mutex,
/// so all of them hand out (and take back) segments of the same pool.
#[derive(Clone)]
pub struct MsegAllocator {
    state: Arc<MsegState>,
}

impl core::fmt::Debug for MsegAllocator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let info = self.stats();
        f.debug_struct("MsegAllocator")
            .field("config", &self.config())
            .field("segments", &info.segments)
            .field("mapped_segments", &info.mapped_segments)
            .field("mapped_bytes", &info.mapped_bytes)
            .finish()
    }
}

impl MsegAllocator {
    /// The version string the legacy segment allocator reports
    /// (`ERTS_MSEG_VSN_STR`).
    pub const VERSION: &'static str = MSEG_VERSION;

    /// Creates an allocator from `config` (normalising it the way `erts_mseg_init`
    /// does).
    ///
    /// This never touches the OS: no memory is mapped until the first
    /// [`map`](MsegAllocator::map).
    pub fn new(config: MsegConfig) -> MsegAllocator {
        MsegAllocator {
            state: Arc::new(MsegState {
                inner: Mutex::new(MsegInner::new(config.normalised())),
            }),
        }
    }

    /// Creates an allocator with the legacy default configuration.
    pub fn with_defaults() -> MsegAllocator {
        MsegAllocator::new(MsegConfig::default())
    }

    /// The effective configuration (after normalisation).
    pub fn config(&self) -> MsegConfig {
        self.state.inner.lock().config.clone()
    }

    /// Maps a segment of at least `size` bytes whose start is `align`-aligned,
    /// classified as a multi-block carrier segment.
    ///
    /// `align` must be a non-zero power of two; any power of two is supported, the
    /// ones above the page size by over-mapping and trimming (see [`map_aligned`]).
    /// A `size` of 0 is valid: the segment then has a page-granular capacity but
    /// reports a usable size of 0.
    ///
    /// # Errors
    ///
    /// * [`BeamError::BadArg`] for an alignment of zero or a non-power-of-two.
    /// * `BeamError::Alloc` when the extent overflows, the OS cannot map the memory,
    ///   or a configured `+MM` limit would be exceeded.
    pub fn map(&self, size: usize, align: usize) -> Result<Segment> {
        self.map_kind(size, align, SegmentKind::MultiBlock)
    }

    /// Like [`map`](MsegAllocator::map), classified as a single-block carrier
    /// segment (bounded by `max_sbc_carriers`).
    pub fn map_sbc(&self, size: usize, align: usize) -> Result<Segment> {
        self.map_kind(size, align, SegmentKind::SingleBlock)
    }

    /// Like [`map`](MsegAllocator::map), with an explicit [`SegmentKind`].
    ///
    /// This is the legacy `erts_mseg_alloc_opt` entry point: it runs the same
    /// cache/limit checks and honours the same configuration.
    ///
    /// # Errors
    ///
    /// As [`map`](MsegAllocator::map).
    pub fn map_kind(&self, size: usize, align: usize, kind: SegmentKind) -> Result<Segment> {
        if !align.is_power_of_two() {
            return Err(error::bad_align(align));
        }
        let mut inner = self.state.inner.lock();
        inner.calls.alloc += 1;

        if inner.config.cache && inner.config.max_cache_size > 0 {
            if let Some(entry) = inner.take_cached(size, align, kind) {
                inner.note_alloc(entry.size);
                MsegHeader::new(&entry).store(entry.payload);
                return Ok(Segment::new(Arc::clone(&self.state), entry));
            }
        }

        inner.reserve(kind, size)?;
        let mapped = map_aligned(size, align)?;
        inner.note_mapping(mapped.map_len, kind);
        let entry = CacheEntry {
            base: mapped.base,
            map_len: mapped.map_len,
            payload: mapped.payload,
            size,
            align,
            kind,
        };
        inner.note_alloc(size);
        MsegHeader::new(&entry).store(entry.payload);
        Ok(Segment::new(Arc::clone(&self.state), entry))
    }

    /// Returns `seg` to the allocator (legacy `erts_mseg_dealloc`): into the reuse
    /// cache if it has room, otherwise straight to the OS.
    ///
    /// Equivalent to dropping the segment; it exists because the legacy API has an
    /// explicit deallocation entry point.
    pub fn free(&self, seg: Segment) {
        drop(seg);
    }

    /// Resizes `seg` to `new_size` usable bytes, preserving its contents (legacy
    /// `erts_mseg_realloc`).
    ///
    /// The segment is grown or shrunk **in place** when its retained mapping already
    /// covers the new size; otherwise a fresh segment is mapped, the contents are
    /// copied and the old segment is released. Shrinking additionally returns the
    /// freed pages to the OS when the configured shrink thresholds say the shrink is
    /// worth it (`abs_shrink_threshold` / `rel_shrink_threshold`; the legacy defaults
    /// shrink whenever a whole page can be released).
    ///
    /// A shrink always takes effect logically — [`Segment::size`] becomes `new_size` —
    /// but pages the thresholds consider "not worth releasing" stay mapped (the legacy
    /// `mseg_realloc` instead refuses the shrink and reports the old size; keeping the
    /// caller's requested size while retaining the address space is the behaviour this
    /// module documents, and [`Segment::capacity`] still exposes the retained room).
    ///
    /// `old_size` is the size the caller believes `seg` has; it must match
    /// [`Segment::size`], otherwise the call is a caller bug and the segment is left
    /// untouched.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] on a size mismatch, and the same errors as
    /// [`map`](MsegAllocator::map) when the growth needs a fresh mapping. On error
    /// `seg` is unchanged and still owns its memory.
    pub fn realloc(&self, seg: &mut Segment, old_size: usize, new_size: usize) -> Result<()> {
        if old_size != seg.entry.size {
            return Err(BeamError::BadArg);
        }
        self.state.inner.lock().calls.realloc += 1;

        if new_size <= seg.entry.capacity() {
            let trim = new_size < seg.entry.size
                && self.shrink_is_worthwhile(old_size, old_size - new_size);
            return self.state.inner.lock().retarget(seg, new_size, trim);
        }

        // Growth beyond the mapping: map a fresh segment, copy, release the old one.
        // `map_kind` cannot return `seg` itself (a live segment is never cached), so
        // the copy can never overlap its source.
        let fresh = self.map_kind(new_size, seg.entry.align, seg.entry.kind)?;
        let copy_len = seg.entry.size.min(fresh.entry.size);
        // SAFETY: both segments own their mappings exclusively and are distinct, so
        // the two ranges of `copy_len` bytes are valid, initialised and disjoint.
        unsafe {
            ptr::copy_nonoverlapping(seg.as_ptr() as *const u8, fresh.as_ptr(), copy_len);
        }
        self.state.inner.lock().calls.recreate += 1;
        let old = core::mem::replace(seg, fresh);
        drop(old);
        Ok(())
    }

    /// Whether the legacy shrink thresholds want a shrink of `shrink` bytes on a
    /// segment of `old_size` bytes to actually release memory (`+M<alloc>rsbcst`).
    fn shrink_is_worthwhile(&self, old_size: usize, shrink: usize) -> bool {
        let config = self.state.inner.lock().config.clone();
        shrink >= config.abs_shrink_threshold
            || (shrink as u128) * 100 >= config.rel_shrink_threshold as u128 * old_size as u128
    }

    /// Returns `seg` to the allocator, reconstructing it from its payload pointer
    /// (legacy `erts_mseg_dealloc`, which takes a bare `void *`).
    ///
    /// # Safety
    ///
    /// `payload` must be the payload of a *live* segment mapped by this (or a cloned)
    /// allocator and must not be used afterwards. The segment handle for it must not
    /// be alive any more (otherwise the mapping is released twice), and the header
    /// bytes in front of `payload` must still be intact.
    pub unsafe fn free_ptr(&self, payload: *mut u8) -> Result<()> {
        if payload.is_null() {
            return Ok(());
        }
        let entry = unsafe { entry_for_payload(payload as usize)? };
        self.state.inner.lock().release(entry);
        Ok(())
    }

    /// Rebuilds the [`Segment`] handle of a live segment from its payload pointer.
    ///
    /// # Safety
    ///
    /// `payload` must be the payload of a *live* segment mapped by this (or a cloned)
    /// allocator; the returned handle takes ownership and must be the only one (see
    /// [`free_ptr`](MsegAllocator::free_ptr)).
    pub unsafe fn from_raw(&self, payload: *mut u8) -> Result<Segment> {
        let entry = unsafe { entry_for_payload(payload as usize)? };
        Ok(Segment::new(Arc::clone(&self.state), entry))
    }

    /// A point-in-time snapshot of the counters.
    pub fn stats(&self) -> MsegInfo {
        self.state.inner.lock().snapshot()
    }

    /// Starts a new "max period": the period maxima are set to the current values,
    /// the "ever" maxima are left alone (legacy `begin_new_max_period`).
    pub fn reset_max(&self) {
        let mut inner = self.state.inner.lock();
        inner.max_live = inner.live;
        inner.max_bytes = inner.live_bytes;
    }

    /// Returns every cached segment to the OS (legacy `erts_mseg_clear_cache`).
    pub fn clear_cache(&self) {
        let mut inner = self.state.inner.lock();
        inner.calls.clear_cache += 1;
        inner.purge_cache();
    }

    /// Returns the oldest cached segment to the OS (legacy `erts_mseg_check_cache`)
    /// and reports how many segments are still cached.
    pub fn check_cache(&self) -> u64 {
        let mut inner = self.state.inner.lock();
        inner.calls.check_cache += 1;
        inner.drop_one_cached()
    }

    /// The number of segments currently mapped from the OS (handed out plus cached),
    /// the legacy `erts_mseg_no()`.
    pub fn segment_count(&self) -> u64 {
        self.state.inner.lock().mapped
    }

    /// The byte size the legacy `erts_mseg_unit_size()` reports: the super-alignment
    /// granularity.
    pub fn unit_size() -> usize {
        SUPER_ALIGNED_SIZE
    }
}

#[cfg(test)]
mod tests;
