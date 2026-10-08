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

//! The `+MM` / `+M` configuration surface of the segment allocator, the kind of
//! carrier a segment is obtained for, and the counter/report types.
//!
//! Everything here mirrors an option of the legacy `erl_mseg.c` /
//! `erl_alloc_util.c` (the `ErtsMsegInit_t` / `AllcUInit_t` / `AllctrInit_t`
//! blocks); the defaults are the ones those sources install.

/// The largest number of segments the reuse cache may hold (`MAX_CACHE_SIZE` in the
/// legacy; a larger `mcs` is clamped to it exactly like `erts_mseg_init` does).
pub const MAX_CACHE_SIZE: usize = 30;

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

    pub(super) fn to_header(self) -> u64 {
        match self {
            SegmentKind::MultiBlock => 0,
            SegmentKind::SingleBlock => 1,
        }
    }

    pub(super) fn from_header(value: u64) -> Option<SegmentKind> {
        match value {
            0 => Some(SegmentKind::MultiBlock),
            1 => Some(SegmentKind::SingleBlock),
            _ => None,
        }
    }
}

/// Configuration of a [`MsegAllocator`](super::MsegAllocator), with the legacy `+MM` /
/// `+M` defaults.
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
    pub(super) fn normalised(mut self) -> MsegConfig {
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

/// A point-in-time snapshot of a [`MsegAllocator`](super::MsegAllocator)'s counters.
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
