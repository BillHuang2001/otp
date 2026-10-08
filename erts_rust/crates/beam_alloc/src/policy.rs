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

//! The allocator **carrier policy**: the legacy `alloc_util` thresholds/limits
//! options and their per-allocator defaults (compatibility contract A.6 —
//! configuration fidelity).
//!
//! The legacy framework configures every carrier-based allocator through exactly one
//! `AllctrInit_t` (`erts/emulator/beam/erl_alloc_util.h:48-89`), which
//! `erts_alcu_start` (`erl_alloc_util.c:6701-6860`) copies into the live `Allctr_t`.
//! [`CarrierPolicy`] is that configuration as plain data: the twelve options that make
//! up the *carrier-pool sizing* policy, with the legacy names, units and defaults, so
//! that `+M<S><opt>` handling (owned by `beam_rt`, the A.6 CLI surface) and a
//! test-allocator configuration do not have to re-derive them.
//!
//! # The modelled options
//!
//! Grouped exactly as `erts_alloc.md` groups them (`erts/doc/references/erts_alloc.md:359-560`);
//! every name is one entry of the `plusM_au_alloc_switches` table of
//! `erts/etc/common/erlexec.c:73-98` and one `handle_au_arg` branch
//! (`erts/emulator/beam/erl_alloc.c:1282-1553`).
//!
//! | option | `AllctrInit_t` field | live `Allctr_t` field / macro | CLI unit | stored unit | legacy default |
//! |---|---|---|---|---|---|
//! | `sbct` | `sbct` | `sbc_threshold` (`erl_alloc_util.h:678`) | kilobytes | bytes | `512*1024` |
//! | `asbcst` | `asbcst` | `mseg_opt.abs_shrink_th` (`erl_mseg.h:83`) | kilobytes | bytes | `2*1024*2024` |
//! | `rsbcst` | `rsbcst` | `mseg_opt.rel_shrink_th` (`erl_mseg.h:84`) | percent | percent | `20` |
//! | `rsbcmt` | `rsbcmt` | `sbc_move_threshold` (`erl_alloc_util.h:679`) | percent | percent | `80` |
//! | `mmsbc` | `mmsbc` | `max_mseg_sbcs` (`erl_alloc_util.h:682`) | amount | count | `256` |
//! | `mmmbc` | `mmmbc` | `max_mseg_mbcs` (`erl_alloc_util.h:683`) | amount | count | `~(UWord)0` |
//! | `smbcs` | `smbcs` | `smallest_mbc_size` (`erl_alloc_util.h:685`) | kilobytes | bytes | `1024*1024` |
//! | `lmbcs` | `lmbcs` | `largest_mbc_size` (`erl_alloc_util.h:684`) | kilobytes | bytes | `10*1024*1024` |
//! | `acul` | `acul` | `cpool.util_limit` (`erl_alloc_util.h:714`) | percent | percent | `0` |
//! | `acful` | `acful` | `cpool.free_util_limit` (`erl_alloc_util.h:715`) | percent | percent | `0` |
//! | `acnl` | `acnl` | `cpool.in_pool_limit` (`erl_alloc_util.h:716`) | amount | count | `1000` |
//! | `acfml` | `acfml` | `cpool.fblk_min_limit` (`erl_alloc_util.h:717`) | bytes | bytes | `0` |
//!
//! The default column is the `ERTS_DEFAULT_ALLCTR_INIT` set
//! (`erl_alloc_util.h:110-141`), i.e. what `SET_DEFAULT_ALLOC_OPTS`
//! (`erl_alloc.c:214-218`) installs for *every* allocator's `AllctrInit_t` before the
//! per-allocator `set_default_*_alloc_opts` function adjusts it. The `SMALL_MEMORY`
//! variant (`erl_alloc_util.h:152-183`) differs for the three size options only
//! (`sbct` = `64*1024`, `smbcs` = `128*1024`, `lmbcs` = `1024*1024`); the system-OTP
//! oracle is built with the non-`SMALL_MEMORY` values modelled here.
//!
//! The twelve names are the *carrier-pool thresholds* (`sbct`, `asbcst`, `rsbcst`,
//! `rsbcmt`, `mmsbc`, `mmmbc`, `smbcs`, `lmbcs` — how many carriers may be handed out
//! by `mseg_alloc` and when one is created/destroyed) and the *carrier-migration
//! utilization limits* (`acul`, `acful`, `acnl`, `acfml` — when a multiblock carrier
//! may be abandoned into its carrier pool) — see [`PolicyGroup`].
//!
//! # Deliberately **not** modelled here
//!
//! `mmbcs` (main multiblock carrier size) is the same knob as
//! [`crate::allocator::AllocatorConfig`]'s `carrier_size` and stays owned by the
//! allocator seam; `rmbcmt` (the multiblock-carrier sibling of `rsbcmt`), `mbcgs`,
//! `mbsd`, `mseg_opt`, `ramv`, `atags`, `cp`, `t`/`e` and the `+Mu…` options are not
//! carrier-pool *threshold/limit* policy. In particular the legacy carrier-growth
//! ladder `smbcs + nc*(lmbcs - smbcs)/mbcgs` (`erts_alloc.md:118-119`,
//! `erl_alloc_util.c:1200-1207`) also needs `mbcgs`, so it is not offered as a derived
//! accessor here.
//!
//! # Per-allocator defaults
//!
//! [`CarrierPolicy::for_allocator`] reproduces the `set_default_*_alloc_opts`
//! functions (`erl_alloc.c:220-490`) plus the two adjustments
//! `erts_alloc_init` makes to the option *values* themselves
//! (`erl_alloc.c:699-710`):
//!
//! | allocator | `sbct` | `asbcst` | `rsbcst` | `rsbcmt` | `mmsbc` | `mmmbc` | `smbcs` | `lmbcs` | `acul` | `acful` | `acnl` | `acfml` |
//! |---|---|---|---|---|---|---|---|---|---|---|---|---|
//! | `sys_alloc` | 512 KiB | 2 MiB† | 20 | 80 | 256 | ∞ | 1 MiB | 10 MiB | 0 | 0 | 1000 | 0 |
//! | `temp_alloc` | 512 KiB | 2 MiB† | **90** | 80 | 256 | ∞ | 1 MiB | 10 MiB | 0 | 0 | 1000 | 0 |
//! | `sl_alloc` | 512 KiB | 2 MiB† | **80** | 80 | 256 | ∞ | 1 MiB | 10 MiB | **60** | 0 | 1000 | 0 |
//! | `std_alloc` | 512 KiB | 2 MiB† | 20 | 80 | 256 | ∞ | 1 MiB | 10 MiB | **60** | 0 | 1000 | 0 |
//! | `ll_alloc` | **∞** | **0** | **0** | **0** | **0** | ∞ | 1 MiB | 10 MiB | **85** | 0 | 1000 | 0 |
//! | `eheap_alloc` | 512 KiB | 2 MiB† | **50** | 80 | 256 | ∞ | 1 MiB | 10 MiB | **45** | 0 | 1000 | 0 |
//! | `ets_alloc` | 512 KiB | 2 MiB† | 20 | 80 | 256 | ∞ | 1 MiB | 10 MiB | **60** | 0 | 1000 | 0 |
//! | `fix_alloc` | 512 KiB | 2 MiB† | 20 | 80 | 256 | ∞ | 1 MiB | 10 MiB | **60** | 0 | 1000 | 0 |
//! | `literal_alloc` | **∞** | **0** | **0** | **0** | **0** | ∞ | 1 MiB | 10 MiB | **0** | 0 | 1000 | 0 |
//! | `binary_alloc` | 512 KiB | 2 MiB† | 20 | 80 | 256 | ∞ | 1 MiB | 10 MiB | **60** | 0 | 1000 | 0 |
//! | `driver_alloc` | 512 KiB | 2 MiB† | 20 | 80 | 256 | ∞ | 1 MiB | 10 MiB | **60** | 0 | 1000 | 0 |
//! | `test_alloc` | **16 KiB** | 2 MiB† | 20 | 80 | 256 | ∞ | **16 KiB** | **16 KiB** | **60** | 0 | 1000 | 0 |
//!
//! † `asbcst`'s legacy initializer literal is `2*1024*2024` (the last factor is
//! `2024`, not `1024`), i.e. **4 145 152** bytes. It is almost certainly a typo for
//! 2 MiB, but it is the value the C actually installs — the oracle reports
//! `{asbcst,4145152}` — so it is reproduced verbatim.
//!
//! `std_alloc`, `ets_alloc`, `fix_alloc`, `binary_alloc` and `driver_alloc` share one
//! built-in default set (the `ERTS_DEFAULT_ALLCTR_INIT` values with `acul`
//! = `ERTS_ALC_DEFAULT_ACUL` = 60, `erl_alloc.c:76-82`) — they differ only in
//! `name_prefix`/strategy/carrier size, none of which is a policy field. `test_alloc`
//! pins `smbcs`/`lmbcs`/`sbct` to the single constant minimal MBC size
//! (`ERTS_SACRR_UNIT_SZ` = `1 << ERTS_SUPER_ALIGN_BITS`, `1 << 14` = 16 KiB on the
//! 64-bit oracle; `erl_alloc.c:480-489`). `sys_alloc` is not a carrier allocator at
//! all (`erts_allctrs[ERTS_ALC_A_SYSTEM].alloc = erts_sys_alloc`,
//! `erl_alloc.c:770-773`) and owns no `Allctr_t`, so its row is the
//! [`CarrierPolicy::ALLCTR_INIT`] set that a hypothetical `AllctrInit_t` would carry
//! — documented as an inherited placeholder, not a live configuration.
//!
//! # Three legacy post-processing steps the *configured* values do not include
//!
//! These are properties of a started `Allctr_t`, not of the options, so
//! [`CarrierPolicy`] holds the configured values and exposes the derivation where it
//! is a pure function of the policy:
//!
//! * `adjust_sbct` (`erl_alloc_util.c:6668-6686`) clamps `sbct` to the largest
//!   `Uint` block that fits a multiblock carrier header — inside `#ifndef ARCH_64`,
//!   so it is the identity on the 64-bit oracle.
//! * `largest_mbc_size` is `MAX(init->lmbcs, init->smbcs)`
//!   (`erl_alloc_util.c:6779`) — see
//!   [`CarrierPolicy::effective_largest_mbc_size`].
//! * when an allocator is thread-preferred the *default* `lmbcs`/`smbcs`/`mmbcs` are
//!   divided by the scheduler count (`adjust_tpref`, `erl_alloc.c:494-513`), and the
//!   carrier pool is only armed when `!init->ts && acul && acnl && cp >= 0`
//!   (`erl_alloc_util.c:6813-6839`) — which is why the oracle's
//!   `erlang:system_info({allocator, _})` reports `{acul,0}` even for `sl_alloc`
//!   (configured `60`) and a divided `lmbcs`. Instance sizing is a runtime concern.

