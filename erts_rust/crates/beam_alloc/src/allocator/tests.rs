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

//! The allocator seam's behaviour tests. The block locator and the header
//! plausibility check are only exercisable with an allocator in hand, so the
//! allocator's tests live here and are split by responsibility into the `basic`,
//! `typed`, `unsized_free` and `info` submodules. The facade pulls this module in
//! with `#[cfg(test)] mod tests;`.

use super::{padding_for, plausible_header_at, probe_alignments, AllocRef, Allocator, MIN_ALIGN};
use super::{AllocatorConfig, CarrierAllocator, PREV_FREE_BIT};

use core::alloc::Layout;
use core::ffi::c_void;
use std::ptr::NonNull;

use beam_common::Result;

use crate::block::{
    BlockFlags, BlockHdr, BlockHeader, ALL_FLAGS, BLOCK_HEADER_SIZE, CLASS_NONE, UNIT_SIZE,
};
use crate::carrier::os_page_size;
use crate::free_list::FreeList;
use crate::policy::{CarrierPolicy, PolicyOptionKind};
use crate::stats::{AllocStats, AllocatorInfo, InfoValue, TypeStats};
use crate::types::{AllocatorId, TypeId};

mod basic;
mod typed;
mod unsized_free;
mod info;

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).expect("test layout")
}

/// The value of a report item, by key — how a consumer reads the item list.
fn lookup(info: &AllocatorInfo, key: &str) -> Option<InfoValue> {
    info.items
        .iter()
        .find(|i| i.key == key)
        .map(|i| i.value)
}

fn allocator() -> CarrierAllocator {
    let config = AllocatorConfig::builder()
        .name("test")
        .carrier_size(64 * 1024)
        .build();
    CarrierAllocator::new(config).expect("allocator")
}

/// The [`TypeId`] named `name` (every name used here is one of the 243 active
/// types).
fn type_id(name: &str) -> TypeId {
    TypeId::from_name(name).unwrap_or_else(|| panic!("type {name} must be active"))
}
