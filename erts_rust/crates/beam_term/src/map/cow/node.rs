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

//! HAMT node classification and slot arithmetic: the C's `primary_tag` dispatch
//! over one child slot word, a node header's layout facts, and the bitmap slot map.

use crate::boxed::{
    boxed_val, is_arity_value, is_boxed_term, is_flatmap_header, is_map_header, map_header_type,
    map_header_val,
};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::list::is_list;
use crate::map::{make_map_header, HAMT_FULL_BITMAP};
use crate::tag::{
    MAP_HEADER_TAG_HAMT_HEAD_ARRAY, MAP_HEADER_TAG_HAMT_HEAD_BITMAP,
    MAP_HEADER_TAG_HAMT_NODE_BITMAP,
};

// ---------------------------------------------------------------------------
// Node classification (the C's `primary_tag` dispatch)
// ---------------------------------------------------------------------------

/// What a HAMT child slot holds.
pub(super) enum Child {
    /// A two-word `[Key, Value]` cons cell.
    Leaf(Eterm),
    /// A deeper hash-map node, at its header word.
    Node(*const Eterm),
    /// A collision node: an `arityval` tuple of leaf cells.
    Collision(Eterm),
}

/// Classifies one HAMT child slot word (`erl_map.c:2538`'s `primary_tag` switch).
pub(super) fn classify_child(t: Eterm) -> TermResult<Child> {
    if is_list(t) {
        return Ok(Child::Leaf(t));
    }
    if !is_boxed_term(t) {
        return Err(TermError::BadMap);
    }
    let p = boxed_val(t) as *const Eterm;
    // SAFETY: `t` is a live boxed term, so `p` addresses its header word.
    let header = unsafe { *p };
    if is_map_header(header) {
        if is_flatmap_header(header) {
            // A flat map can never be a hash-map child.
            return Err(TermError::BadMap);
        }
        Ok(Child::Node(p))
    } else if is_arity_value(header) {
        Ok(Child::Collision(t))
    } else {
        Err(TermError::BadMap)
    }
}

/// The layout facts of a hash-map node header: `(slot-word offset, bitmap, is a
/// head)`. `(2, HAMT_FULL_BITMAP, true)` for an 18-word head array.
pub(super) fn node_layout(header: Eterm) -> TermResult<(usize, u16, bool)> {
    match map_header_type(header) as usize {
        MAP_HEADER_TAG_HAMT_HEAD_ARRAY => Ok((2, HAMT_FULL_BITMAP, true)),
        MAP_HEADER_TAG_HAMT_HEAD_BITMAP => Ok((2, map_header_val(header) as u16, true)),
        MAP_HEADER_TAG_HAMT_NODE_BITMAP => Ok((1, map_header_val(header) as u16, false)),
        _ => Err(TermError::BadMap),
    }
}

/// The output slot index of nibble `nib` in a node with `bitmap`: the number of
/// set bits below it (`hashmap_bitcount(bmp & (bp - 1))`, `erl_map.h:206`).
pub(super) fn slot_of(bitmap: u16, bit: u16) -> usize {
    (bitmap & (bit - 1)).count_ones() as usize
}

/// The header word of a head/inner bitmap node with `bitmap` set bits. A head
/// whose bitmap is full is the 18-word `MAP_HEADER_HAMT_HEAD_ARRAY`
/// (`erl_map.c:2748-2750`); an inner full-bitmap node stays a bitmap
/// (`erl_map.c:1146`).
pub(super) fn bitmap_header(bitmap: u16, head: bool) -> Eterm {
    if head {
        if bitmap == HAMT_FULL_BITMAP {
            make_map_header(MAP_HEADER_TAG_HAMT_HEAD_ARRAY, 1, HAMT_FULL_BITMAP as usize)
        } else {
            make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, bitmap as usize)
        }
    } else {
        make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, bitmap as usize)
    }
}