use crate::types::AllocatorId;
use beam_common::{BeamError, Result};

/// The unit the `+M<S><opt>` CLI form and the legacy parser use for one option,
/// together with the unit [`CarrierPolicy`] stores it in.
///
/// `Kilobytes` is the interesting case: the CLI (and `get_kb_value` →
/// `kb_to_bytes`, `erl_alloc.c:1157-1183`) takes kilobytes but the `AllctrInit_t`
/// field holds **bytes**, so [`CarrierPolicy::set`] converts. `Bytes` is the unit of
/// `acfml`, which the CLI already gives in bytes (`get_amount_value`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PolicyUnit {
    /// `+M<S><opt> <kb>`: given in kilobytes, stored as bytes (×1024).
    Kilobytes,
    /// `<bytes>`: given and stored in bytes.
    Bytes,
    /// `<percent>`: an integer in `0..=100`, stored as a percentage.
    Percent,
    /// `<amount>`: an untyped count (a maximum number of carriers), stored as a count.
    Amount,
}

/// Which of the two legacy option groups an option belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PolicyGroup {
    /// Carrier-pool thresholds: how `mseg_alloc` carriers are sized, capped and
    /// destroyed (`sbct`, `asbcst`, `rsbcst`, `rsbcmt`, `mmsbc`, `mmmbc`, `smbcs`,
    /// `lmbcs`).
    CarrierPoolThresholds,
    /// Carrier-migration (abandoned-carrier) utilization limits: when a multiblock
    /// carrier may be handed to the allocator's carrier pool (`acul`, `acful`,
    /// `acnl`, `acfml`).
    UtilizationLimits,
}

/// One `+M<S><opt>` option of the legacy carrier policy (the twelve options of
/// [`CarrierPolicy`], named exactly as the CLI names them).
///
/// The variants are spelled like the legacy options (`Sbct` for `sbct`) so a reader can
/// map each one 1:1 to the `AllctrInit_t` field, the `Allctr_t` field and the
/// `plusM_au_alloc_switches` entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PolicyOptionKind {
    /// `sbct` — singleblock carrier threshold.
    Sbct,
    /// `asbcst` — absolute singleblock carrier shrink threshold.
    Asbcst,
    /// `rsbcst` — relative singleblock carrier shrink threshold.
    Rsbcst,
    /// `rsbcmt` — relative singleblock carrier move threshold.
    Rsbcmt,
    /// `mmsbc` — maximum number of `mseg_alloc` singleblock carriers.
    Mmsbc,
    /// `mmmbc` — maximum number of `mseg_alloc` multiblock carriers.
    Mmmbc,
    /// `smbcs` — smallest `mseg_alloc` multiblock carrier size.
    Smbcs,
    /// `lmbcs` — largest `mseg_alloc` multiblock carrier size.
    Lmbcs,
    /// `acul` — abandoned carrier utilization limit.
    Acul,
    /// `acful` — carrier free utilization limit.
    Acful,
    /// `acnl` — abandoned carriers number limit.
    Acnl,
    /// `acfml` — abandoned carrier free block min limit.
    Acfml,
}

impl PolicyOptionKind {
    /// Every policy option: the eight carrier-pool thresholds in the order
    /// [`PolicyOptionKind::CARRIER_POOL_THRESHOLDS`] lists them, then the four
    /// utilization limits of [`PolicyOptionKind::UTILIZATION_LIMITS`].
    pub const ALL: [PolicyOptionKind; 12] = [
        PolicyOptionKind::Sbct,
        PolicyOptionKind::Asbcst,
        PolicyOptionKind::Rsbcst,
        PolicyOptionKind::Rsbcmt,
        PolicyOptionKind::Mmsbc,
        PolicyOptionKind::Mmmbc,
        PolicyOptionKind::Smbcs,
        PolicyOptionKind::Lmbcs,
        PolicyOptionKind::Acul,
        PolicyOptionKind::Acful,
        PolicyOptionKind::Acnl,
        PolicyOptionKind::Acfml,
    ];

    /// The carrier-pool thresholds, in legacy report order
    /// (`erl_alloc_util.c:5535-5548`).
    pub const CARRIER_POOL_THRESHOLDS: [PolicyOptionKind; 8] = [
        PolicyOptionKind::Sbct,
        PolicyOptionKind::Asbcst,
        PolicyOptionKind::Rsbcst,
        PolicyOptionKind::Rsbcmt,
        PolicyOptionKind::Mmsbc,
        PolicyOptionKind::Mmmbc,
        PolicyOptionKind::Smbcs,
        PolicyOptionKind::Lmbcs,
    ];

    /// The carrier-migration utilization limits, in legacy report order
    /// (`erl_alloc_util.c:5550-5553`).
    pub const UTILIZATION_LIMITS: [PolicyOptionKind; 4] = [
        PolicyOptionKind::Acul,
        PolicyOptionKind::Acful,
        PolicyOptionKind::Acnl,
        PolicyOptionKind::Acfml,
    ];

    /// The legacy option name (the `+M<S><opt>` sub-parameter, one entry of
    /// `plusM_au_alloc_switches`, `erts/etc/common/erlexec.c:73-98`).
    pub const fn option_name(self) -> &'static str {
        match self {
            PolicyOptionKind::Sbct => "sbct",
            PolicyOptionKind::Asbcst => "asbcst",
            PolicyOptionKind::Rsbcst => "rsbcst",
            PolicyOptionKind::Rsbcmt => "rsbcmt",
            PolicyOptionKind::Mmsbc => "mmsbc",
            PolicyOptionKind::Mmmbc => "mmmbc",
            PolicyOptionKind::Smbcs => "smbcs",
            PolicyOptionKind::Lmbcs => "lmbcs",
            PolicyOptionKind::Acul => "acul",
            PolicyOptionKind::Acful => "acful",
            PolicyOptionKind::Acnl => "acnl",
            PolicyOptionKind::Acfml => "acfml",
        }
    }

    /// The `AllctrInit_t` member this option sets (identical to the option name —
    /// `erl_alloc_util.h:61-75` is the option set).
    pub const fn init_field(self) -> &'static str {
        self.option_name()
    }

    /// The live `Allctr_t` member (or member path) `erts_alcu_start` copies the option
    /// into (`erl_alloc_util.c:6762-6785`, `6820-6823`).
    pub const fn allctr_field(self) -> &'static str {
        match self {
            PolicyOptionKind::Sbct => "sbc_threshold",
            PolicyOptionKind::Asbcst => "mseg_opt.abs_shrink_th",
            PolicyOptionKind::Rsbcst => "mseg_opt.rel_shrink_th",
            PolicyOptionKind::Rsbcmt => "sbc_move_threshold",
            PolicyOptionKind::Mmsbc => "max_mseg_sbcs",
            PolicyOptionKind::Mmmbc => "max_mseg_mbcs",
            PolicyOptionKind::Smbcs => "smallest_mbc_size",
            PolicyOptionKind::Lmbcs => "largest_mbc_size",
            PolicyOptionKind::Acul => "cpool.util_limit",
            PolicyOptionKind::Acful => "cpool.free_util_limit",
            PolicyOptionKind::Acnl => "cpool.in_pool_limit",
            PolicyOptionKind::Acfml => "cpool.fblk_min_limit",
        }
    }

    /// The CLI unit and the storage unit of this option.
    pub const fn unit(self) -> PolicyUnit {
        match self {
            PolicyOptionKind::Sbct
            | PolicyOptionKind::Asbcst
            | PolicyOptionKind::Smbcs
            | PolicyOptionKind::Lmbcs => PolicyUnit::Kilobytes,
            PolicyOptionKind::Acfml => PolicyUnit::Bytes,
            PolicyOptionKind::Rsbcst
            | PolicyOptionKind::Rsbcmt
            | PolicyOptionKind::Acul
            | PolicyOptionKind::Acful => PolicyUnit::Percent,
            PolicyOptionKind::Mmsbc | PolicyOptionKind::Mmmbc | PolicyOptionKind::Acnl => {
                PolicyUnit::Amount
            }
        }
    }

    /// Which legacy group this option belongs to.
    pub const fn group(self) -> PolicyGroup {
        match self {
            PolicyOptionKind::Acul
            | PolicyOptionKind::Acful
            | PolicyOptionKind::Acnl
            | PolicyOptionKind::Acfml => PolicyGroup::UtilizationLimits,
            _ => PolicyGroup::CarrierPoolThresholds,
        }
    }

    /// Whether this option is a percentage.
    pub const fn is_percent(self) -> bool {
        matches!(self.unit(), PolicyUnit::Percent)
    }

    /// Resolves a legacy `+M<S><opt>` option name to its kind, case-sensitively (the
    /// C parser lower-cases nothing and compares with `sys_strcmp`/`has_prefix`,
    /// `erl_alloc.c:1291-1553`). Returns `None` for any other name.
    ///
    /// Mirrors the `Strategy::from_option_name` precedent
    /// ([`crate::strategy::Strategy::from_option_name`]).
    pub fn from_option_name(name: &str) -> Option<PolicyOptionKind> {
        PolicyOptionKind::ALL
            .into_iter()
            .find(|kind| kind.option_name() == name)
    }
}

