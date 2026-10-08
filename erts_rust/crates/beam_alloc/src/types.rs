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

//! The `erts_alloc` allocation-type inventory.
//!
//! This module is the Rust counterpart of the *declaration data* half of the
//! legacy allocator: the **allocator** declarations, the **class** declarations
//! and the complete `type` table of
//! [`../../erts/emulator/beam/erl_alloc.types`](../../erts/emulator/beam/erl_alloc.types)
//! (the source file consumed by the `erts/emulator/utils/make_alloc_types`
//! generator, which emits the C `ERTS_ALC_A_*`, `ERTS_ALC_C_*` and
//! `ERTS_ALC_T_*` macros and the type-number helpers used by `erts_alloc`).
//!
//! # Why the table is one module
//!
//! [`ALLOC_TYPES`] is a hand-maintained table of **243** rows copied in *file
//! order* from that ground-truth file. The order *is* the allocator-type
//! numbering (`ERTS_ALC_T_*` is assigned in declaration order by the
//! generator), and the table's invariants (unique names, complete
//! allocator/class coverage) are only reviewable with the whole table in one
//! place — so the table deliberately lives in a single module; its unit tests
//! sit in the sibling `tests` module.
//!
//! # The filter applied (which declarations are active)
//!
//! One legacy build configuration is modelled: **`unix` enabled, `win32`
//! disabled, `lcnt` disabled and `beamasm` enabled** — the configuration of the
//! system-OTP differential oracle. `erl_alloc.types` contains 249 `type` lines;
//! that configuration activates **243** of them, i.e. all 249 minus
//!
//! * the `+ifnot beamasm` type `CODE` (1 row),
//! * the `+if lcnt` types `LCNT_CARRIER` and `LCNT_VECTOR` (2 rows),
//! * the `+if win32` types `DRV_DATA_BUF`, `PRELOADED` and `WAITER_OBJ` (3 rows),
//!
//! and including the five `+if unix` types `SYS_READ_BUF`, `FD_ENTRY_BUF`,
//! `CS_PROG_PATH`, `SYS_BLOCKING` and `SYS_WRITE_BUF`. The arithmetic is
//! recorded in [`EXPECTED_TYPE_COUNT`] and pinned by a compile-time assertion
//! against [`TYPE_COUNT`].
//!
//! # The `TypeFlags` model and its derivation from the legacy source
//!
//! The legacy framework does not attach attributes to a *type* directly; a type
//! names an **allocator** (`type <TYPE> <ALLOCATOR> <CLASS> <DESCRIPTION>`), and
//! the allocator's configuration (`AllctrInit_t` in `erl_alloc_util.h` plus the
//! per-allocator overrides in `erl_alloc.c`) determines how its memory is
//! obtained. [`TypeFlags`] is that allocator configuration projected onto every
//! type bound to that allocator, so that a Rust caller can ask "is this type's
//! memory carved out of carriers / is it fixed-size / is it a process heap?"
//! without walking the allocator table. Every bit is hand-derived from a
//! concrete C symbol — nothing is invented:
//!
//! | Bit | Legacy anchor | Rule used here |
//! |---|---|---|
//! | [`TypeFlags::MSEG`] | `Allctr_t::mseg_alloc`, the carrier source (`erl_alloc_util.h:81`); when the init does not override it, `erts_alcu_init` installs the default `erts_alcu_mseg_alloc` (`erl_alloc_util.c:6901-6914`). The `SYSTEM` allocator is *not* a carrier allocator at all: it is the raw `erts_sys_alloc` malloc wrapper (`erl_alloc.c:770-773`). | set for every allocator except `SYSTEM` |
//! | [`TypeFlags::SUPER_ALIGNED`] | super-aligned carriers: `ERTS_HAVE_MSEG_SUPER_ALIGNED` / `ERTS_SUPER_ALIGN_BITS` (`erl_alloc_util.h:323-349`) and the `superaligned` carrier teardown path (`erl_alloc_util.c:4489`). The only allocator whose init installs a super-aligning mmapper-backed `mseg_alloc` is the literal allocator (`erl_alloc.c:328-334`, `erts_alcu_mmapper_mseg_alloc` / `erts_literal_mmapper`). | set for the `LITERAL` allocator |
//! | [`TypeFlags::FIXED_SIZE`] | the fixed-size allocator: `ERTS_ALC_A_FIXED_SIZE` and the `ERTS_ALC_IS_FIX_TYPE` / `ERTS_ALC_FIX_TYPE_IX` macros (`erl_alloc.h:67-76`), configured by `set_default_fix_alloc_opts` (`erl_alloc.c`). | set for the `FIXED_SIZE` allocator |
//! | [`TypeFlags::HEAP`] | the process-heap allocator: `ERTS_ALC_A_EHEAP` and its `ERTS_ALC_DEFAULT_ENABLED_ACUL_EHEAP_ALLOC` carrier-utilization limit (`erl_alloc.c:77-85`), configured by `set_default_eheap_alloc_opts`. | set for the `EHEAP` allocator |
//! | [`TypeFlags::SBC`] | **s**ingle-**b**lock **c**arriers: `IS_SBC_BLK` / `SBC_BLK_SZ` and the `sbct` "sbc threshold" option (`erl_alloc_util.h:412-425`, `:123`). Every carrier-backed allocator can place a large block in its own SBC. | set for every allocator except `SYSTEM` |
//! | [`TypeFlags::MBC`] | **m**ulti-**b**lock **c**arriers: `IS_MBC_BLK` / `MBC_BLK_SZ` and the main-carrier options `mmbc0` / `mmbcs` (`erl_alloc_util.h:421-443`, `:122`, `:128`). Every carrier-backed allocator subdivides a main MBC into same-size blocks. | set for every allocator except `SYSTEM` |
//!
//! Because the three capability bits `MSEG`, `SBC` and `MBC` are properties of
//! *every* carrier-backed allocator in the modelled configuration, their
//! extensions coincide today (all allocators except `SYSTEM`) — they are
//! nevertheless modelled as independent bits because the legacy code path that
//! consumes them (`create_carrier` choosing `CFLG_FORCE_MSEG` vs
//! `CFLG_FORCE_SYS_ALLOC`, and `sbct` deciding SBC vs MBC) tests them
//! independently.
//!
//! The single source of truth for the mapping is [`TypeFlags::for_allocator`];
//! the table stores the result of that function (which a test re-checks), so the
//! mapping cannot silently drift from the documentation above.

