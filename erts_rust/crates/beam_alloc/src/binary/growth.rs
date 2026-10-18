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

//! Capacity growth for the off-heap refcounted payload ([`RefcBinary`]).
//!
//! # Why this exists
//!
//! BEAM's writable-binary idiom — `bs_init_writable` followed by a chain of
//! `private_append` instructions — appends into one growing buffer. Without a capacity
//! word, every append past the current allocation would have to rebuild the whole payload,
//! which is O(n²) in the number of appended bytes. The payload therefore carries a
//! *reservation* (`header.reserved`) and a *writable capacity* (`header.capacity`), and
//! growth follows the C's `GROW_PROC_BIN_SIZE` schedule (double up to 16 MiB, then +20 %,
//! `erts/emulator/beam/erl_bits.c:70-73`), which makes an append loop amortised linear:
//! each byte is copied O(1) times on average, not O(n).
//!
//! # The relocation/fixup protocol (what keeps the frozen term contract honest)
//!
//! The payload is **off-heap**, and the on-heap `[BinRef][ErlSubBits]` pair records only
//! the payload's first data byte (`BinRef.val`) and the *logical* bit window — never the
//! capacity. Two consequences follow, and they are the whole of the host-side contract:
//!
//! * [`GrowOutcome::InPlace`] guarantees `data_ptr()` is **unchanged**. Every on-heap
//!   `BinRef.val` word that points at this payload, every GC copy of that word, and the
//!   engine's own `(base, capacity)` registry entry stay valid: **no fixup is owed**, and
//!   the new capacity is just read back ([`RefcBinary::capacity`], or
//!   [`payload_capacity`] from a raw pointer).
//! * [`GrowOutcome::Moved`] **invalidates every such word**. The payload was reallocated
//!   and copied, so the host must republish the term over the new pointer — exactly C's
//!   non-writable fallback arm (`erl_bits.c:1990-2010`), which allocates a fresh binary of
//!   `GROW_PROC_BIN_SIZE(used_size)` and copies the live bytes. [`Relocation`] carries the
//!   old and new data pointers (plus the copied length and their delta) so a host holding
//!   several references to one payload can find and rewrite each of them.
//!
//! Because the payload lives outside the process heap, **no `beam_gc` root-set,
//! `StackFrame` or `RootSet` contract changes**: growth never moves, allocates or frees a
//! term, and it never touches the process heap. It only moves an off-heap byte buffer that
//! the term reaches through a pointer word.
//!
//! Growth is also **exclusive by construction**: every mutator takes `&mut self` and
//! refuses to run unless this handle is the only one left (`refcount() == 1`), mirroring
//! the C's `ASSERT(erts_refc_read(&refc_binary->intern.refc, 1) == 1)`
//! (`erl_bits.c:1968`). A shared payload can never be reallocated behind another handle's
//! back, so a stale `BinRef.val` can only ever come from a *recorded* move.
//!
//! `unsafe` is confined to this module's marked functions; every `unsafe fn` has a
//! `# Safety` section and every `unsafe` block a `// SAFETY:` rationale.

use core::alloc::Layout;

use beam_common::{BeamError, Result};

use super::{grow_span, payload_type, round_up_unit, BinaryHeader, RefcBinary, HEADER_SIZE};
use crate::allocator::MIN_ALIGN;
use crate::error;
use crate::refcount::RefCount;

/// What a capacity-growth operation did to the payload's address.
///
/// See the module documentation for the host-side protocol; the short version is that an
/// [`InPlace`](GrowOutcome::InPlace) outcome leaves every `BinRef.val` pointing at this
/// payload valid, while a [`Moved`](GrowOutcome::Moved) one invalidates them and hands the
/// host the means (a [`Relocation`]) to republish.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GrowOutcome {
    /// The payload did **not** move: `data_ptr()` is unchanged, so nothing has to be
    /// republished. `capacity` is the payload's capacity after the call (it may be larger
    /// than requested, per the growth schedule, and is unchanged when the request was
    /// already satisfied).
    InPlace {
        /// The payload's capacity after the call, in bytes.
        capacity: usize,
    },
    /// The payload **moved**: it was reallocated at a new address and its live bytes were
    /// copied. Every pointer previously obtained from
    /// [`data_ptr`](RefcBinary::data_ptr) — including every on-heap `BinRef.val` word and
    /// any GC copy of one — is now stale and must be republished over the relocation's
    /// [`to`](Relocation::to) address.
    Moved(Relocation),
}