/// The legacy `alloc_util` carrier policy: the carrier-pool thresholds and the
/// carrier-migration utilization limits, in the units the legacy `AllctrInit_t` stores
/// them in.
///
/// The field names are the legacy option names verbatim (so a reader can map a field
/// to `AllctrInit_t`, `Allctr_t`, the `+M<S><opt>` flag and the
/// `erlang:system_info({allocator, _})` `options` proplist 1:1); see each field for its
/// unit, its legacy default and what the C does with it. Construct one with
/// [`CarrierPolicy::default`] (the `ERTS_DEFAULT_ALLCTR_INIT` set) or
/// [`CarrierPolicy::for_allocator`] (the per-allocator legacy defaults), then adjust it
/// with [`CarrierPolicy::set`]/[`CarrierPolicy::set_option`]/
/// [`CarrierPolicy::with_option`], which validate every value and reject impossible
/// ones with [`BeamError::BadArg`] rather than substituting a default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CarrierPolicy {
    /// **`sbct`** — singleblock carrier threshold, in **bytes**: a request of
    /// `size >= sbct` gets a dedicated singleblock carrier
    /// (`erl_alloc_util.c:1408`; [`CarrierPolicy::uses_single_block_carrier`]).
    /// `AllctrInit_t::sbct` → `Allctr_t::sbc_threshold` via `adjust_sbct`
    /// (`erl_alloc_util.c:6847`), which only clamps on 32-bit. Default `512*1024`;
    /// [`CarrierPolicy::UNLIMITED`] for `ll_alloc`/`literal_alloc`, `1 << 14` (16 KiB)
    /// for `test_alloc` (`erl_alloc.c:269`, `303`, `480-489`).
    pub sbct: usize,
    /// **`asbcst`** — absolute singleblock carrier shrink threshold, in **bytes**:
    /// below this much newly-unused memory an `mseg_alloc` singleblock carrier is left
    /// unchanged, above it the carrier is shrunk (`erts_alloc.md:438-442`).
    /// `AllctrInit_t::asbcst` → `Allctr_t::mseg_opt.abs_shrink_th`
    /// (`ErtsMsegOpt_t::abs_shrink_th`, `erts/emulator/sys/common/erl_mseg.h:83`;
    /// assigned at `erl_alloc_util.c:6765`, clamped at `6850-6851`). Default
    /// `2*1024*2024` = 4 145 152 bytes — the legacy literal's `2024` is reproduced
    /// verbatim (`erl_alloc_util.h:124`); `0` for `ll_alloc`/`literal_alloc`.
    pub asbcst: usize,
    /// **`rsbcst`** — relative singleblock carrier shrink threshold, in **percent**
    /// (the same shrink decision as [`CarrierPolicy::asbcst`], as the unused *fraction*
    /// of the carrier, `erts_alloc.md:546-550`). `AllctrInit_t::rsbcst` →
    /// `Allctr_t::mseg_opt.rel_shrink_th` (`erl_mseg.h:84`, `erl_alloc_util.c:6766`).
    /// Default `20`; `80` for `sl_alloc`, `90` for `temp_alloc`, `50` for
    /// `eheap_alloc`, `0` for `ll_alloc`/`literal_alloc`.
    pub rsbcst: u32,
    /// **`rsbcmt`** — relative singleblock carrier move threshold, in **percent**: a
    /// block shrunk below [`CarrierPolicy::sbct`] stays in its singleblock carrier
    /// while the freed fraction is below this, and moves to a multiblock carrier above
    /// it (`erts_alloc.md:539-544`). `AllctrInit_t::rsbcmt` →
    /// `Allctr_t::sbc_move_threshold` (`erl_alloc_util.c:6768`, compared at `6322`).
    /// Default `80`; `0` for `ll_alloc`/`literal_alloc`.
    pub rsbcmt: u32,
    /// **`mmsbc`** — maximum number of `mseg_alloc` **singleblock** carriers, an
    /// untyped **amount**: above it new singleblock carriers come from `sys_alloc`
    /// (`erts_alloc.md:524-527`, `erl_alloc_util.c:4206`). `AllctrInit_t::mmsbc` →
    /// `Allctr_t::max_mseg_sbcs` (`erl_alloc_util.c:6771`). Default `256`; `0` for
    /// `ll_alloc`/`literal_alloc`.
    pub mmsbc: usize,
    /// **`mmmbc`** — maximum number of `mseg_alloc` **multiblock** carriers, an untyped
    /// **amount**: above it new multiblock carriers come from `sys_alloc`
    /// (`erts_alloc.md:519-522`, `erl_alloc_util.c:4211`). `AllctrInit_t::mmmbc` →
    /// `Allctr_t::max_mseg_mbcs` (`erl_alloc_util.c:6775`; forced to `~(Uint)0` under
    /// `ERTS_SUPER_ALIGNED_MSEG_ONLY`, `6772-6776`). Default `~(UWord)0`
    /// ([`CarrierPolicy::UNLIMITED`], no limit) for every allocator.
    pub mmmbc: usize,
    /// **`smbcs`** — smallest `mseg_alloc` multiblock carrier size, in **bytes**
    /// (`erts_alloc.md:557-560`). `AllctrInit_t::smbcs` → `Allctr_t::smallest_mbc_size`
    /// (`erl_alloc_util.c:6785`). Default `1024*1024`; `1 << 14` (16 KiB) for
    /// `test_alloc` (`erl_alloc.c:480-489`). The live `largest_mbc_size` is
    /// `MAX(lmbcs, smbcs)` — see [`CarrierPolicy::effective_largest_mbc_size`].
    pub smbcs: usize,
    /// **`lmbcs`** — largest `mseg_alloc` multiblock carrier size, in **bytes**
    /// (`erts_alloc.md:496-500`). `AllctrInit_t::lmbcs` → `Allctr_t::largest_mbc_size`
    /// (`erl_alloc_util.c:6779`). Default `10*1024*1024`; `1 << 14` (16 KiB) for
    /// `test_alloc` (`erl_alloc.c:480-489`); capped to `MBC_SZ_MAX_LIMIT` (64 MB) on
    /// 32-bit (`erl_alloc_util.c:6780-6784`).
    pub lmbcs: usize,
    /// **`acul`** — abandon carrier utilization limit, in **percent**: while non-zero
    /// an allocator may abandon multiblock carriers used below this value into its
    /// carrier pool (`erts_alloc.md:359-385`, `erl_alloc_util.c:3932`), and it is also
    /// the `adjust_carrier_migration_support` switch that enables thread-preferred
    /// instances (`erl_alloc.c:583-603`). `AllctrInit_t::acul` →
    /// `Allctr_t::cpool.util_limit` (`erl_alloc_util.c:6820`; zeroed when the pool is
    /// not armed, `6833-6839`). Default `0`; `ERTS_ALC_DEFAULT_ACUL` = `60` for the six
    /// allocators whose only policy change it is plus `sl_alloc`/`test_alloc`, `85` for
    /// `ll_alloc`, `45` for `eheap_alloc`, `0` for `temp_alloc`
    /// (`erl_alloc.c:76-82`, `236`, `283`, `378`, `700-701`).
    pub acul: u32,
    /// **`acful`** — carrier free utilization limit, in **percent**: at or below it the
    /// unused part of a carrier is offered back to the OS (`madvise(MADV_FREE)`,
    /// `erts_alloc.md:401-419`). `AllctrInit_t::acful` →
    /// `Allctr_t::cpool.free_util_limit` (`erl_alloc_util.c:6821`). Default `0` for
    /// every allocator (`erl_alloc_util.h:135`); the legacy `de` spelling means
    /// `acul - 20` (`ERTS_ALC_DEFAULT_ENABLED_ACFUL`, `erl_alloc.c:84-86`), and
    /// `adjust_carrier_migration_support` lowers it to `acul` when larger
    /// (`erl_alloc.c:600-602`).
    pub acful: u32,
    /// **`acnl`** — abandoned carrier number limit, an untyped **amount**: at most this
    /// many abandoned carriers are kept per allocator instance
    /// (`erts_alloc.md:394-397`, `erl_alloc_util.c:2279`). `AllctrInit_t::acnl` →
    /// `Allctr_t::cpool.in_pool_limit` (`erl_alloc_util.c:6822`). Default `1000` for
    /// every allocator (`erl_alloc_util.h:136`).
    pub acnl: usize,
    /// **`acfml`** — abandoned carrier free block min limit, in **bytes**: a carrier is
    /// only abandoned if its largest free block is at least this large
    /// (`erts_alloc.md:387-392`, `erl_alloc_util.c:2295`). `AllctrInit_t::acfml` →
    /// `Allctr_t::cpool.fblk_min_limit` (`erl_alloc_util.c:6823`). Default `0` for every
    /// allocator (`erl_alloc_util.h:137`).
    pub acfml: usize,
}