use core::fmt;

// ---------------------------------------------------------------------------
// Allocators
// ---------------------------------------------------------------------------

/// An allocator instance id.
///
/// Exactly the twelve `allocator` declarations of
/// `erts/emulator/beam/erl_alloc.types`, in declaration order — which is also
/// the ascending `ERTS_ALC_A_*` numbering assigned by `make_alloc_types`
/// (`SYSTEM` = 1, `TEST` = 12; [`AllocatorId::index`] is that number minus one).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum AllocatorId {
    /// `sys_alloc` — long-lived, system-wide allocations (the raw system
    /// allocator; it allocates no carriers).
    System = 0,
    /// `temp_alloc` — very short-lived temporary allocations.
    Temporary = 1,
    /// `sl_alloc` — short-lived allocations.
    ShortLived = 2,
    /// `std_alloc` — general-purpose allocations.
    Standard = 3,
    /// `ll_alloc` — long-lived allocations.
    LongLived = 4,
    /// `eheap_alloc` — Erlang process heaps.
    EHeap = 5,
    /// `ets_alloc` — ETS table data.
    Ets = 6,
    /// `fix_alloc` — fixed-size blocks.
    FixedSize = 7,
    /// `literal_alloc` — literal/code areas.
    Literal = 8,
    /// `binary_alloc` — off-heap binaries.
    Binary = 9,
    /// `driver_alloc` — port drivers.
    Driver = 10,
    /// `test_alloc` — test allocator (disabled by default in the legacy VM).
    Test = 11,
}

impl AllocatorId {
    /// Every allocator, in `erl_alloc.types` declaration order.
    pub const ALL: [AllocatorId; 12] = [
        AllocatorId::System,
        AllocatorId::Temporary,
        AllocatorId::ShortLived,
        AllocatorId::Standard,
        AllocatorId::LongLived,
        AllocatorId::EHeap,
        AllocatorId::Ets,
        AllocatorId::FixedSize,
        AllocatorId::Literal,
        AllocatorId::Binary,
        AllocatorId::Driver,
        AllocatorId::Test,
    ];

    /// The `allocator` description of this id, i.e. the stable name used by the
    /// legacy VM (the third column of the `allocator` declaration, which
    /// `make_alloc_types` does **not** expose as a macro but which
    /// `ERTS_ALC_A2AD` resolves to at runtime).
    pub fn as_str(&self) -> &'static str {
        match self {
            AllocatorId::System => "sys_alloc",
            AllocatorId::Temporary => "temp_alloc",
            AllocatorId::ShortLived => "sl_alloc",
            AllocatorId::Standard => "std_alloc",
            AllocatorId::LongLived => "ll_alloc",
            AllocatorId::EHeap => "eheap_alloc",
            AllocatorId::Ets => "ets_alloc",
            AllocatorId::FixedSize => "fix_alloc",
            AllocatorId::Literal => "literal_alloc",
            AllocatorId::Binary => "binary_alloc",
            AllocatorId::Driver => "driver_alloc",
            AllocatorId::Test => "test_alloc",
        }
    }

    /// The position of this allocator in [`AllocatorId::ALL`] (equivalently its
    /// `ERTS_ALC_A_*` number minus one).
    pub fn index(&self) -> usize {
        *self as usize
    }
}

// ---------------------------------------------------------------------------
// Classes
// ---------------------------------------------------------------------------

/// An allocation class id.
///
/// The six `class` declarations of `erl_alloc.types`; a class groups types for
/// the `erlang:system_info(alloc_util)` / `allocator`-info reporting, and each
/// class is registered as an `erts_alloc` "class" alongside the allocators.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum ClassId {
    /// `process_data` — process heaps, stacks, messages.
    Processes = 0,
    /// `atom_data` — the atom table and atom text.
    Atom = 1,
    /// `code_data` — modules, literals, fun/export tables.
    Code = 2,
    /// `ets_data` — ETS tables and their intermediate structures.
    Ets = 3,
    /// `binary_data` — off-heap binary payloads.
    Binaries = 4,
    /// `system_data` — everything else (ports, drivers, run queues, …).
    System = 5,
}

impl ClassId {
    /// Every class, in `erl_alloc.types` declaration order.
    pub const ALL: [ClassId; 6] = [
        ClassId::Processes,
        ClassId::Atom,
        ClassId::Code,
        ClassId::Ets,
        ClassId::Binaries,
        ClassId::System,
    ];

    /// The `class` description of this id (`process_data`, …) — the second
    /// column of the `class` declaration.
    pub fn as_str(&self) -> &'static str {
        match self {
            ClassId::Processes => "process_data",
            ClassId::Atom => "atom_data",
            ClassId::Code => "code_data",
            ClassId::Ets => "ets_data",
            ClassId::Binaries => "binary_data",
            ClassId::System => "system_data",
        }
    }

    /// The position of this class in [`ClassId::ALL`].
    pub fn index(&self) -> usize {
        *self as usize
    }
}

// ---------------------------------------------------------------------------
// Type flags
// ---------------------------------------------------------------------------

/// The carrier/allocator capability flags a type inherits from its allocator.
///
/// A bitflags-style newtype (the crate deliberately does not pull in an external
/// bitflags crate). See the module documentation for the exact legacy anchor of
/// each bit; [`TypeFlags::for_allocator`] is the single source of truth.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeFlags(u32);