/// Where a moved payload was, where it is now, and how much was copied.
///
/// All three pointers/lengths describe the *payload* (they are `data_ptr()` values, i.e.
/// first-data-byte addresses), never the internal allocation base.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Relocation {
    from: *mut u8,
    to: *mut u8,
    len: usize,
}

impl Relocation {
    /// The payload's first data byte **before** the move (now dangling — the old block was
    /// freed).
    pub fn from(&self) -> *mut u8 {
        self.from
    }

    /// The payload's first data byte **after** the move: equal to the mover's current
    /// [`data_ptr`](RefcBinary::data_ptr), and the value a republished `BinRef.val` must
    /// hold.
    pub fn to(&self) -> *mut u8 {
        self.to
    }

    /// The number of **live** payload bytes that were copied (the control header is copied
    /// too, but it is not payload).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the relocation copied an empty payload (an empty binary that still had to
    /// move, e.g. a `compact` of a reserved-but-unwritten payload).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The byte offset to add to a stale payload pointer to obtain its new value
    /// (`to() - from()`).
    ///
    /// This is the constant a host uses to re-base *any* pointer into the payload it holds
    /// (e.g. an `ErlSubBits` base, or an `offset`-adjusted window base), not just
    /// [`from`](Relocation::from) itself. Computed with `wrapping_sub`, so it is a delta
    /// even when the allocator happened to pick a lower address.
    pub fn delta(&self) -> isize {
        (self.to as isize).wrapping_sub(self.from as isize)
    }

    /// The number of live payload bytes the move copied — always
    /// [`len`](Relocation::len); named explicitly for hosts that account the cost of a move.
    pub fn copied_bytes(&self) -> usize {
        self.len
    }
}

/// Structural growth counters for one [`RefcBinary`] handle (no wall clock, so tests can
/// assert the *shape* of the growth curve).
///
/// They record what *this* handle did: a [`Clone`](RefcBinary) starts its own counters at
/// zero (and, being a shared handle, can never grow the payload at all). A payload created
/// by [`with_allocator`](RefcBinary::with_allocator) or another constructor reports all
/// zeros.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GrowthCounters {
    /// How many growth requests were satisfied by raising the capacity **in place** (only
    /// counted when the capacity actually rose).
    pub in_place_grows: usize,
    /// How many times the payload had to be reallocated and moved.
    pub moves: usize,
    /// The total number of live payload bytes those moves copied (the amortised cost).
    pub bytes_copied: usize,
}

/// The writable capacity, in bytes, of a **live** off-heap payload, read from its control
/// header.
///
/// This answers the question a host asks about a `BinRef.val` it has just read from a live
/// `ErlSubBits`: "is this payload still writable in place, and how wide is it?" It is the
/// raw-pointer equivalent of [`RefcBinary::capacity`], for hosts whose handle registry may
/// have gone stale (a payload reachable only through a term).
///
/// # Safety
///
/// `data_ptr` must be the [`data_ptr()`](RefcBinary::data_ptr) of a **live**
/// [`RefcBinary`] payload — i.e. a pointer obtained from a handle that is still alive, or
/// from an on-heap `BinRef.val` word whose owning process still holds (a clone of) that
/// handle — **and** it must still be the payload's *current* address (no
/// [`GrowOutcome::Moved`] happened since the word was read). The function reads the
/// control header at `data_ptr - HEADER_SIZE`, which exists only under that contract;
/// reading it for a dangling or stale pointer is undefined behaviour.
pub unsafe fn payload_capacity(data_ptr: *const u8) -> usize {
    // SAFETY: per the contract above, `data_ptr` is a live payload's first data byte, so
    // `data_ptr - HEADER_SIZE` is the initialised `BinaryHeader` of that allocation; the
    // pointer arithmetic stays inside the allocation and the read forms no reference and
    // touches nothing but the `capacity` word.
    let header = unsafe { data_ptr.sub(HEADER_SIZE).cast::<BinaryHeader>() };
    // SAFETY: `header` is that initialised `BinaryHeader` (see above); reading one `usize`
    // field of it is in bounds and needs no initialised neighbours.
    unsafe { (*header).capacity }
}