impl Default for CarrierPolicy {
    /// The `ERTS_DEFAULT_ALLCTR_INIT` values (`erl_alloc_util.h:110-141`) — the set
    /// every allocator starts from before its `set_default_*_alloc_opts` runs.
    fn default() -> Self {
        CarrierPolicy::ALLCTR_INIT
    }
}

impl CarrierPolicy {
    /// The legacy "no limit" marker, `~((UWord) 0)` in the C (`UWord` is the 64-bit
    /// machine word on the 64-bit oracle). Used for `mmmbc` everywhere and for `sbct`
    /// in `ll_alloc`/`literal_alloc`.
    pub const UNLIMITED: usize = usize::MAX;

    /// The values of `ERTS_DEFAULT_ALLCTR_INIT` (`erl_alloc_util.h:110-141`), the
    /// non-`SMALL_MEMORY` variant: the base of every allocator's `AllctrInit_t`
    /// (`SET_DEFAULT_ALLOC_OPTS`, `erl_alloc.c:214-218`).
    pub const ALLCTR_INIT: CarrierPolicy = CarrierPolicy {
        sbct: 512 * 1024,
        asbcst: 2 * 1024 * 2024,
        rsbcst: 20,
        rsbcmt: 80,
        mmsbc: 256,
        mmmbc: Self::UNLIMITED,
        smbcs: 1024 * 1024,
        lmbcs: 10 * 1024 * 1024,
        acul: 0,
        acful: 0,
        acnl: 1000,
        acfml: 0,
    };

