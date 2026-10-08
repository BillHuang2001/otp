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

//! The process-wide allocator registry (`erts_allctrs` equivalent).
//!
//! Mirrors the legacy one-allocator-per-`AllocType` scheme of
//! `erts/emulator/beam/erl_alloc.types`: the runtime's allocation classes are
//! addressed by a stable name and resolved through a lazily-initialised, one-time
//! table, so `erts_alloc(TYPE, size)` (and its Rust replacement) has a single
//! allocator instance per type.
//!
//! The allocator *inventory* itself (which allocators and types exist, their
//! classes and capability flags) is owned by [`crate::types`]; this module maps
//! each [`AllocatorId`] to its live [`AllocRef`] and answers the registry-shaped
//! queries the legacy C API exposes (`erts_alloc_util_allocators`, per-allocator
//! type listing).
//!
//! In Milestone 1 every type uses the same default [`AllocatorConfig`]; per-type
//! tuning and thread-specific instances are documented gaps.

use std::sync::{Arc, LazyLock};

use crate::allocator::{AllocRef, AllocatorConfig, CarrierAllocator};
use crate::stats::AllocStats;
use crate::types::AllocatorId;

/// Backwards-compatible alias: `AllocType` is the historical name of the
/// per-allocator id. The id now lives in [`crate::types::AllocatorId`] (beside the
/// full type table it indexes), but every existing path —
/// `beam_alloc::AllocType::Ets`, `beam_alloc::registry::AllocType`,
/// `beam_alloc::prelude::AllocType` — keeps working unchanged, including
/// `AllocType::ALL`, `as_str()` and `index()`.
pub use crate::types::AllocatorId as AllocType;

/// One lazily-created allocator per [`AllocatorId`], indexed by
/// [`AllocatorId::index`].
static ALLOCATORS: LazyLock<[AllocRef; 12]> = LazyLock::new(|| {
    AllocatorId::ALL.map(|ty| -> AllocRef {
        let config = AllocatorConfig::builder().name(ty.as_str()).build();
        // `CarrierAllocator::new` only reserves bookkeeping and maps no carrier, so a
        // failure here means a broken build, not runtime OOM. This `expect` is the
        // SINGLE permitted panic on the allocator path (registry initialisation);
        // the per-call allocator API never panics on OOM — it returns
        // `BeamError::Alloc` (see the crate `CONTEXT.md`, Design Decisions).
        Arc::new(
            CarrierAllocator::new(config)
                .expect("beam_alloc registry: CarrierAllocator::new must not fail"),
        )
    })
});

/// The shared allocator registered for `ty`.
pub fn allocator(ty: AllocType) -> &'static AllocRef {
    &ALLOCATORS[ty.index()]
}

/// The default allocator (the [`AllocType::Standard`] instance).
pub fn default_allocator() -> &'static AllocRef {
    allocator(AllocatorId::Standard)
}

/// A snapshot of every registered allocator's counters, in [`AllocatorId::ALL`]
/// order.
pub fn all_stats() -> Vec<(AllocType, AllocStats)> {
    AllocatorId::ALL
        .iter()
        .map(|&ty| (ty, allocator(ty).stats()))
        .collect()
}

/// The names of the *alloc-util* allocators, in [`AllocatorId::ALL`] order.
///
/// Mirrors the C `erts_alloc_util_allocators` (`erl_alloc.c:2789`), which returns
/// every allocator **except** `sys_alloc` (it is the raw system allocator, not a
/// carrier-based alloc-util allocator) and `test_alloc` (disabled by default and
/// only intended for the VM's own testing).
pub fn alloc_util_allocators() -> Vec<&'static str> {
    AllocatorId::ALL
        .iter()
        .filter(|ty| !matches!(ty, AllocatorId::System | AllocatorId::Test))
        .map(|ty| ty.as_str())
        .collect()
}

/// Every type bound to `allocator`, in [`crate::types::ALLOC_TYPES`] (file)
/// order — the Rust counterpart of the per-allocator type listing used by the
/// `allocator`-info reporting.
pub fn types_for(allocator: AllocatorId) -> Vec<crate::types::TypeId> {
    crate::types::types_for_allocator(allocator).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{TypeFlags, TypeId, ALLOC_TYPES, TYPE_COUNT};

    #[test]
    fn registry_provides_one_named_allocator_per_id() {
        for ty in AllocatorId::ALL {
            assert_eq!(allocator(ty).name(), ty.as_str());
        }
        assert_eq!(default_allocator().name(), AllocatorId::Standard.as_str());
    }

    #[test]
    fn all_stats_covers_every_allocator_in_order() {
        let stats = all_stats();
        assert_eq!(stats.len(), AllocatorId::ALL.len());
        for (snapshot, ty) in stats.iter().zip(AllocatorId::ALL) {
            assert_eq!(snapshot.0, ty);
        }
    }

    #[test]
    fn alloc_util_allocators_excludes_sys_and_test() {
        let names = alloc_util_allocators();
        assert_eq!(names.len(), 10);
        assert!(!names.contains(&"sys_alloc"));
        assert!(!names.contains(&"test_alloc"));
        assert!(names.contains(&"std_alloc"));
        assert!(names.contains(&"eheap_alloc"));
        // Declaration order is preserved.
        let expected: Vec<&str> = AllocatorId::ALL
            .iter()
            .filter(|ty| !matches!(ty, AllocatorId::System | AllocatorId::Test))
            .map(|ty| ty.as_str())
            .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn types_for_agrees_with_the_type_table() {
        let mut total = 0usize;
        for ty in AllocatorId::ALL {
            let ids: Vec<TypeId> = types_for(ty);
            assert!(!ids.is_empty());
            for id in &ids {
                assert_eq!(id.allocator(), ty);
                assert_eq!(id.flags(), TypeFlags::for_allocator(ty));
            }
            total += ids.len();
        }
        assert_eq!(total, TYPE_COUNT);
        assert_eq!(ALLOC_TYPES.len(), TYPE_COUNT);
    }

    #[test]
    fn alloc_type_alias_keeps_the_historic_paths() {
        // The alias must expose the same items the pre-refactor `registry::AllocType`
        // did: the variant names, `ALL`, `as_str` and `index`.
        let ty: AllocType = AllocType::Ets;
        assert_eq!(ty.as_str(), "ets_alloc");
        assert_eq!(AllocType::ALL.len(), 12);
        assert_eq!(ty.index(), AllocatorId::Ets.index());

        // `AllocType` and `AllocatorId` are literally the same type, so the alias
        // is fully interchangeable at every call site.
        let same: AllocatorId = ty;
        assert_eq!(allocator(same).name(), "ets_alloc");
        assert_eq!(
            allocator(AllocType::Standard).name(),
            default_allocator().name()
        );
    }
}
