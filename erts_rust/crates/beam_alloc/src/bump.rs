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

//! The raw bump-pointer term-construction primitives shared by the per-process term
//! heaps.
//!
//! Both [`crate::heap::Heap`] (fixed capacity) and [`crate::reserved_heap::ReservedHeap`]
//! (reservation-capable) are bump arenas: a base pointer, a bump pointer (`hp`) and a
//! limit. The *term-writing* logic is identical — reserve or reject, then write the
//! words at `hp` — so it lives here **exactly once** and each heap supplies only its own
//! `used`/`peak` bookkeeping. Nothing in this module allocates, moves memory or keeps
//! state: every helper is a pure function of the pointer and the available word count
//! it is handed.
//!
//! A rejected request is reported exactly as the heaps report exhaustion:
//! [`error::heap_exhausted`] (a [`BeamError::Alloc`](beam_common::BeamError::Alloc) of
//! [`AllocKind::Heap`](beam_common::AllocKind), i.e. *collect and retry*, never a fatal
//! OOM). A rejection **writes nothing** — a failed term construction can never leave a
//! partially written or corrupted term behind.
//!
//! `unsafe` is confined to this module's marked functions; every one carries a
//! `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale.

use beam_common::Result;
use beam_term::prelude::{make_list, make_tuple, try_make_arityval, Eterm, CAR_OFFSET, CDR_OFFSET};

use crate::error;

/// Reserves `n` contiguous words at `hp` for a **headerless** term, writing nothing.
///
/// This is the bump behind `Heap::alloc_words` / `ReservedHeap::alloc_words`: it only
/// decides whether the request fits. The caller advances its own bump pointer over the
/// returned words.
///
/// # Safety
///
/// `hp` must be the bump pointer of a live, exclusively-owned term heap: a pointer into
/// a writable region of at least `available` term words. When `available == 0`, `hp`
/// must be the (possibly one-past-the-end) end of that region. The returned pointer
/// aliases `hp`; the caller may use only its first `n` words, and must write them
/// before they are read.
pub(crate) unsafe fn alloc_words_at(
    hp: *mut Eterm,
    available: usize,
    n: usize,
) -> Result<*mut Eterm> {
    if n > available {
        return Err(error::heap_exhausted(n));
    }
    Ok(hp)
}

/// Reserves `1 + n` contiguous words at `hp` and writes the arityval header of an
/// `n`-arity tuple into the first, leaving the `n` element words uninitialised.
///
/// Returns the header pointer. An unrepresentable arity is rejected before any write
/// (the same [`beam_term`] check `Heap::alloc_tuple` has always applied).
///
/// # Safety
///
/// `hp` must be the bump pointer of a live, exclusively-owned term heap: a pointer into
/// a writable region of at least `available` term words. The returned pointer aliases
/// `hp`; the `n` element words after the header are uninitialised and must be written
/// by the caller before they are read.
pub(crate) unsafe fn alloc_tuple_at(
    hp: *mut Eterm,
    available: usize,
    n: usize,
) -> Result<*mut Eterm> {
    // Reject an unrepresentable arity first (and so also keep `n + 1` from overflowing).
    let header = try_make_arityval(n)?;
    if n + 1 > available {
        return Err(error::heap_exhausted(n + 1));
    }
    // SAFETY: the caller's contract makes `hp` valid for `available >= n + 1` writable
    // words, so the header word is in-bounds.
    unsafe { *hp = header };
    Ok(hp)
}

/// Builds a cons cell `[car, cdr]` at `hp` and returns its list term.
///
/// # Safety
///
/// `hp` must be the bump pointer of a live, exclusively-owned term heap: a pointer into
/// a writable region of at least `available` term words. A write happens only when
/// `available >= 2`; the returned term aliases `hp` and its words must not be read
/// before this call writes them.
pub(crate) unsafe fn cons_at(
    hp: *mut Eterm,
    available: usize,
    car: Eterm,
    cdr: Eterm,
) -> Result<Eterm> {
    if 2 > available {
        return Err(error::heap_exhausted(2));
    }
    // SAFETY: the caller's contract makes `hp` valid for `available >= 2` writable
    // words, so both cell words are in-bounds.
    unsafe {
        *hp.add(CAR_OFFSET) = car;
        *hp.add(CDR_OFFSET) = cdr;
    }
    Ok(make_list(hp as *const Eterm))
}

/// Builds a tuple from `elems` at `hp` (header plus element words) and returns its
/// tuple term.
///
/// # Safety
///
/// `hp` must be the bump pointer of a live, exclusively-owned term heap: a pointer into
/// a writable region of at least `available` term words. The whole `1 + elems.len()`
/// words are written here, so no uninitialised word is handed back; the returned term
/// aliases `hp`.
pub(crate) unsafe fn tuple_at(hp: *mut Eterm, available: usize, elems: &[Eterm]) -> Result<Eterm> {
    // SAFETY: forwarded unchanged — the caller asserted `hp` is valid for `available`
    // writable words and the element writes below stay within `1 + elems.len()`.
    let p = unsafe { alloc_tuple_at(hp, available, elems.len())? };
    for (i, elem) in elems.iter().enumerate() {
        // SAFETY: `alloc_tuple_at` succeeded, so `available >= 1 + elems.len()` and
        // `1 + i` is in-bounds for every `0 <= i < elems.len()`.
        unsafe { *p.add(1 + i) = *elem };
    }
    Ok(make_tuple(p))
}