impl TypeFlags {
    const MSEG_BIT: u32 = 1 << 0;
    const SUPER_ALIGNED_BIT: u32 = 1 << 1;
    const FIXED_SIZE_BIT: u32 = 1 << 2;
    const HEAP_BIT: u32 = 1 << 3;
    const SBC_BIT: u32 = 1 << 4;
    const MBC_BIT: u32 = 1 << 5;

    /// The three bits shared by every carrier-backed allocator
    /// (`mseg_alloc`-sourced SBCs and MBCs).
    const CARRIER_BITS: u32 = Self::MSEG_BIT | Self::SBC_BIT | Self::MBC_BIT;

    /// Every bit defined by this type (the mask used by
    /// [`TypeFlags::from_bits_truncate`]).
    pub const ALL_BITS: u32 = Self::MSEG_BIT
        | Self::SUPER_ALIGNED_BIT
        | Self::FIXED_SIZE_BIT
        | Self::HEAP_BIT
        | Self::SBC_BIT
        | Self::MBC_BIT;

    /// Carriers of this type's allocator are obtained from the memory-segment
    /// (`mseg`) allocator (`Allctr_t::mseg_alloc`).
    pub const MSEG: TypeFlags = TypeFlags(Self::MSEG_BIT);
    /// This type's allocator may hand out super-aligned carriers
    /// (`ERTS_HAVE_MSEG_SUPER_ALIGNED`).
    pub const SUPER_ALIGNED: TypeFlags = TypeFlags(Self::SUPER_ALIGNED_BIT);
    /// This type's blocks are fixed-size (`ERTS_ALC_A_FIXED_SIZE`).
    pub const FIXED_SIZE: TypeFlags = TypeFlags(Self::FIXED_SIZE_BIT);
    /// This type's memory is a process heap (`ERTS_ALC_A_EHEAP`).
    pub const HEAP: TypeFlags = TypeFlags(Self::HEAP_BIT);
    /// This type's allocator can place a large block in a single-block carrier.
    pub const SBC: TypeFlags = TypeFlags(Self::SBC_BIT);
    /// This type's allocator subdivides a main multi-block carrier.
    pub const MBC: TypeFlags = TypeFlags(Self::MBC_BIT);

    /// The empty flag set.
    pub const fn empty() -> TypeFlags {
        TypeFlags(0)
    }

    /// The raw bits of this flag set.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// `true` if this set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// `true` if **all** bits of `other` are present in `self`.
    pub const fn contains(self, other: TypeFlags) -> bool {
        (self.0 & other.0) == other.0
    }

    /// `true` if **any** bit of `other` is present in `self`.
    pub const fn intersects(self, other: TypeFlags) -> bool {
        (self.0 & other.0) != 0
    }

    /// The union of `self` and `other` (usable in `const` context).
    pub const fn union(self, other: TypeFlags) -> TypeFlags {
        TypeFlags(self.0 | other.0)
    }

    /// Build a flag set from raw bits, discarding any bit not defined by
    /// [`TypeFlags::ALL_BITS`].
    pub const fn from_bits_truncate(bits: u32) -> TypeFlags {
        TypeFlags(bits & Self::ALL_BITS)
    }

    /// The capability bits an allocator contributes to every type bound to it.
    ///
    /// This is the documented derivation of the module header, expressed once:
    /// * `SYSTEM` allocates raw memory (`erts_sys_alloc`) and uses no carriers.
    /// * `EHEAP` additionally marks process heaps.
    /// * `FIXED_SIZE` additionally marks fixed-size blocks.
    /// * `LITERAL` additionally marks super-aligned carrier capability.
    /// * every other allocator is a plain carrier-backed allocator.
    pub const fn for_allocator(allocator: AllocatorId) -> TypeFlags {
        match allocator {
            AllocatorId::System => TypeFlags(0),
            AllocatorId::EHeap => TypeFlags(Self::HEAP_BIT | Self::CARRIER_BITS),
            AllocatorId::FixedSize => TypeFlags(Self::FIXED_SIZE_BIT | Self::CARRIER_BITS),
            AllocatorId::Literal => TypeFlags(Self::SUPER_ALIGNED_BIT | Self::CARRIER_BITS),
            _ => TypeFlags(Self::CARRIER_BITS),
        }
    }
}

impl core::ops::BitOr for TypeFlags {
    type Output = TypeFlags;

    fn bitor(self, rhs: TypeFlags) -> TypeFlags {
        self.union(rhs)
    }
}

impl fmt::Debug for TypeFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const NAMED: [(u32, &str); 6] = [
            (TypeFlags::MSEG_BIT, "MSEG"),
            (TypeFlags::SUPER_ALIGNED_BIT, "SUPER_ALIGNED"),
            (TypeFlags::FIXED_SIZE_BIT, "FIXED_SIZE"),
            (TypeFlags::HEAP_BIT, "HEAP"),
            (TypeFlags::SBC_BIT, "SBC"),
            (TypeFlags::MBC_BIT, "MBC"),
        ];

        if self.0 == 0 {
            return f.write_str("TypeFlags(empty)");
        }
        f.write_str("TypeFlags(")?;
        let mut first = true;
        for (bit, name) in NAMED {
            if self.0 & bit != 0 {
                if !first {
                    f.write_str(" | ")?;
                }
                first = false;
                f.write_str(name)?;
            }
        }
        let unknown = self.0 & !TypeFlags::ALL_BITS;
        if unknown != 0 {
            if !first {
                f.write_str(" | ")?;
            }
            write!(f, "0x{unknown:x}")?;
        }
        f.write_str(")")
    }
}

// ---------------------------------------------------------------------------
// The type table
// ---------------------------------------------------------------------------