impl RefcBinary {
    /// This handle's growth counters (a clone's start at zero — see [`GrowthCounters`]).
    pub fn growth_counters(&self) -> GrowthCounters {
        GrowthCounters {
            in_place_grows: self.in_place_grows,
            moves: self.moves,
            bytes_copied: self.bytes_copied,
        }
    }

    /// Refuses to mutate a payload another handle still shares.
    ///
    /// The C asserts the same precondition before a writable-binary realloc
    /// (`erl_bits.c:1968`); here it is a typed [`BeamError::BadArg`] instead of an
    /// assertion, because a shared payload legitimately reaches this path (a host may hold
    /// a clone of every off-heap payload it published).
    fn require_unique(&self) -> Result<()> {
        let refcount = self.refcount();
        if refcount != 1 {
            return Err(error::payload_shared(refcount));
        }
        Ok(())
    }

    /// Grows the payload so that its capacity is **at least** `capacity` bytes.
    ///
    /// Three outcomes, all leaving `len()` (and therefore the payload's bytes) untouched:
    ///
    /// 1. The request already fits: [`GrowOutcome::InPlace`] with the *current* capacity —
    ///    no allocation and no counter bump (the capacity did not rise).
    /// 2. The grown target fits the existing reservation: the capacity is committed in
    ///    place ([`GrowOutcome::InPlace`], `in_place_grows` bumped); `data_ptr()` does not
    ///    move.
    /// 3. Otherwise the payload is reallocated, copied and freed
    ///    ([`GrowOutcome::Moved`], `moves`/`bytes_copied` bumped).
    ///
    /// The grown target is `min(max(grow_span(current_capacity), capacity), max_capacity)`
    /// — the C's `GROW_PROC_BIN_SIZE` schedule, capped by the handle's
    /// [`max_capacity`](RefcBinary::max_capacity). A request beyond that cap is the typed
    /// `AllocKind::LargeObject` failure and changes **nothing at all**.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] if the payload is still shared (`refcount != 1`);
    /// [`BeamError::Alloc`] (`AllocKind::LargeObject`) if the request exceeds
    /// `max_capacity` or the new block cannot be allocated.
    pub fn ensure_capacity(&mut self, capacity: usize) -> Result<GrowOutcome> {
        self.require_unique()?;
        let current = self.capacity();
        if capacity <= current {
            // Already wide enough: nothing is allocated, nothing moves, and the capacity
            // did not rise, so no counter is bumped either.
            return Ok(GrowOutcome::InPlace { capacity: current });
        }
        let max_capacity = self.max_capacity();
        if capacity > max_capacity {
            return Err(error::large_object_failure(capacity));
        }
        // `capacity <= target <= max_capacity`, so the request is always covered.
        let target = grow_span(current).max(capacity).min(max_capacity);
        let reserved = self.reserved_bytes();
        if target <= reserved {
            self.header_mut().capacity = target;
            self.in_place_grows += 1;
            return Ok(GrowOutcome::InPlace { capacity: target });
        }
        // `target <= max_capacity <= MAX_PAYLOAD_BYTES`, so the rounded-up reservation is
        // clamped only by the policy cap and can never exceed what the allocator serves.
        let new_reserved = round_up_unit(grow_span(reserved).max(target)).min(max_capacity);
        Ok(GrowOutcome::Moved(self.relocate(new_reserved, target)?))
    }