    /// `temp_alloc` — `set_default_temp_alloc_opts` (`erl_alloc.c:340-359`) and the
    /// forced `acul = acful = 0` of `erl_alloc.c:700-701`: only `rsbcst = 90`.
    pub const TEMP_ALLOC: CarrierPolicy = CarrierPolicy {
        rsbcst: 90,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `sl_alloc` — `set_default_sl_alloc_opts` (`erl_alloc.c:220-238`): `rsbcst = 80`,
    /// `acul = ERTS_ALC_DEFAULT_ACUL` (60).
    pub const SL_ALLOC: CarrierPolicy = CarrierPolicy {
        rsbcst: 80,
        acul: 60,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `std_alloc` — `set_default_std_alloc_opts` (`erl_alloc.c:240-257`):
    /// `acul = ERTS_ALC_DEFAULT_ACUL` (60).
    pub const STD_ALLOC: CarrierPolicy = CarrierPolicy {
        acul: 60,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `ll_alloc` — `set_default_ll_alloc_opts` (`erl_alloc.c:259-284`): no singleblock
    /// carriers, no carrier shrink/move, `acul = 85`.
    pub const LL_ALLOC: CarrierPolicy = CarrierPolicy {
        sbct: Self::UNLIMITED,
        asbcst: 0,
        rsbcst: 0,
        rsbcmt: 0,
        mmsbc: 0,
        acul: 85,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `eheap_alloc` — `set_default_eheap_alloc_opts` (`erl_alloc.c:361-379`):
    /// `rsbcst = 50`, `acul = 45`.
    pub const EHEAP_ALLOC: CarrierPolicy = CarrierPolicy {
        rsbcst: 50,
        acul: 45,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `ets_alloc` — `set_default_ets_alloc_opts` (`erl_alloc.c:401-419`): `acul = 60`.
    pub const ETS_ALLOC: CarrierPolicy = CarrierPolicy {
        acul: 60,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `fix_alloc` — `set_default_fix_alloc_opts` (`erl_alloc.c:441-461`): `acul = 60`.
    pub const FIX_ALLOC: CarrierPolicy = CarrierPolicy {
        acul: 60,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `binary_alloc` — `set_default_binary_alloc_opts` (`erl_alloc.c:381-399`):
    /// `acul = 60`.
    pub const BINARY_ALLOC: CarrierPolicy = CarrierPolicy {
        acul: 60,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `driver_alloc` — `set_default_driver_alloc_opts` (`erl_alloc.c:421-439`):
    /// `acul = 60`.
    pub const DRIVER_ALLOC: CarrierPolicy = CarrierPolicy {
        acul: 60,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `literal_alloc` — `set_default_literal_alloc_opts` (`erl_alloc.c:286-338`): like
    /// `ll_alloc`, but `acul = acful = 0` (no carrier migration either).
    pub const LITERAL_ALLOC: CarrierPolicy = CarrierPolicy {
        sbct: Self::UNLIMITED,
        asbcst: 0,
        rsbcst: 0,
        rsbcmt: 0,
        mmsbc: 0,
        acul: 0,
        acful: 0,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// `test_alloc` — `set_default_test_alloc_opts` (`erl_alloc.c:463-490`): `acul = 60`
    /// and a constant minimal multiblock carrier (`smbcs = lmbcs = sbct` =
    /// `ERTS_SACRR_UNIT_SZ` = `1 << 14` = 16 KiB on the 64-bit oracle).
    pub const TEST_ALLOC: CarrierPolicy = CarrierPolicy {
        sbct: 1 << 14,
        smbcs: 1 << 14,
        lmbcs: 1 << 14,
        acul: 60,
        ..CarrierPolicy::ALLCTR_INIT
    };

    /// The legacy default policy of an allocator — the values its
    /// `set_default_*_alloc_opts` function (`erl_alloc.c:220-490`) installs on top of
    /// [`CarrierPolicy::ALLCTR_INIT`], with the value adjustments `erts_alloc_init`
    /// makes afterwards (`erl_alloc.c:699-710`). See the module docs for the full
    /// table.
    ///
    /// `sys_alloc` is not a carrier allocator and owns no `AllctrInit_t`
    /// (`erts_allctrs[ERTS_ALC_A_SYSTEM].alloc = erts_sys_alloc`, `erl_alloc.c:770-773`),
    /// so it yields [`CarrierPolicy::ALLCTR_INIT`] — the values an `AllctrInit_t` would
    /// have carried, not a live configuration. The values are the *configured* ones:
    /// `adjust_sbct` (identity on 64-bit), the `adjust_tpref` division of
    /// `lmbcs`/`smbcs` (`erl_alloc.c:494-513`) and the `cpool` arming test
    /// (`erl_alloc_util.c:6813-6839`) are per-instance runtime concerns (module docs).
    pub const fn for_allocator(allocator: AllocatorId) -> CarrierPolicy {
        match allocator {
            AllocatorId::System => CarrierPolicy::ALLCTR_INIT,
            AllocatorId::Temporary => CarrierPolicy::TEMP_ALLOC,
            AllocatorId::ShortLived => CarrierPolicy::SL_ALLOC,
            AllocatorId::Standard => CarrierPolicy::STD_ALLOC,
            AllocatorId::LongLived => CarrierPolicy::LL_ALLOC,
            AllocatorId::EHeap => CarrierPolicy::EHEAP_ALLOC,
            AllocatorId::Ets => CarrierPolicy::ETS_ALLOC,
            AllocatorId::FixedSize => CarrierPolicy::FIX_ALLOC,
            AllocatorId::Literal => CarrierPolicy::LITERAL_ALLOC,
            AllocatorId::Binary => CarrierPolicy::BINARY_ALLOC,
            AllocatorId::Driver => CarrierPolicy::DRIVER_ALLOC,
            AllocatorId::Test => CarrierPolicy::TEST_ALLOC,
        }
    }

    /// Whether `value` is the legacy "no limit" marker ([`CarrierPolicy::UNLIMITED`],
    /// `~((UWord) 0)`).
    pub const fn is_unlimited(value: usize) -> bool {
        value == CarrierPolicy::UNLIMITED
    }

    /// Whether a request of `size` bytes is served by a dedicated singleblock carrier
    /// (the legacy `size >= allctr->sbc_threshold`, `erl_alloc_util.c:1408`) rather
    /// than from a multiblock carrier.
    pub const fn uses_single_block_carrier(&self, size: usize) -> bool {
        size >= self.sbct
    }

    /// The value of one option, in the unit the field stores it in.
    pub fn get(&self, kind: PolicyOptionKind) -> usize {
        match kind {
            PolicyOptionKind::Sbct => self.sbct,
            PolicyOptionKind::Asbcst => self.asbcst,
            PolicyOptionKind::Rsbcst => self.rsbcst as usize,
            PolicyOptionKind::Rsbcmt => self.rsbcmt as usize,
            PolicyOptionKind::Mmsbc => self.mmsbc,
            PolicyOptionKind::Mmmbc => self.mmmbc,
            PolicyOptionKind::Smbcs => self.smbcs,
            PolicyOptionKind::Lmbcs => self.lmbcs,
            PolicyOptionKind::Acul => self.acul as usize,
            PolicyOptionKind::Acful => self.acful as usize,
            PolicyOptionKind::Acnl => self.acnl,
            PolicyOptionKind::Acfml => self.acfml,
        }
    }

    /// Sets one option, validating `value` in the option's CLI unit
    /// ([`PolicyOptionKind::unit`]): kilobytes for the four size options, bytes for
    /// `acfml`, a percentage in `0..=100` for `acul`/`acful`/`rsbcst`/`rsbcmt`, and a
    /// count for `mmsbc`/`mmmbc`/`acnl`.
    ///
    /// Every rejection is [`BeamError::BadArg`] — the value is never clamped, rounded
    /// or replaced by a default; see [`CarrierPolicy::with_option`] for the one place
    /// this deliberately differs from the legacy CLI, and the module docs for the
    /// legacy ranges (`kb_to_bytes`/`get_amount_value`/`get_acul_value`,
    /// `erl_alloc.c:1157-1280`).
    pub fn set(&mut self, kind: PolicyOptionKind, value: i64) -> Result<()> {
        match kind {
            // `get_kb_value` → `kb_to_bytes` (`erl_alloc.c:1157-1183`): kilobytes,
            // rejected when negative or above `~(Uint)0/1024 + 1`; exactly that
            // maximum means "unlimited" at the 32-bit `Uint` width the C writes.
            PolicyOptionKind::Sbct => self.sbct = kb_to_bytes(kind, value)?,
            PolicyOptionKind::Asbcst => self.asbcst = kb_to_bytes(kind, value)?,
            PolicyOptionKind::Smbcs => self.smbcs = kb_to_bytes(kind, value)?,
            PolicyOptionKind::Lmbcs => self.lmbcs = kb_to_bytes(kind, value)?,
            // `get_amount_value` (`erl_alloc.c:1220-1232`): non-negative, `Uint`-ranged.
            PolicyOptionKind::Acfml => self.acfml = amount(kind, value)?,
            PolicyOptionKind::Mmsbc => self.mmsbc = amount(kind, value)?,
            PolicyOptionKind::Mmmbc => self.mmmbc = amount(kind, value)?,
            PolicyOptionKind::Acnl => self.acnl = amount(kind, value)?,
            // `get_acul_value`/`get_acful_value` (`erl_alloc.c:1234-1280`): `0..=100`.
            PolicyOptionKind::Rsbcst => self.rsbcst = percent(kind, value)?,
            PolicyOptionKind::Rsbcmt => self.rsbcmt = percent(kind, value)?,
            PolicyOptionKind::Acul => self.acul = percent(kind, value)?,
            PolicyOptionKind::Acful => self.acful = percent(kind, value)?,
        }
        Ok(())
    }

    /// [`CarrierPolicy::set`] by legacy option name (`erl_alloc.c:1282-1553`); an
    /// unknown name is [`BeamError::BadArg`].
    pub fn set_option(&mut self, name: &str, value: i64) -> Result<()> {
        match PolicyOptionKind::from_option_name(name) {
            Some(kind) => self.set(kind, value),
            None => Err(unknown_option(name)),
        }
    }

    /// [`CarrierPolicy::set_option`] as a chainable builder (the policy is `Copy`, so
    /// the original value is untouched).
    pub fn with_option(mut self, name: &str, value: i64) -> Result<CarrierPolicy> {
        self.set_option(name, value)?;
        Ok(self)
    }

    /// **`sbct`** — the singleblock carrier threshold in bytes, the value
    /// `Allctr_t::sbc_threshold` holds after `adjust_sbct` (identity on 64-bit).
    pub const fn small_block_carrier_threshold(&self) -> usize {
        self.sbct
    }

    /// **`mmsbc`** — the cap on `mseg_alloc` singleblock carriers
    /// (`Allctr_t::max_mseg_sbcs`); [`CarrierPolicy::UNLIMITED`] means no cap.
    pub const fn single_block_carrier_cap(&self) -> usize {
        self.mmsbc
    }

    /// **`mmmbc`** — the cap on `mseg_alloc` multiblock carriers
    /// (`Allctr_t::max_mseg_mbcs`); [`CarrierPolicy::UNLIMITED`] means no cap (the
    /// default).
    pub const fn multi_block_carrier_cap(&self) -> usize {
        self.mmmbc
    }

    /// **`smbcs`** — the smallest `mseg_alloc` multiblock carrier size in bytes
    /// (`Allctr_t::smallest_mbc_size`).
    pub const fn minimum_multi_block_carrier_size(&self) -> usize {
        self.smbcs
    }

    /// **`lmbcs`** — the largest `mseg_alloc` multiblock carrier size in bytes, 1:1
    /// with the option. The live `Allctr_t::largest_mbc_size` is
    /// `MAX(lmbcs, smbcs)`; use [`CarrierPolicy::effective_largest_mbc_size`] for that.
    pub const fn maximum_multi_block_carrier_size(&self) -> usize {
        self.lmbcs
    }

    /// `Allctr_t::largest_mbc_size` — `MAX(init->lmbcs, init->smbcs)`
    /// (`erl_alloc_util.c:6779`): a `smbcs` above `lmbcs` raises the largest carrier
    /// size to `smbcs` rather than leaving an inverted range.
    pub const fn effective_largest_mbc_size(&self) -> usize {
        if self.smbcs > self.lmbcs {
            self.smbcs
        } else {
            self.lmbcs
        }
    }

    /// **`asbcst`** — the absolute singleblock carrier shrink threshold in bytes
    /// (`Allctr_t::mseg_opt.abs_shrink_th`).
    pub const fn absolute_single_block_carrier_shrink_threshold(&self) -> usize {
        self.asbcst
    }

    /// **`rsbcst`** — the relative singleblock carrier shrink threshold in percent
    /// (`Allctr_t::mseg_opt.rel_shrink_th`).
    pub const fn relative_single_block_carrier_shrink_threshold(&self) -> u32 {
        self.rsbcst
    }

    /// **`rsbcmt`** — the relative singleblock carrier move threshold in percent
    /// (`Allctr_t::sbc_move_threshold`).
    pub const fn relative_single_block_carrier_move_threshold(&self) -> u32 {
        self.rsbcmt
    }

    /// **`acul`** — the abandon carrier utilization limit in percent
    /// (`Allctr_t::cpool.util_limit`).
    pub const fn abandon_carrier_utilization_limit(&self) -> u32 {
        self.acul
    }

    /// **`acful`** — the carrier free utilization limit in percent
    /// (`Allctr_t::cpool.free_util_limit`).
    pub const fn abandon_carrier_free_utilization_limit(&self) -> u32 {
        self.acful
    }

    /// **`acnl`** — the abandoned carrier number limit
    /// (`Allctr_t::cpool.in_pool_limit`).
    pub const fn abandon_carrier_number_limit(&self) -> usize {
        self.acnl
    }

    /// **`acfml`** — the abandoned carrier free block min limit in bytes
    /// (`Allctr_t::cpool.fblk_min_limit`).
    pub const fn abandon_carrier_free_block_min_limit(&self) -> usize {
        self.acfml
    }

    /// Whether carrier migration is enabled, i.e. whether the allocator may abandon
    /// multiblock carriers into its carrier pool. The legacy switch is `acul`
    /// (`adjust_carrier_migration_support`, `erl_alloc.c:583-603`), and the pool is
    /// only armed when `acul` and `acnl` are non-zero as well
    /// (`erl_alloc_util.c:6813`); `acnl` defaults to a practical no-limit `1000`, so
    /// `acul != 0` is the deciding test.
    pub const fn carrier_migration_enabled(&self) -> bool {
        self.acul != 0
    }
}

/// The largest value the legacy `get_amount_value` accepts: the C stores its result in
/// a 32-bit `Uint` (`erl_alloc.c:1220-1232`), so anything above `u32::MAX` is an
/// impossible value rather than a valid one.
const MAX_AMOUNT: i64 = u32::MAX as i64;

/// The largest kilobyte count `kb_to_bytes` accepts, `(~((Uint) 0))/1024 + 1`
/// (`erl_alloc.c:1157-1168`); this exact value spells "unlimited".
const MAX_KB: i64 = (u32::MAX as i64) / 1024 + 1;

/// Logs the rejected option value and yields [`BeamError::BadArg`] (the legacy
/// `bad_value`, `erl_alloc.c:1140-1155`).
fn bad_value(kind: PolicyOptionKind, value: i64) -> BeamError {
    beam_common::beam_debug!(
        "beam_alloc: rejected {}={} ({} option)",
        kind.option_name(),
        value,
        kind.allctr_field()
    );
    BeamError::BadArg
}

/// The unknown-name counterpart of [`bad_value`].
fn unknown_option(name: &str) -> BeamError {
    beam_common::beam_debug!("beam_alloc: unknown carrier policy option {name:?}");
    BeamError::BadArg
}

/// Mirrors `kb_to_bytes` (`erl_alloc.c:1157-1168`): kilobytes in, bytes out, with the
/// maximum representable kilobyte count meaning `~((Uint) 0)` ("unlimited" at the
/// width the C stores).
fn kb_to_bytes(kind: PolicyOptionKind, kb: i64) -> Result<usize> {
    if kb < 0 || kb > MAX_KB {
        return Err(bad_value(kind, kb));
    }
    if kb == MAX_KB {
        return Ok(u32::MAX as usize);
    }
    Ok((kb as usize) * 1024)
}

/// Mirrors `get_amount_value` (`erl_alloc.c:1220-1232`): a non-negative count that
/// fits the legacy 32-bit `Uint`.
fn amount(kind: PolicyOptionKind, value: i64) -> Result<usize> {
    if !(0..=MAX_AMOUNT).contains(&value) {
        return Err(bad_value(kind, value));
    }
    Ok(value as usize)
}

/// Mirrors `get_acul_value`/`get_acful_value` (`erl_alloc.c:1234-1280`): an integer
/// percentage in `0..=100`.
///
/// `rsbcst`/`rsbcmt` are handled by the same validation, which is *stricter* than the
/// legacy CLI: `handle_au_arg` silently clamps those two to 100
/// (`erl_alloc.c:1498-1504`), whereas a typed policy value is required to be exactly
/// what was asked for, so an out-of-range value is rejected rather than clamped.
fn percent(kind: PolicyOptionKind, value: i64) -> Result<u32> {
    if !(0..=100).contains(&value) {
        return Err(bad_value(kind, value));
    }
    Ok(value as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The twelve fields as one row, in [`PolicyOptionKind::ALL`] order, so a test can
    /// pin a whole allocator's policy with one literal.
    fn row(allocator: AllocatorId) -> [usize; 12] {
        let policy = CarrierPolicy::for_allocator(allocator);
        PolicyOptionKind::ALL.map(|kind| policy.get(kind))
    }

    #[test]
    fn defaults_are_the_legacy_default_allctr_init_values() {
        let p = CarrierPolicy::default();
        assert_eq!(p, CarrierPolicy::ALLCTR_INIT);
        // ERTS_DEFAULT_ALLCTR_INIT, erl_alloc_util.h:110-141 (non-SMALL_MEMORY).
        assert_eq!(p.sbct, 512 * 1024, "sbct");
        assert_eq!(p.asbcst, 2 * 1024 * 2024, "asbcst, the legacy literal");
        assert_eq!(p.asbcst, 4_145_152, "asbcst is 4 145 152 bytes, not 2 MiB");
        assert_eq!(p.rsbcst, 20, "rsbcst");
        assert_eq!(p.rsbcmt, 80, "rsbcmt");
        assert_eq!(p.mmsbc, 256, "mmsbc");
        assert_eq!(p.mmmbc, CarrierPolicy::UNLIMITED, "mmmbc is ~((UWord)0)");
        assert_eq!(p.mmmbc, usize::MAX);
        assert_eq!(p.smbcs, 1024 * 1024, "smbcs");
        assert_eq!(p.lmbcs, 10 * 1024 * 1024, "lmbcs");
        assert_eq!(p.acul, 0, "acul");
        assert_eq!(p.acful, 0, "acful");
        assert_eq!(p.acnl, 1000, "acnl");
        assert_eq!(p.acfml, 0, "acfml");
        // The three size options agree with the size-class ladder's defaults, which
        // were derived from the same legacy constants (see `crate::size_class`).
        assert_eq!(p.sbct, crate::size_class::SBC_THRESHOLD_DEFAULT);
        assert_eq!(p.smbcs, crate::size_class::SMALLEST_MBC_SIZE_DEFAULT);
        assert_eq!(p.lmbcs, crate::size_class::MBC_THRESHOLD_DEFAULT);
    }

    #[test]
    fn only_the_documented_allocators_deviate_from_the_base_set() {
        // The default set is per allocator, not shared: at least, e.g., ll_alloc and
        // test_alloc must differ from it.
        assert_ne!(row(AllocatorId::LongLived), row(AllocatorId::Test));
        assert_ne!(CarrierPolicy::for_allocator(AllocatorId::Test), CarrierPolicy::default());
        // The allocators whose only policy change is `acul = 60` share one row.
        let base_with_acul60 = row(AllocatorId::Standard);
        for allocator in [
            AllocatorId::Ets,
            AllocatorId::FixedSize,
            AllocatorId::Binary,
            AllocatorId::Driver,
        ] {
            assert_eq!(
                row(allocator),
                base_with_acul60,
                "{} shares std_alloc's built-in default set",
                allocator.as_str()
            );
        }
    }

    #[test]
    fn per_allocator_defaults_match_the_legacy_set_default_functions() {
        const K512: usize = 512 * 1024;
        const ASBCST: usize = 2 * 1024 * 2024;
        const MIB: usize = 1024 * 1024;
        const MIB10: usize = 10 * 1024 * 1024;
        let unlim = CarrierPolicy::UNLIMITED;

        // Order: sbct, asbcst, rsbcst, rsbcmt, mmsbc, mmmbc, smbcs, lmbcs, acul,
        // acful, acnl, acfml.
        assert_eq!(
            row(AllocatorId::System),
            [K512, ASBCST, 20, 80, 256, unlim, MIB, MIB10, 0, 0, 1000, 0],
            "sys_alloc has no Allctr_t; it inherits the AllctrInit_t base set"
        );
        assert_eq!(
            row(AllocatorId::Temporary),
            [K512, ASBCST, 90, 80, 256, unlim, MIB, MIB10, 0, 0, 1000, 0],
            "temp_alloc: rsbcst = 90 (erl_alloc.c:357), acul forced to 0 (erl_alloc.c:700)"
        );
        assert_eq!(
            row(AllocatorId::ShortLived),
            [K512, ASBCST, 80, 80, 256, unlim, MIB, MIB10, 60, 0, 1000, 0],
            "sl_alloc: rsbcst = 80 (erl_alloc.c:236), acul = 60 (erl_alloc.c:237)"
        );
        assert_eq!(
            row(AllocatorId::Standard),
            [K512, ASBCST, 20, 80, 256, unlim, MIB, MIB10, 60, 0, 1000, 0],
            "std_alloc: acul = 60 (erl_alloc.c:256)"
        );
        assert_eq!(
            row(AllocatorId::LongLived),
            [unlim, 0, 0, 0, 0, unlim, MIB, MIB10, 85, 0, 1000, 0],
            "ll_alloc: no sbcs, no shrink/move thresholds, acul = 85 (erl_alloc.c:268-283)"
        );
        assert_eq!(
            row(AllocatorId::EHeap),
            [K512, ASBCST, 50, 80, 256, unlim, MIB, MIB10, 45, 0, 1000, 0],
            "eheap_alloc: rsbcst = 50, acul = 45 (erl_alloc.c:377-378)"
        );
        assert_eq!(
            row(AllocatorId::Ets),
            [K512, ASBCST, 20, 80, 256, unlim, MIB, MIB10, 60, 0, 1000, 0],
            "ets_alloc: acul = 60 (erl_alloc.c:418)"
        );
        assert_eq!(
            row(AllocatorId::FixedSize),
            [K512, ASBCST, 20, 80, 256, unlim, MIB, MIB10, 60, 0, 1000, 0],
            "fix_alloc: acul = 60 (erl_alloc.c:460)"
        );
        assert_eq!(
            row(AllocatorId::Literal),
            [unlim, 0, 0, 0, 0, unlim, MIB, MIB10, 0, 0, 1000, 0],
            "literal_alloc: like ll_alloc but acul = acful = 0 (erl_alloc.c:302-317)"
        );
        assert_eq!(
            row(AllocatorId::Binary),
            [K512, ASBCST, 20, 80, 256, unlim, MIB, MIB10, 60, 0, 1000, 0],
            "binary_alloc: acul = 60 (erl_alloc.c:397)"
        );
        assert_eq!(
            row(AllocatorId::Driver),
            [K512, ASBCST, 20, 80, 256, unlim, MIB, MIB10, 60, 0, 1000, 0],
            "driver_alloc: acul = 60 (erl_alloc.c:437)"
        );
        assert_eq!(
            row(AllocatorId::Test),
            [1 << 14, ASBCST, 20, 80, 256, unlim, 1 << 14, 1 << 14, 60, 0, 1000, 0],
            "test_alloc: constant minimal mbc size, acul = 60 (erl_alloc.c:477-489)"
        );
    }

    #[test]
    fn every_allocator_has_an_unlimited_multiblock_carrier_cap() {
        // `mmmbc` is `~((UWord)0)` for every allocator (none overrides it).
        for allocator in AllocatorId::ALL {
            let p = CarrierPolicy::for_allocator(allocator);
            assert_eq!(p.multi_block_carrier_cap(), CarrierPolicy::UNLIMITED);
            assert!(CarrierPolicy::is_unlimited(p.multi_block_carrier_cap()));
            assert!(!CarrierPolicy::is_unlimited(p.multi_block_carrier_cap() - 1));
        }
    }

    #[test]
    fn option_names_are_the_legacy_plus_m_switches() {
        // The twelve names of the objective/legacy option set, in ALL order.
        let names: Vec<&str> = PolicyOptionKind::ALL
            .iter()
            .map(|kind| kind.option_name())
            .collect();
        assert_eq!(
            names,
            vec![
                "sbct", "asbcst", "rsbcst", "rsbcmt", "mmsbc", "mmmbc", "smbcs", "lmbcs", "acul",
                "acful", "acnl", "acfml",
            ]
        );
    }

    #[test]
    fn option_names_parse_to_the_right_kinds() {
        for kind in PolicyOptionKind::ALL {
            assert_eq!(
                PolicyOptionKind::from_option_name(kind.option_name()),
                Some(kind),
                "{} must round-trip",
                kind.option_name()
            );
        }
        assert_eq!(
            PolicyOptionKind::from_option_name("sbct"),
            Some(PolicyOptionKind::Sbct)
        );
        assert_eq!(
            PolicyOptionKind::from_option_name("acfml"),
            Some(PolicyOptionKind::Acfml)
        );
        assert_eq!(
            PolicyOptionKind::from_option_name("mmmbc"),
            Some(PolicyOptionKind::Mmmbc)
        );
    }

    #[test]
    fn unknown_or_mis_cased_option_names_are_rejected() {
        for bad in [
            "", "sbct ", " sbct", "SBCT", "Sbct", "asbcst ", "mmbcs", "mbcgs", "mbsd", "rmbcmt",
            "ramv", "atags", "cp", "t", "e", "as", "acfmlx", "acul!",
        ] {
            assert_eq!(
                PolicyOptionKind::from_option_name(bad),
                None,
                "{bad:?} is not one of the twelve policy options"
            );
        }
    }

    #[test]
    fn option_groups_follow_the_legacy_grouping() {
        assert_eq!(
            PolicyOptionKind::CARRIER_POOL_THRESHOLDS.map(|k| k.option_name()),
            ["sbct", "asbcst", "rsbcst", "rsbcmt", "mmsbc", "mmmbc", "smbcs", "lmbcs"]
        );
        assert_eq!(
            PolicyOptionKind::UTILIZATION_LIMITS.map(|k| k.option_name()),
            ["acul", "acful", "acnl", "acfml"]
        );
        // ALL is the two groups concatenated.
        let all: Vec<PolicyOptionKind> = PolicyOptionKind::CARRIER_POOL_THRESHOLDS
            .into_iter()
            .chain(PolicyOptionKind::UTILIZATION_LIMITS)
            .collect();
        assert_eq!(all, PolicyOptionKind::ALL.to_vec());
        assert_eq!(PolicyOptionKind::ALL.len(), 12);

        for kind in PolicyOptionKind::CARRIER_POOL_THRESHOLDS {
            assert_eq!(kind.group(), PolicyGroup::CarrierPoolThresholds);
        }
        for kind in PolicyOptionKind::UTILIZATION_LIMITS {
            assert_eq!(kind.group(), PolicyGroup::UtilizationLimits);
        }
        assert_eq!(PolicyOptionKind::Acnl.group(), PolicyGroup::UtilizationLimits);
        assert_eq!(PolicyOptionKind::Sbct.group(), PolicyGroup::CarrierPoolThresholds);
    }

    #[test]
    fn option_units_are_the_legacy_units() {
        use PolicyUnit::*;
        assert_eq!(PolicyOptionKind::Sbct.unit(), Kilobytes);
        assert_eq!(PolicyOptionKind::Asbcst.unit(), Kilobytes);
        assert_eq!(PolicyOptionKind::Lmbcs.unit(), Kilobytes);
        assert_eq!(PolicyOptionKind::Smbcs.unit(), Kilobytes);
        assert_eq!(PolicyOptionKind::Acfml.unit(), Bytes);
        for kind in [
            PolicyOptionKind::Rsbcst,
            PolicyOptionKind::Rsbcmt,
            PolicyOptionKind::Acul,
            PolicyOptionKind::Acful,
        ] {
            assert_eq!(kind.unit(), Percent, "{}", kind.option_name());
            assert!(kind.is_percent());
        }
        for kind in [
            PolicyOptionKind::Mmsbc,
            PolicyOptionKind::Mmmbc,
            PolicyOptionKind::Acnl,
        ] {
            assert_eq!(kind.unit(), Amount, "{}", kind.option_name());
            assert!(!kind.is_percent());
        }
    }

    #[test]
    fn legacy_identifiers_are_documented() {
        // The `AllctrInit_t` field of every option is the option name itself
        // (erl_alloc_util.h:61-75), and the live `Allctr_t` member is the one
        // erts_alcu_start copies into (erl_alloc_util.c:6762-6785, 6820-6823).
        assert_eq!(PolicyOptionKind::Sbct.init_field(), "sbct");
        assert_eq!(PolicyOptionKind::Sbct.allctr_field(), "sbc_threshold");
        assert_eq!(PolicyOptionKind::Asbcst.allctr_field(), "mseg_opt.abs_shrink_th");
        assert_eq!(PolicyOptionKind::Rsbcst.allctr_field(), "mseg_opt.rel_shrink_th");
        assert_eq!(PolicyOptionKind::Rsbcmt.allctr_field(), "sbc_move_threshold");
        assert_eq!(PolicyOptionKind::Mmsbc.allctr_field(), "max_mseg_sbcs");
        assert_eq!(PolicyOptionKind::Mmmbc.allctr_field(), "max_mseg_mbcs");
        assert_eq!(PolicyOptionKind::Smbcs.allctr_field(), "smallest_mbc_size");
        assert_eq!(PolicyOptionKind::Lmbcs.allctr_field(), "largest_mbc_size");
        assert_eq!(PolicyOptionKind::Acul.allctr_field(), "cpool.util_limit");
        assert_eq!(PolicyOptionKind::Acful.allctr_field(), "cpool.free_util_limit");
        assert_eq!(PolicyOptionKind::Acnl.allctr_field(), "cpool.in_pool_limit");
        assert_eq!(PolicyOptionKind::Acfml.allctr_field(), "cpool.fblk_min_limit");
    }

    #[test]
    fn kb_options_convert_to_bytes_as_kb_to_bytes_does() {
        // get_kb_value -> kb_to_bytes (erl_alloc.c:1157-1183): value * 1024 bytes.
        let p = CarrierPolicy::default()
            .with_option("sbct", 512)
            .unwrap()
            .with_option("smbcs", 1024)
            .unwrap()
            .with_option("lmbcs", 4 * 1024)
            .unwrap()
            .with_option("asbcst", 0)
            .unwrap();
        assert_eq!(p.sbct, 512 * 1024);
        assert_eq!(p.smbcs, 1024 * 1024);
        assert_eq!(p.lmbcs, 4 * 1024 * 1024);
        assert_eq!(p.asbcst, 0);
        // The derived accessors reflect the fields (the 1:1 mapping).
        assert_eq!(p.small_block_carrier_threshold(), 512 * 1024);
        assert_eq!(p.minimum_multi_block_carrier_size(), 1024 * 1024);
        assert_eq!(p.maximum_multi_block_carrier_size(), 4 * 1024 * 1024);
        assert_eq!(p.absolute_single_block_carrier_shrink_threshold(), 0);
    }

    #[test]
    fn kb_options_reject_impossible_values() {
        // kb_to_bytes: negative, and above (~((Uint)0)/1024) + 1, are bad_value.
        for option in ["sbct", "asbcst", "smbcs", "lmbcs"] {
            for bad in [-1, -1024, MAX_KB + 1, MAX_KB + 2, i64::MAX] {
                assert!(
                    matches!(
                        CarrierPolicy::default().with_option(option, bad),
                        Err(BeamError::BadArg)
                    ),
                    "{option}={bad} must be rejected"
                );
            }
        }
        // The maximum accepted kilobyte count spells "unlimited" at the legacy 32-bit
        // width (`*bytes = ~((Uint) 0)`, erl_alloc.c:1163-1164).
        let p = CarrierPolicy::default().with_option("sbct", MAX_KB).unwrap();
        assert_eq!(p.sbct, u32::MAX as usize);
    }

    #[test]
    fn amount_and_byte_options_reject_impossible_values() {
        // get_amount_value (erl_alloc.c:1220-1232) rejects negatives and stores into a
        // 32-bit Uint.
        for option in ["mmsbc", "mmmbc", "acnl", "acfml"] {
            for bad in [-1, -1000, MAX_AMOUNT + 1, i64::MAX] {
                assert!(
                    matches!(
                        CarrierPolicy::default().with_option(option, bad),
                        Err(BeamError::BadArg)
                    ),
                    "{option}={bad} must be rejected"
                );
            }
            assert!(CarrierPolicy::default().with_option(option, 0).is_ok());
            assert!(CarrierPolicy::default().with_option(option, MAX_AMOUNT).is_ok());
        }
        let p = CarrierPolicy::default()
            .with_option("mmsbc", 0)
            .unwrap()
            .with_option("acfml", 4096)
            .unwrap()
            .with_option("acnl", 1000)
            .unwrap();
        assert_eq!(p.single_block_carrier_cap(), 0);
        assert_eq!(p.abandon_carrier_free_block_min_limit(), 4096);
        assert_eq!(p.abandon_carrier_number_limit(), 1000);
    }

    #[test]
    fn percent_options_reject_out_of_range_values() {
        // get_acul_value/get_acful_value reject outside 0..=100
        // (erl_alloc.c:1253-1254), and this typed setter applies the same rule to
        // rsbcst/rsbcmt (which the CLI would clamp, erl_alloc.c:1498-1504).
        for option in ["acul", "acful", "rsbcst", "rsbcmt"] {
            for bad in [-1, -100, 101, 1000, i64::MAX] {
                assert!(
                    matches!(
                        CarrierPolicy::default().with_option(option, bad),
                        Err(BeamError::BadArg)
                    ),
                    "{option}={bad} must be rejected"
                );
            }
            for ok in [0, 1, 45, 60, 85, 100] {
                let p = CarrierPolicy::default().with_option(option, ok).unwrap();
                let kind = PolicyOptionKind::from_option_name(option).unwrap();
                assert_eq!(p.get(kind), ok as usize);
            }
        }
    }

    #[test]
    fn unknown_option_names_are_badarg_and_never_touch_the_policy() {
        let p = CarrierPolicy::default();
        for bad in ["", "mmsbcx", "rmbcmt", "mmbcs", "MBcgs"] {
            assert!(
                matches!(p.with_option(bad, 1), Err(BeamError::BadArg)),
                "{bad:?} must be rejected"
            );
            let mut mutated = p;
            assert!(matches!(mutated.set_option(bad, 1), Err(BeamError::BadArg)));
            assert_eq!(mutated, p, "{bad:?} must not change any field");
        }
    }

    #[test]
    fn setting_an_option_changes_only_its_field() {
        for kind in PolicyOptionKind::ALL {
            let value = match kind.unit() {
                PolicyUnit::Percent => 7,
                PolicyUnit::Kilobytes => 2, // -> 2048 bytes
                PolicyUnit::Bytes | PolicyUnit::Amount => 3,
            };
            let mut p = CarrierPolicy::default();
            p.set(kind, value).unwrap();
            for other in PolicyOptionKind::ALL {
                let expected = if other == kind {
                    match kind.unit() {
                        PolicyUnit::Kilobytes => (value as usize) * 1024,
                        _ => value as usize,
                    }
                } else {
                    CarrierPolicy::ALLCTR_INIT.get(other)
                };
                assert_eq!(p.get(other), expected, "{} after setting {}", other.option_name(), kind.option_name());
            }
        }
    }

    #[test]
    fn set_option_and_with_option_agree_with_the_typed_setter() {
        let mut mutated = CarrierPolicy::default();
        mutated.set_option("sbct", 256).unwrap();
        mutated.set_option("acul", 60).unwrap();
        let chained = CarrierPolicy::default()
            .with_option("sbct", 256)
            .unwrap()
            .with_option("acul", 60)
            .unwrap();
        assert_eq!(mutated.sbct, 256 * 1024);
        assert_eq!(mutated.acul, 60);
        assert_eq!(chained, mutated);
        assert_eq!(chained.small_block_carrier_threshold(), 256 * 1024);
        assert_eq!(chained.abandon_carrier_utilization_limit(), 60);
        assert_ne!(chained, CarrierPolicy::default());
        // The typed `set` is the same code path as the name-based `set_option`.
        let mut typed = chained;
        typed.set(PolicyOptionKind::Mmsbc, 4).unwrap();
        assert_eq!(typed, chained.with_option("mmsbc", 4).unwrap());
        assert_eq!(typed.single_block_carrier_cap(), 4);
    }

    #[test]
    fn derived_accessors_reflect_the_fields() {
        let p = CarrierPolicy::for_allocator(AllocatorId::ShortLived)
            .with_option("sbct", 1024)
            .unwrap()
            .with_option("mmsbc", 8)
            .unwrap()
            .with_option("mmmbc", 16)
            .unwrap()
            .with_option("smbcs", 512)
            .unwrap()
            .with_option("lmbcs", 2048)
            .unwrap()
            .with_option("asbcst", 4)
            .unwrap()
            .with_option("rsbcst", 33)
            .unwrap()
            .with_option("rsbcmt", 44)
            .unwrap()
            .with_option("acul", 55)
            .unwrap()
            .with_option("acful", 11)
            .unwrap()
            .with_option("acnl", 22)
            .unwrap()
            .with_option("acfml", 6)
            .unwrap();
        assert_eq!(p.small_block_carrier_threshold(), 1024 * 1024);
        assert_eq!(p.single_block_carrier_cap(), 8);
        assert_eq!(p.multi_block_carrier_cap(), 16);
        assert_eq!(p.minimum_multi_block_carrier_size(), 512 * 1024);
        assert_eq!(p.maximum_multi_block_carrier_size(), 2048 * 1024);
        assert_eq!(p.absolute_single_block_carrier_shrink_threshold(), 4 * 1024);
        assert_eq!(p.relative_single_block_carrier_shrink_threshold(), 33);
        assert_eq!(p.relative_single_block_carrier_move_threshold(), 44);
        assert_eq!(p.abandon_carrier_utilization_limit(), 55);
        assert_eq!(p.abandon_carrier_free_utilization_limit(), 11);
        assert_eq!(p.abandon_carrier_number_limit(), 22);
        assert_eq!(p.abandon_carrier_free_block_min_limit(), 6);
        // The raw fields and the accessors are 1:1.
        assert_eq!(p.small_block_carrier_threshold(), p.sbct);
        assert_eq!(p.maximum_multi_block_carrier_size(), p.lmbcs);
        assert_eq!(p.abandon_carrier_number_limit(), p.acnl);

        // Setting sbct moves the singleblock-carrier decision with it
        // (`size >= sbc_threshold`, erl_alloc_util.c:1408).
        let p = CarrierPolicy::default().with_option("sbct", 64).unwrap();
        assert_eq!(p.small_block_carrier_threshold(), 64 * 1024);
        assert!(!p.uses_single_block_carrier(64 * 1024 - 1));
        assert!(p.uses_single_block_carrier(64 * 1024));
        assert!(p.uses_single_block_carrier(64 * 1024 + 1));
        // The default threshold: 512 KiB, so a 1 MiB request always gets an sbc.
        assert!(CarrierPolicy::default().uses_single_block_carrier(1024 * 1024));
        assert!(!CarrierPolicy::default().uses_single_block_carrier(512 * 1024 - 1));
    }

    #[test]
    fn effective_largest_mbc_size_is_the_max_of_lmbcs_and_smbcs() {
        // Allctr_t::largest_mbc_size = MAX(init->lmbcs, init->smbcs)
        // (erl_alloc_util.c:6779).
        let p = CarrierPolicy::default();
        assert_eq!(p.maximum_multi_block_carrier_size(), 10 * 1024 * 1024);
        assert_eq!(p.effective_largest_mbc_size(), 10 * 1024 * 1024);

        let lmbcs_below = p.with_option("smbcs", 16 * 1024).unwrap(); // 16 MiB
        assert_eq!(lmbcs_below.minimum_multi_block_carrier_size(), 16 * 1024 * 1024);
        assert_eq!(lmbcs_below.maximum_multi_block_carrier_size(), 10 * 1024 * 1024);
        assert_eq!(
            lmbcs_below.effective_largest_mbc_size(),
            16 * 1024 * 1024,
            "largest_mbc_size is raised to smbcs, never left inverted"
        );

        let lmbcs_above = p.with_option("lmbcs", 32 * 1024).unwrap(); // 32 MiB
        assert_eq!(lmbcs_above.effective_largest_mbc_size(), 32 * 1024 * 1024);
        assert_eq!(lmbcs_above.maximum_multi_block_carrier_size(), 32 * 1024 * 1024);
        // `test_alloc` pins smbcs == lmbcs, so the two agree there too.
        let test = CarrierPolicy::for_allocator(AllocatorId::Test);
        assert_eq!(test.effective_largest_mbc_size(), test.lmbcs);
        assert_eq!(test.lmbcs, 1 << 14);
    }

    #[test]
    fn carrier_migration_follows_acul() {
        // adjust_carrier_migration_support: only a non-zero acul arms the carrier pool
        // (erl_alloc.c:583-603, erl_alloc_util.c:6813).
        assert!(!CarrierPolicy::default().carrier_migration_enabled());
        assert!(!CarrierPolicy::for_allocator(AllocatorId::Temporary).carrier_migration_enabled());
        assert!(!CarrierPolicy::for_allocator(AllocatorId::Literal).carrier_migration_enabled());
        for (allocator, acul) in [
            (AllocatorId::ShortLived, 60),
            (AllocatorId::Standard, 60),
            (AllocatorId::LongLived, 85),
            (AllocatorId::EHeap, 45),
            (AllocatorId::Binary, 60),
            (AllocatorId::Test, 60),
        ] {
            let p = CarrierPolicy::for_allocator(allocator);
            assert_eq!(p.abandon_carrier_utilization_limit(), acul);
            assert!(
                p.carrier_migration_enabled(),
                "{} enables carrier migration",
                allocator.as_str()
            );
        }
        // Enabling it on a default policy (as `+Mu`/`+M<S>acul` would).
        let enabled = CarrierPolicy::default().with_option("acul", 100).unwrap();
        assert!(enabled.carrier_migration_enabled());
        let disabled = enabled.with_option("acul", 0).unwrap();
        assert!(!disabled.carrier_migration_enabled());
    }

    #[test]
    fn policies_are_copy_and_comparable() {
        let a = CarrierPolicy::for_allocator(AllocatorId::EHeap);
        let b = a; // Copy
        let c = a.with_option("acul", 45).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, c, "setting acul to its current value is a no-op");
        assert_ne!(a, a.with_option("acul", 46).unwrap());
        let dbg = format!("{a:?}");
        assert!(dbg.starts_with("CarrierPolicy {"), "{dbg}");
        assert!(dbg.contains("sbct: 524288"), "{dbg}");
        assert!(dbg.contains("mmsbc: 256"), "{dbg}");
    }
}