/// One `type` declaration of `erl_alloc.types`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TypeEntry {
    /// The `type` name (the first column), e.g. `PROC`. This is the name
    /// `ERTS_ALC_T_<NAME>` is generated for.
    pub name: &'static str,
    /// The allocator this type allocates through (second column).
    pub allocator: AllocatorId,
    /// The class this type belongs to (third column).
    pub class: ClassId,
    /// The description (fourth column), e.g. `proc`.
    pub description: &'static str,
    /// The capability flags inherited from [`TypeEntry::allocator`]
    /// (see [`TypeFlags::for_allocator`]).
    pub flags: TypeFlags,
}

/// Every active `type` declaration of `erts/emulator/beam/erl_alloc.types`,
/// **in file order**.
///
/// Active means: the `unix`/`lcnt`/`win32`/`beamasm` filter described in the
/// module header is applied (243 of the file's 249 `type` lines). The `flags`
/// column is [`TypeFlags::for_allocator`] of the type's allocator; a test
/// re-checks that invariant so the table cannot silently diverge from the
/// documented derivation.
pub static ALLOC_TYPES: &[TypeEntry] = &[
    TypeEntry { name: "PROC", allocator: AllocatorId::FixedSize, class: ClassId::Processes, description: "proc", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "PORT", allocator: AllocatorId::Driver, class: ClassId::System, description: "port", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "ATOM", allocator: AllocatorId::LongLived, class: ClassId::Atom, description: "atom_entry", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "MODULE", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "module_entry", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "REG_PROC", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "reg_proc", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "PROC_LIST", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "proc_list", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "SAVED_ESTACK", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "saved_estack", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "FUN_ENTRY", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "fun_entry", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "ATOM_TXT", allocator: AllocatorId::LongLived, class: ClassId::Atom, description: "atom_text", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "BEAM_REGISTER", allocator: AllocatorId::EHeap, class: ClassId::Processes, description: "beam_register", flags: TypeFlags::for_allocator(AllocatorId::EHeap) },
    TypeEntry { name: "HEAP", allocator: AllocatorId::EHeap, class: ClassId::Processes, description: "heap", flags: TypeFlags::for_allocator(AllocatorId::EHeap) },
    TypeEntry { name: "OLD_HEAP", allocator: AllocatorId::EHeap, class: ClassId::Processes, description: "old_heap", flags: TypeFlags::for_allocator(AllocatorId::EHeap) },
    TypeEntry { name: "HEAP_FRAG", allocator: AllocatorId::EHeap, class: ClassId::Processes, description: "heap_frag", flags: TypeFlags::for_allocator(AllocatorId::EHeap) },
    TypeEntry { name: "TMP_HEAP", allocator: AllocatorId::Temporary, class: ClassId::Processes, description: "tmp_heap", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "RECV_MARK_BLK", allocator: AllocatorId::FixedSize, class: ClassId::Processes, description: "receive_marker_block", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "MSG_REF", allocator: AllocatorId::FixedSize, class: ClassId::Processes, description: "msg_ref", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "MSG", allocator: AllocatorId::EHeap, class: ClassId::Processes, description: "message", flags: TypeFlags::for_allocator(AllocatorId::EHeap) },
    TypeEntry { name: "MSGQ_CHNG", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "messages_queue_change", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ROOTSET", allocator: AllocatorId::Temporary, class: ClassId::Processes, description: "root_set", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "LOADER_TMP", allocator: AllocatorId::Temporary, class: ClassId::Code, description: "loader_tmp", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "PREPARED_CODE", allocator: AllocatorId::ShortLived, class: ClassId::Code, description: "prepared_code", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "TIMER_SERVICE", allocator: AllocatorId::LongLived, class: ClassId::System, description: "timer_service", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "LL_PTIMER", allocator: AllocatorId::FixedSize, class: ClassId::Processes, description: "ll_ptimer", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "HL_PTIMER", allocator: AllocatorId::FixedSize, class: ClassId::Processes, description: "hl_ptimer", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "BIF_TIMER", allocator: AllocatorId::FixedSize, class: ClassId::Processes, description: "bif_timer", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "PAUSED_TIMER", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "paused_timer", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "TIMER_REQUEST", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "timer_request", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "BTM_YIELD_STATE", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "btm_yield_state", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "REG_TABLE", allocator: AllocatorId::Standard, class: ClassId::System, description: "reg_tab", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "FUN_TABLE", allocator: AllocatorId::Standard, class: ClassId::Code, description: "fun_tab", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "DIST_TABLE", allocator: AllocatorId::Standard, class: ClassId::System, description: "dist_tab", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "NODE_TABLE", allocator: AllocatorId::Standard, class: ClassId::System, description: "node_tab", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "ATOM_TABLE", allocator: AllocatorId::LongLived, class: ClassId::Atom, description: "atom_tab", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "MODULE_TABLE", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "module_tab", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "TAINT", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "taint_list", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "MODULE_REFS", allocator: AllocatorId::Standard, class: ClassId::Code, description: "module_refs", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "NC_TMP", allocator: AllocatorId::Temporary, class: ClassId::System, description: "nc_tmp", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "NC_STD", allocator: AllocatorId::Standard, class: ClassId::System, description: "nc_std", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "TMP", allocator: AllocatorId::Temporary, class: ClassId::System, description: "tmp", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "UNDEF", allocator: AllocatorId::System, class: ClassId::System, description: "undefined", flags: TypeFlags::for_allocator(AllocatorId::System) },
    TypeEntry { name: "DCACHE", allocator: AllocatorId::Standard, class: ClassId::System, description: "dcache", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "DCTRL_BUF", allocator: AllocatorId::Temporary, class: ClassId::System, description: "dctrl_buf", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "DIST_ENTRY", allocator: AllocatorId::Standard, class: ClassId::System, description: "dist_entry", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "NODE_ENTRY", allocator: AllocatorId::Standard, class: ClassId::System, description: "node_entry", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "PROC_TABLE", allocator: AllocatorId::LongLived, class: ClassId::Processes, description: "proc_tab", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "PORT_TABLE", allocator: AllocatorId::LongLived, class: ClassId::System, description: "port_tab", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "TIMER_WHEEL", allocator: AllocatorId::LongLived, class: ClassId::System, description: "timer_wheel", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "DRV", allocator: AllocatorId::Driver, class: ClassId::System, description: "drv_internal", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "DRV_BINARY", allocator: AllocatorId::Binary, class: ClassId::Binaries, description: "drv_binary", flags: TypeFlags::for_allocator(AllocatorId::Binary) },
    TypeEntry { name: "DRIVER", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "DRV_CMD_DATA", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver_command_data", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "DRV_CTRL_DATA", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver_control_data", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "DRV_CALL_DATA", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver_call_data", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "NIF", allocator: AllocatorId::Driver, class: ClassId::System, description: "nif_internal", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "BINARY", allocator: AllocatorId::Binary, class: ClassId::Binaries, description: "binary", flags: TypeFlags::for_allocator(AllocatorId::Binary) },
    TypeEntry { name: "NBIF_TABLE", allocator: AllocatorId::System, class: ClassId::System, description: "nbif_tab", flags: TypeFlags::for_allocator(AllocatorId::System) },
    TypeEntry { name: "ARG_REG", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "arg_reg", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "PROC_DICT", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "proc_dict", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "CALLS_BUF", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "calls_buf", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "BPD", allocator: AllocatorId::Standard, class: ClassId::System, description: "bpd", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "LINEBUF", allocator: AllocatorId::Standard, class: ClassId::System, description: "line_buf", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "IOQ", allocator: AllocatorId::Standard, class: ClassId::System, description: "io_queue", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "BITS_BUF", allocator: AllocatorId::Standard, class: ClassId::System, description: "bits_buf", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "ASYNC_DATA", allocator: AllocatorId::LongLived, class: ClassId::System, description: "internal_async_data", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "ESTACK", allocator: AllocatorId::Temporary, class: ClassId::System, description: "estack", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "DB_TABLE", allocator: AllocatorId::Ets, class: ClassId::Ets, description: "db_tab", flags: TypeFlags::for_allocator(AllocatorId::Ets) },
    TypeEntry { name: "DB_FIXATION", allocator: AllocatorId::ShortLived, class: ClassId::Ets, description: "db_fixation", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "DB_FIX_DEL", allocator: AllocatorId::ShortLived, class: ClassId::Ets, description: "fixed_del", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "DB_TABLES", allocator: AllocatorId::LongLived, class: ClassId::Ets, description: "db_tabs", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "DB_NTAB_ENT", allocator: AllocatorId::Standard, class: ClassId::Ets, description: "db_named_table_entry", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "DB_TMP", allocator: AllocatorId::Temporary, class: ClassId::Ets, description: "db_tmp", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "DB_MC_STK", allocator: AllocatorId::Temporary, class: ClassId::Ets, description: "db_mc_stack", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "DB_MS_RUN_HEAP", allocator: AllocatorId::ShortLived, class: ClassId::Ets, description: "db_match_spec_run_heap", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "DB_MS_CMPL_HEAP", allocator: AllocatorId::Temporary, class: ClassId::Ets, description: "db_match_spec_cmpl_heap", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "DB_SEG", allocator: AllocatorId::Ets, class: ClassId::Ets, description: "db_segment", flags: TypeFlags::for_allocator(AllocatorId::Ets) },
    TypeEntry { name: "DB_STK", allocator: AllocatorId::Ets, class: ClassId::Ets, description: "db_stack", flags: TypeFlags::for_allocator(AllocatorId::Ets) },
    TypeEntry { name: "DB_TRANS_TAB", allocator: AllocatorId::Ets, class: ClassId::Ets, description: "db_trans_tab", flags: TypeFlags::for_allocator(AllocatorId::Ets) },
    TypeEntry { name: "DB_SEL_LIST", allocator: AllocatorId::Ets, class: ClassId::Ets, description: "db_select_list", flags: TypeFlags::for_allocator(AllocatorId::Ets) },
    TypeEntry { name: "DB_DMC_ERROR", allocator: AllocatorId::Ets, class: ClassId::Ets, description: "db_dmc_error", flags: TypeFlags::for_allocator(AllocatorId::Ets) },
    TypeEntry { name: "DB_DMC_ERR_INFO", allocator: AllocatorId::Ets, class: ClassId::Ets, description: "db_dmc_error_info", flags: TypeFlags::for_allocator(AllocatorId::Ets) },
    TypeEntry { name: "DB_TERM", allocator: AllocatorId::Ets, class: ClassId::Ets, description: "db_term", flags: TypeFlags::for_allocator(AllocatorId::Ets) },
    TypeEntry { name: "DB_PROC_CLEANUP", allocator: AllocatorId::ShortLived, class: ClassId::Ets, description: "db_proc_cleanup_state", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ETS_ALL_REQ", allocator: AllocatorId::ShortLived, class: ClassId::Ets, description: "ets_all_request", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ETS_CTRS", allocator: AllocatorId::Ets, class: ClassId::Ets, description: "ets_decentralized_ctrs", flags: TypeFlags::for_allocator(AllocatorId::Ets) },
    TypeEntry { name: "ETS_I_LST_TRAP", allocator: AllocatorId::ShortLived, class: ClassId::Ets, description: "ets_insert_list_bif_trap_state", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "LOGGER_DSBUF", allocator: AllocatorId::Temporary, class: ClassId::System, description: "logger_dsbuf", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "TMP_DSBUF", allocator: AllocatorId::Temporary, class: ClassId::System, description: "tmp_dsbuf", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "INFO_DSBUF", allocator: AllocatorId::System, class: ClassId::System, description: "info_dsbuf", flags: TypeFlags::for_allocator(AllocatorId::System) },
    TypeEntry { name: "SCHDLR_SLP_INFO", allocator: AllocatorId::LongLived, class: ClassId::System, description: "scheduler_sleep_info", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "RUNQS", allocator: AllocatorId::LongLived, class: ClassId::System, description: "run_queues", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "DDLL_HANDLE", allocator: AllocatorId::Standard, class: ClassId::System, description: "ddll_handle", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "DDLL_ERRCODES", allocator: AllocatorId::LongLived, class: ClassId::System, description: "ddll_errcodes", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "DDLL_TMP_BUF", allocator: AllocatorId::Temporary, class: ClassId::System, description: "ddll_tmp_buf", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "PORT_TASK", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "port_task", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PT_HNDL_LIST", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "port_task_handle_list", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "MISC_OP_LIST", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "misc_op_list", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PORT_NAMES", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "port_names", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PORT_DATA_LOCK", allocator: AllocatorId::Driver, class: ClassId::System, description: "port_data_lock", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "PTAB_LIST_DEL", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "ptab_list_deleted_el", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PTAB_LIST_CNKI", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "ptab_list_chunk_info", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PTAB_LIST_PIDS", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "ptab_list_pids", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "RE_TMP_BUF", allocator: AllocatorId::Temporary, class: ClassId::System, description: "re_tmp_buf", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "RE_SHORTLIVED", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "re_shortlived", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "RE_INIT", allocator: AllocatorId::LongLived, class: ClassId::System, description: "re_init", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "RE_SUBJECT", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "re_subject", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "RE_PRECOMPILE", allocator: AllocatorId::Standard, class: ClassId::System, description: "re_precompile", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "UNICODE_BUFFER", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "unicode_buffer", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "BINARY_BUFFER", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "binary_buffer", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PRE_ALLOC_DATA", allocator: AllocatorId::LongLived, class: ClassId::System, description: "pre_alloc_data", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "DRV_THR_OPTS", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver_thread_opts", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "DRV_TID", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver_tid", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "DRV_MTX", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver_mutex", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "DRV_CND", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver_cond", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "DRV_RWLCK", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver_rwlock", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "DRV_TSD", allocator: AllocatorId::Driver, class: ClassId::System, description: "driver_tsd", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "PSD", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "process_specific_data", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "PRTSD", allocator: AllocatorId::Standard, class: ClassId::System, description: "port_specific_data", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "CPUDATA", allocator: AllocatorId::LongLived, class: ClassId::System, description: "cpu_data", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "TMP_CPU_IDS", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "tmp_cpu_ids", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "EXT_TERM_DATA", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "external_term_data", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "CPU_GRPS_MAP", allocator: AllocatorId::LongLived, class: ClassId::System, description: "cpu_groups_map", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "MISC_AUX_WORK_Q", allocator: AllocatorId::LongLived, class: ClassId::System, description: "misc_aux_work_q", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "CODE_IX_LOCK_Q", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "code_ix_lock_q", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "BUSY_CALLER_TAB", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "busy_caller_table", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "BUSY_CALLER", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "busy_caller", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PROC_SYS_TSK", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "proc_sys_task", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PROC_SYS_TSK_QS", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "proc_sys_task_queues", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "NEW_TIME_OFFSET", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "new_time_offset", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "IOB_REQ", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "io_bytes_request", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "TRACER_NIF", allocator: AllocatorId::LongLived, class: ClassId::System, description: "tracer_nif", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "TRACE_MSG_QUEUE", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "trace_message_queue", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "DIRTY_SL", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "dirty_short_lived", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "MREF_NSCHED_ENT", allocator: AllocatorId::FixedSize, class: ClassId::System, description: "nsched_magic_ref_entry", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "MREF_ENT", allocator: AllocatorId::Standard, class: ClassId::System, description: "magic_ref_entry", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "MREF_TAB_BKTS", allocator: AllocatorId::Standard, class: ClassId::System, description: "magic_ref_table_buckets", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "MREF_TAB", allocator: AllocatorId::LongLived, class: ClassId::System, description: "magic_ref_table", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "PREF_NSCHED_ENT", allocator: AllocatorId::FixedSize, class: ClassId::System, description: "nsched_pid_ref_entry", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "PREF_ENT", allocator: AllocatorId::Standard, class: ClassId::System, description: "pid_ref_entry", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "PREF_TAB_BKTS", allocator: AllocatorId::Standard, class: ClassId::System, description: "pid_ref_table_buckets", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "PREF_TAB", allocator: AllocatorId::LongLived, class: ClassId::System, description: "pid_ref_table", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "MINDIRECTION", allocator: AllocatorId::FixedSize, class: ClassId::System, description: "magic_indirection", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "BINARY_FIND", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "binary_find", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "CRASH_DUMP", allocator: AllocatorId::Standard, class: ClassId::System, description: "crash_dump", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "DIST_TRANSCODE", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "dist_transcode_context", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "RLA_BLOCK_CNTRS", allocator: AllocatorId::LongLived, class: ClassId::System, description: "release_literal_area_block_counters", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "SIGQ_BUFFERS", allocator: AllocatorId::FixedSize, class: ClassId::Processes, description: "process_signal_queue_buffers", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "THR_Q_EL", allocator: AllocatorId::Standard, class: ClassId::System, description: "thr_q_element", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "THR_Q_EL_SL", allocator: AllocatorId::FixedSize, class: ClassId::System, description: "sl_thr_q_element", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "MISC_AUX_WORK", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "misc_aux_work", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "THR_Q", allocator: AllocatorId::Standard, class: ClassId::System, description: "thr_queue", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "THR_Q_SL", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "short_lived_thr_queue", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "THR_Q_LL", allocator: AllocatorId::LongLived, class: ClassId::System, description: "long_lived_thr_queue", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "ASYNC", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "async", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ZLIB", allocator: AllocatorId::Standard, class: ClassId::System, description: "zlib", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "DRIVER_LOCK", allocator: AllocatorId::Standard, class: ClassId::System, description: "driver_lock", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "XPORTS_LIST", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "extra_port_list", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "RUNQ_BLNS", allocator: AllocatorId::LongLived, class: ClassId::System, description: "run_queue_balancing", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "THR_PRGR_IDATA", allocator: AllocatorId::LongLived, class: ClassId::System, description: "thr_prgr_internal_data", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "THR_PRGR_DATA", allocator: AllocatorId::LongLived, class: ClassId::System, description: "thr_prgr_data", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "T_THR_PRGR_DATA", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "temp_thr_prgr_data", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "RELEASE_LAREA", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "release_literal_area", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "SIG_DATA", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "signal_data", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "SIG_YIELD_DATA", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "signal_yield_data", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "DIST_DEMONITOR", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "dist_demonitor", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "CML_CLEANUP", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "connection_ml_cleanup", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ML_YIELD_STATE", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "monitor_link_yield_state", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ML_DIST", allocator: AllocatorId::Standard, class: ClassId::System, description: "monitor_link_dist", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "PF3_ARGS", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "process_flag_3_arguments", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "TRACE_INFO_REQ", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "trace_info_proc_request", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "SETUP_CONN_ARG", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "setup_connection_argument", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "LIST_TRAP", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "list_bif_trap_state", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "CONT_EXIT_TRAP", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "continue_exit_trap_state", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "SEQ_YIELD_STATE", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "dist_seq_yield_state", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PHASH2_TRAP", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "phash2_trap_state", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "MAP_TRAP", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "map_bif_trap_state", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ENVIRONMENT", allocator: AllocatorId::System, class: ClassId::System, description: "environment", flags: TypeFlags::for_allocator(AllocatorId::System) },
    TypeEntry { name: "PERSISTENT_TERM", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "persistent_term", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "PERSISTENT_LOCK_Q", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "persistent_lock_q", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PERSISTENT_TERM_TMP", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "persistent_term_tmp_table", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "T2B_VEC", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "term_to_binary_vector", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ETHR_STD", allocator: AllocatorId::Standard, class: ClassId::System, description: "ethread_standard", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "ETHR_SL", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "ethread_short_lived", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ETHR_LL", allocator: AllocatorId::LongLived, class: ClassId::System, description: "ethread_long_lived", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "SYS_MSG_Q", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "system_messages_queue", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "FP_EXCEPTION", allocator: AllocatorId::LongLived, class: ClassId::System, description: "fp_exception", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "LL_MPATHS", allocator: AllocatorId::LongLived, class: ClassId::System, description: "ll_migration_paths", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "SL_MPATHS", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "sl_migration_paths", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "T2B_DETERMINISTIC", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "term_to_binary_deterministic", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "DSIG_HNDL_NTFY", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "dirty_signal_handler_notification", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "SCHD_SIG_NTFY", allocator: AllocatorId::ShortLived, class: ClassId::Processes, description: "scheduled_signal_notify", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "CODE_COVERAGE", allocator: AllocatorId::Standard, class: ClassId::System, description: "code_coverage", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "PRIO_Q_INFO", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "message_prio_queue_info", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "ENO_MAP", allocator: AllocatorId::LongLived, class: ClassId::System, description: "errno_map", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "ENO_ENTRY", allocator: AllocatorId::LongLived, class: ClassId::System, description: "errno_entry", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "ENO_TAB", allocator: AllocatorId::LongLived, class: ClassId::System, description: "errno_table", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "ENO_TAB_BKTS", allocator: AllocatorId::LongLived, class: ClassId::System, description: "errno_table_buckets", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "EXPORT_TABLE", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "export_tab", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "EXPORT", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "export_entry", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "CATCHES", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "catches", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "DEBUG", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "debugging", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "DDLL_PROCESS", allocator: AllocatorId::Standard, class: ClassId::System, description: "ddll_processes", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "MONITOR_EXT", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "monitor_extended", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "LINK_EXT", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "link_extended", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "LITERAL", allocator: AllocatorId::Literal, class: ClassId::Code, description: "literal", flags: TypeFlags::for_allocator(AllocatorId::Literal) },
    TypeEntry { name: "LITERAL_REF", allocator: AllocatorId::ShortLived, class: ClassId::Code, description: "literal_area_ref", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PURGE_DATA", allocator: AllocatorId::ShortLived, class: ClassId::Code, description: "purge_data", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "DB_HEIR_DATA", allocator: AllocatorId::Standard, class: ClassId::Ets, description: "db_heir_data", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "DB_MS_PSDO_PROC", allocator: AllocatorId::LongLived, class: ClassId::Ets, description: "db_match_pseudo_proc", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "SCHDLR_DATA", allocator: AllocatorId::LongLived, class: ClassId::System, description: "scheduler_data", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "NFUNC_TRAP_WRAPPER", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "nfunc_trap_wrapper", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "MONITOR", allocator: AllocatorId::FixedSize, class: ClassId::Processes, description: "monitor", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "MONITOR_TAG", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "monitor_tag", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "ALIAS", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "alias", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "MONITOR_SUSPEND", allocator: AllocatorId::Standard, class: ClassId::Processes, description: "monitor_suspend", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "LINK", allocator: AllocatorId::FixedSize, class: ClassId::Processes, description: "link", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "AINFO_REQ", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "alloc_info_request", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "SCHED_WTIME_REQ", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "sched_wall_time_request", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "GC_INFO_REQ", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "gc_info_request", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "PORT_DATA_HEAP", allocator: AllocatorId::Standard, class: ClassId::System, description: "port_data_heap", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "MSACC", allocator: AllocatorId::Driver, class: ClassId::System, description: "microstate_accounting", flags: TypeFlags::for_allocator(AllocatorId::Driver) },
    TypeEntry { name: "SYS_CHECK_REQ", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "system_check_request", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "ATOMICS", allocator: AllocatorId::Standard, class: ClassId::System, description: "erl_bif_atomics", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "COUNTERS", allocator: AllocatorId::Standard, class: ClassId::System, description: "erl_bif_counters", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "RECORD_TABLE", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "record_table", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "RECORD", allocator: AllocatorId::LongLived, class: ClassId::Code, description: "record_entry", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "TEMP_TERM", allocator: AllocatorId::Temporary, class: ClassId::System, description: "temp_term", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "SHORT_LIVED_TERM", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "short_lived_term", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "DRV_TAB", allocator: AllocatorId::LongLived, class: ClassId::System, description: "drv_tab", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "DRV_EV_STATE", allocator: AllocatorId::LongLived, class: ClassId::System, description: "driver_event_state", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "DRV_SEL_D_STATE", allocator: AllocatorId::FixedSize, class: ClassId::System, description: "driver_select_data_state", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "NIF_SEL_D_STATE", allocator: AllocatorId::FixedSize, class: ClassId::System, description: "enif_select_data_state", flags: TypeFlags::for_allocator(AllocatorId::FixedSize) },
    TypeEntry { name: "POLLSET", allocator: AllocatorId::LongLived, class: ClassId::System, description: "pollset", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "POLLSET_UPDREQ", allocator: AllocatorId::ShortLived, class: ClassId::System, description: "pollset_update_req", flags: TypeFlags::for_allocator(AllocatorId::ShortLived) },
    TypeEntry { name: "POLL_FDS", allocator: AllocatorId::LongLived, class: ClassId::System, description: "poll_fds", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "BLOCK_PTHR_DATA", allocator: AllocatorId::LongLived, class: ClassId::System, description: "block_poll_thread_data", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "FD_STATUS", allocator: AllocatorId::LongLived, class: ClassId::System, description: "fd_status", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "SELECT_FDS", allocator: AllocatorId::LongLived, class: ClassId::System, description: "select_fds", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "SYS_READ_BUF", allocator: AllocatorId::Temporary, class: ClassId::System, description: "sys_read_buf", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "FD_ENTRY_BUF", allocator: AllocatorId::Standard, class: ClassId::System, description: "fd_entry_buf", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "CS_PROG_PATH", allocator: AllocatorId::LongLived, class: ClassId::System, description: "cs_prog_path", flags: TypeFlags::for_allocator(AllocatorId::LongLived) },
    TypeEntry { name: "SYS_BLOCKING", allocator: AllocatorId::Standard, class: ClassId::System, description: "sys_blocking", flags: TypeFlags::for_allocator(AllocatorId::Standard) },
    TypeEntry { name: "SYS_WRITE_BUF", allocator: AllocatorId::Temporary, class: ClassId::System, description: "sys_write_buf", flags: TypeFlags::for_allocator(AllocatorId::Temporary) },
    TypeEntry { name: "TEST", allocator: AllocatorId::Test, class: ClassId::System, description: "testing", flags: TypeFlags::for_allocator(AllocatorId::Test) },
];

/// The number of types in [`ALLOC_TYPES`].
pub const TYPE_COUNT: usize = ALLOC_TYPES.len();

/// The number of `type` declarations of `erts/emulator/beam/erl_alloc.types`
/// that are active under the modelled configuration (`unix` enabled, `win32`
/// disabled, `lcnt` disabled, `beamasm` enabled): the file's 249 `type` lines
/// minus the 1 `+ifnot beamasm` `CODE` type, the 2 `+if lcnt` `LCNT_*` types and
/// the 3 `+if win32` types — and including the 5 `+if unix` types.
pub const EXPECTED_TYPE_COUNT: usize = 243;

const _: () = assert!(
    TYPE_COUNT == EXPECTED_TYPE_COUNT,
    "ALLOC_TYPES must contain exactly the active erl_alloc.types declarations"
);

/// A compact index into [`ALLOC_TYPES`].
///
/// Obtained from [`TypeId::from_index`] or [`TypeId::from_name`]; every accessor
/// is total for a constructed `TypeId` (the constructors are the only way to
/// make one, and they validate the index).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct TypeId(u16);

impl TypeId {
    /// The position of this type in [`ALLOC_TYPES`].
    pub fn index(self) -> usize {
        usize::from(self.0)
    }

    /// The table row for this type.
    pub fn entry(self) -> &'static TypeEntry {
        // The index is in range by construction: `TypeId` is only ever made by
        // `from_index`/`from_name`, both of which bounds-check. This is a plain
        // slice index, not `unsafe` — it panics rather than reading out of
        // bounds, and the invariant makes that unreachable.
        &ALLOC_TYPES[self.index()]
    }

    /// The `type` name (e.g. `PROC`).
    pub fn name(self) -> &'static str {
        self.entry().name
    }

    /// The allocator this type allocates through.
    pub fn allocator(self) -> AllocatorId {
        self.entry().allocator
    }

    /// The class this type belongs to.
    pub fn class(self) -> ClassId {
        self.entry().class
    }

    /// The capability flags inherited from the type's allocator.
    pub fn flags(self) -> TypeFlags {
        self.entry().flags
    }

    /// The type at `index`, or `None` if the index is out of range.
    pub fn from_index(index: usize) -> Option<TypeId> {
        if index < TYPE_COUNT {
            Some(TypeId(index as u16))
        } else {
            None
        }
    }

    /// The type named `name`, or `None` if no such type is active.
    pub fn from_name(name: &str) -> Option<TypeId> {
        type_by_name(name)
    }
}

/// Look up an active type by its `type` name, e.g.
/// `type_by_name("PROC").unwrap().allocator()`.
pub fn type_by_name(name: &str) -> Option<TypeId> {
    ALLOC_TYPES
        .iter()
        .position(|entry| entry.name == name)
        .and_then(TypeId::from_index)
}

/// Every type bound to `allocator`, in table (file) order.
pub fn types_for_allocator(allocator: AllocatorId) -> impl Iterator<Item = TypeId> {
    ALLOC_TYPES
        .iter()
        .enumerate()
        .filter(move |(_, entry)| entry.allocator == allocator)
        .map(|(index, _)| TypeId(index as u16))
}

#[cfg(test)]
mod tests;