    /// Grows the payload so that it can hold `extra` more bytes than it currently does
    /// (`ensure_capacity(len() + extra)`, saturating).
    ///
    /// The single-implementation entry point for "I am about to append `extra` bytes"; see
    /// [`ensure_capacity`](RefcBinary::ensure_capacity) for the outcomes and errors.
    pub fn reserve(&mut self, extra: usize) -> Result<GrowOutcome> {
        self.ensure_capacity(self.len().saturating_add(extra))
    }

    /// Appends `bytes` to the payload, growing it first if necessary, and returns what the
    /// growth did.
    ///
    /// The appended bytes become part of [`as_bytes`](RefcBinary::as_bytes); the caller
    /// must honour the outcome exactly as for
    /// [`ensure_capacity`](RefcBinary::ensure_capacity) (republish on
    /// [`GrowOutcome::Moved`]).
    ///
    /// # Errors
    ///
    /// As [`ensure_capacity`](RefcBinary::ensure_capacity); on an error the payload is
    /// left completely unchanged (no bytes are written).
    pub fn append(&mut self, bytes: &[u8]) -> Result<GrowOutcome> {
        let extra = bytes.len();
        let outcome = self.reserve(extra)?;
        if extra > 0 {
            let len = self.len();
            debug_assert!(len + extra <= self.capacity(), "reserve made room for it");
            // SAFETY: the reserve above guarantees `capacity >= len + extra`, so
            // `[len, len + extra)` lies inside the payload allocation; those bytes were
            // uninitialised (beyond the logical length) and `bytes` is a distinct, fully
            // initialised source slice that cannot alias the off-heap payload.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.data_ptr().add(len), extra);
            }
            self.header_mut().len = len + extra;
        }
        Ok(outcome)
    }

    /// Sets the logical payload length, zero-filling any bytes this newly exposes.
    ///
    /// Shrinking is free (the bytes stay but are no longer part of the payload). Growing
    /// zero-fills `[old_len, len)` — required, not cosmetic: [`as_bytes`](RefcBinary::as_bytes)
    /// would otherwise expose uninitialised off-heap bytes, which is undefined behaviour.
    /// Growing past the current [`capacity`](RefcBinary::capacity) never silently
    /// reallocates; call [`ensure_capacity`](RefcBinary::ensure_capacity) first.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] if the payload is still shared or if `len` exceeds the
    /// capacity; in both cases the payload is unchanged.
    pub fn set_len(&mut self, len: usize) -> Result<()> {
        self.require_unique()?;
        let capacity = self.capacity();
        if len > capacity {
            return Err(error::payload_len_exceeds_capacity(len, capacity));
        }
        let old_len = self.len();
        if len > old_len {
            // SAFETY: `capacity >= len`, so `[old_len, len)` is inside the payload
            // allocation; those bytes are beyond the logical length and therefore
            // uninitialised, so writing zeros over them cannot clobber live data.
            unsafe { core::ptr::write_bytes(self.data_ptr().add(old_len), 0, len - old_len) };
        }
        self.header_mut().len = len;
        Ok(())
    }

    /// Releases the spare capacity: reallocates (if needed) so that the payload reserves
    /// and offers exactly [`len`](RefcBinary::len) writable bytes.
    ///
    /// This is the "publish then shrink" step a host runs once an accumulator is finished:
    /// the payload keeps precisely the bytes it uses, and the reservation (the amount
    /// `alloc` reports and the amount the process is charged for) drops to `len` rounded up
    /// to [`UNIT_SIZE`](crate::block::UNIT_SIZE). The bytes are unchanged.
    ///
    /// Returns [`GrowOutcome::InPlace`] when the reservation is already tight (only the
    /// capacity word is lowered — a *reduction*, so no counter is bumped), and
    /// [`GrowOutcome::Moved`] when a smaller block was allocated.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] if the payload is still shared; the payload is unchanged.
    pub fn compact(&mut self) -> Result<GrowOutcome> {
        self.require_unique()?;
        let len = self.len();
        // `max_capacity >= capacity >= len`, so the clamp can never shrink below `len`.
        let tight = round_up_unit(len).min(self.max_capacity());
        if self.reserved_bytes() == tight {
            self.header_mut().capacity = len;
            return Ok(GrowOutcome::InPlace { capacity: len });
        }
        Ok(GrowOutcome::Moved(self.relocate(tight, len)?))
    }

    /// Reallocates the payload into a block of `new_reserved` payload bytes and installs a
    /// fresh control header carrying `capacity` (`<= new_reserved`) and the current `len`.
    ///
    /// The new block is obtained **first**, so an allocation failure leaves the payload —
    /// and every pointer into it — completely valid; only then is the old block freed with
    /// its own exact layout.
    ///
    /// # Errors
    ///
    /// [`BeamError::Alloc`] (`AllocKind::LargeObject`) if the new block cannot be
    /// allocated.
    fn relocate(&mut self, new_reserved: usize, capacity: usize) -> Result<Relocation> {
        debug_assert!(capacity <= new_reserved, "the capacity must fit the reservation");
        debug_assert!(
            new_reserved <= self.max_capacity(),
            "the caller clamps the reservation to the policy cap"
        );
        let len = self.len();
        // Every size here was already accepted as a `Layout` once (the current allocation's
        // own, or a reservation the caller validated), so the recomputation is infallible;
        // it is still written fallibly so a hostile size is a typed error, never a panic.
        let old_layout = payload_layout(self.reserved_bytes())?;
        let new_layout = payload_layout(new_reserved)?;
        let old_base = self.base;
        // The old payload (header + live bytes) is byte-identical in the new block, except
        // for the header the relocation rewrites, so only `HEADER_SIZE + len` bytes move.
        let new_base = match self.alloc.alloc_typed(payload_type(), new_layout) {
            Ok(ptr) => ptr,
            Err(BeamError::Alloc(_)) => {
                return Err(error::large_object_failure(
                    HEADER_SIZE.saturating_add(new_reserved),
                ))
            }
            Err(other) => return Err(other),
        };
        // SAFETY: `old_base` and `new_base` are distinct live allocations (the new one was
        // just obtained, the old one is still owned), each of at least
        // `HEADER_SIZE + len` initialised bytes — the old header plus its `len` live payload
        // bytes — so the copy is in bounds and the regions cannot overlap; the fresh header
        // is then written at `new_base` and its reference count starts at 1 because this
        // handle is the unique owner (`require_unique`).
        unsafe {
            core::ptr::copy_nonoverlapping(old_base.as_ptr(), new_base.as_ptr(), HEADER_SIZE + len);
            new_base
                .cast::<BinaryHeader>()
                .as_ptr()
                .write(BinaryHeader {
                    refc: RefCount::new(1),
                    len,
                    capacity,
                    reserved: new_reserved,
                });
        }
        let old_data = old_base.as_ptr().wrapping_add(HEADER_SIZE);
        // SAFETY: `old_base`/`old_layout` are exactly the pointer and layout the current
        // allocation obtained from `self.alloc` through `alloc_typed(payload_type(), ..)`,
        // the payload has just been copied out of it, and it is not used again after this
        // call (the base is replaced below). Same allocator, same type, so the per-type and
        // aggregate counters are unwound together.
        unsafe { self.alloc.dealloc_typed(payload_type(), old_base, old_layout) };
        self.base = new_base;
        self.moves += 1;
        self.bytes_copied += len;
        Ok(Relocation {
            from: old_data,
            to: self.data_ptr(),
            len,
        })
    }
}

/// The [`Layout`] of a payload block reserving `reserved` payload bytes.
///
/// `pub(super)` so the one allocation site in the parent module
/// ([`RefcBinary::allocate`]) and [`Drop`] size their blocks with exactly this function:
/// the pointer handed out here, the layout [`relocate`](RefcBinary::relocate) frees, and
/// the layout the final drop frees with are then provably the same computation.
pub(super) fn payload_layout(reserved: usize) -> Result<Layout> {
    let total = HEADER_SIZE
        .checked_add(reserved)
        .ok_or_else(|| error::large_object_failure(reserved))?;
    Layout::from_size_align(total, MIN_ALIGN).map_err(|_| error::bad_align(MIN_ALIGN))
}
