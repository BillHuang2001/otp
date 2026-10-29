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

//! Off-heap ("refc") binaries: the `BinRef` + `ErlSubBits` on-heap pair.
//!
//! A binary larger than [`ERL_ONHEAP_BINARY_LIMIT`] (or a bitstring exceeding
//! [`ERL_ONHEAP_BITS_LIMIT`] bits) is not copied onto the process heap: its payload
//! lives in an **off-heap refcounted object** owned by `beam_alloc`/`beam_gc`, and
//! the term is an on-heap **pair**:
//!
//! ```text
//!   hp[0] = HEADER_BIN_REF      // BinRef.thing_word  = make_header(2, BIN_REF_SUBTAG)
//!   hp[1] = val                 // BinRef.val   -> the off-heap payload
//!   hp[2] = next                // BinRef.next  -> the MSO off-heap chain
//!   hp[3] = HEADER_SUB_BITS     // ErlSubBits.thing_word = make_header(4, SUB_BITS_SUBTAG)
//!   hp[4] = base_flags          // base pointer | flags
//!   hp[5] = start               // bit offset into the base
//!   hp[6] = end                 // end bit offset (size = end - start)
//!   hp[7] = orig                // = make_boxed(hp)  (a boxed pointer to the BinRef)
//! ```
//!
//! and the tagged root is `make_boxed(hp + 3)` — the **`ErlSubBits`**, which is what
//! a term points at (`erts/emulator/beam/erl_bits.c:2672-2712`,
//! `erts_wrap_refc_bitstring`: `br = *hpp`, `sb = &(*hpp)[ERL_BIN_REF_SIZE]`,
//! `return make_bitstring(sb)`). Note the ordering: the `BinRef` sits at the **lower**
//! address and the `ErlSubBits` above it, so the root offset is
//! [`bin_ref_size_words`]. `ERL_REFC_BITS_SIZE` (8) is the pair's total size.
//!
//! A `BinRef` is never a user-visible term on its own — it is only reachable through
//! the `ErlSubBits::orig` slot — but a *sub-bitstring window* is just an `ErlSubBits`
//! (see [`crate::bitstring::write_sub_bits`]) whose `orig` is a `BinRef`.
//!
//! Ground truth: `erts/emulator/beam/erl_bits.h:58-176` (the `ErlSubBits`/`BinRef`/
//! `ErlHeapBits` definitions, `HEADER_SUB_BITS`/`HEADER_BIN_REF`,
//! `ERL_SUB_BITS_SIZE`/`ERL_BIN_REF_SIZE`/`ERL_REFC_BITS_SIZE`, the on-heap limits)
//! and `erl_bits.c:2672-2712`.
//!
//! # Frozen inter-crate contract (`beam_alloc` / `beam_gc`)
//!
//! * The on-heap `BinRef.val` word holds a raw pointer to the **first data byte** of
//!   the off-heap refcounted payload. Where that payload's own header lives (and how
//!   it is refcounted / swept) is `beam_alloc`'s and `beam_gc`'s internal concern —
//!   `beam_term` only reads/writes the pointer word ([`write_bin_ref`] /
//!   [`bin_ref_parts`]).
//! * `ErlSubBits.orig` is a **term**: a boxed pointer to the `BinRef` (a full
//!   refc binary), or an on-heap bitstring when the `ErlSubBits` is a match context
//!   (`ERL_SUB_BITS_FLAG_MUTABLE`).
//! * The 8-word on-heap arrangement is exactly `[BinRef(3)][ErlSubBits(5)]` with the
//!   root at `hp + 3`, as written by [`write_refc_binary`]; `beam_gc` must move/copy
//!   all 8 words together and fix up `orig` to the new `BinRef` address.
//!
//! # Sub-binary windows ([`write_sub_binary`]) and `binary:referenced_byte_size/1`
//!
//! [`write_sub_binary`] is this crate's port of `erts_build_sub_bitstring`
//! (`erl_bits.c:2637-2665`), the one place that decides **which arm** a window takes:
//!
//! * `bit_size <= ERL_ONHEAP_BITS_LIMIT` — a fresh `ErlHeapBits` holding a *shifted copy*
//!   of the window bits (the C's `HEAP_BITSTRING`, whose `copy_binary_to_buffer` re-aligns
//!   MSB-first when the source offset is not byte aligned, `erl_bits.h:541-557`);
//! * otherwise — the 8-word `[BinRef][ErlSubBits]` pair over the **parent's payload**,
//!   `start` = the parent's bit offset + `bit_offset` (the C's `erts_make_sub_bitstring`
//!   convention, `erl_bits.c:2714-2745`), so the payload — and the referenced size — stays
//!   reachable from the child.
//!
//! [`sub_binary_words`] answers the same decision's reservation without writing (the C's
//! `erts_extracted_bitstring_size`), and [`refc_payload`] is the one call a consumer needs
//! to look a refc binary's payload up in its off-heap table.
//!
//! ## Why the referenced size cannot live in the term
//!
//! The frozen representation has no room for a parent capacity: `BinRef` is
//! `ERL_BIN_REF_SIZE` = 3 words, its header `_make_header(2, BIN_REF_SUBTAG)` — arity 2,
//! exactly `[val, next]` (`erl_bits.h:135-147`); a fourth word would move
//! `HEADER_BIN_REF`/`ERL_REFC_BITS_SIZE` (8 → 9) and with them
//! [`crate::layout::object_words`], the copier's price (`beam_gc`), `size_object`, message
//! sizing and `max_heap_size` accounting, and the shape would no longer be bit-identical to
//! the oracle's (workspace `CONTEXT.md` §2). `ErlSubBits`' only spare bits are the two low
//! bits of `base_flags`, and both are spoken for (`_MUTABLE` = match context, `_VOLATILE` =
//! the data may move) by the base pointer's alignment scheme (`erl_bits.h:60-95`).
//!
//! The capacity is a property of the **payload** — the C's off-heap `Binary.orig_size`
//! (`erl_bits.h:75`), whose frozen analogue is `beam_alloc::RefcBinary`'s header — so this
//! crate only hands back the payload key.
//!
//! So `binary:referenced_byte_size/1` (`erl_bif_binary.c:2691-2717`) is, for a **refc
//! window** — including a plain refc binary, a window with `start == 0` — the payload's
//! allocated size, never the window's (the oracle's `1025` for a 510-byte part of a
//! 1025-byte binary; `2050` for an `erl_eval`-built one whose payload was allocated at its
//! grown width while `byte_size` is 1025). `RefcBinary::capacity()` is today's closest
//! figure — `orig_size` exactly when the payload was allocated at the width the C would have
//! asked for, differing after a growth-then-trim. An **`ErlHeapBits`** instead answers its
//! own bytes and a **match context** its own window's, because its `orig` is not a `BinRef`
//! (the C's `br == NULL`).
//!
//! # `unsafe` contract
//!
//! Every function is `unsafe`: an `Eterm` being a live, well-formed term (and the
//! off-heap payload being alive) is a caller invariant, and reading or writing a raw
//! heap word has no safe form. Each documents, under `# Safety`, that invariant and
//! the lifetime of any borrowed slice.

use crate::bitstring::{
    sub_bits_parts, ERL_SUB_BITS_FLAG_MASK, SUB_BITS_BASE_WORD, SUB_BITS_END_WORD,
    SUB_BITS_ORIG_WORD, SUB_BITS_START_WORD,
};
use crate::boxed::{
    arityval, boxed_val, header_subtag, is_bin_ref, is_boxed_term, make_boxed, ERL_BIN_REF_SIZE,
    ERL_SUB_BITS_SIZE, HEADER_BIN_REF, HEADER_SUB_BITS,
};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::tag::HEAP_BITS_SUBTAG;

/// `ERL_BIN_REF_SIZE` (`erl_bits.h:144`): the words in a `BinRef`
/// (`[thing_word, val, next]`).
#[inline]
pub const fn bin_ref_size_words() -> usize {
    ERL_BIN_REF_SIZE
}

/// `ERL_SUB_BITS_SIZE` (`erl_bits.h:128`): the words in an `ErlSubBits`
/// (`[thing_word, base_flags, start, end, orig]`).
#[inline]
pub const fn sub_bits_size_words() -> usize {
    ERL_SUB_BITS_SIZE
}

/// `ERL_REFC_BITS_SIZE` (`erl_bits.h:147`): the words the on-heap
/// `BinRef` + `ErlSubBits` pair occupies (`ERL_BIN_REF_SIZE + ERL_SUB_BITS_SIZE`).
pub const ERL_REFC_BITS_SIZE: usize = ERL_BIN_REF_SIZE + ERL_SUB_BITS_SIZE;

/// `ERL_ONHEAP_BINARY_LIMIT` (`erl_bits.h:167`): the largest binary (in **bytes**)
/// stored on the heap as an `ErlHeapBits`; larger binaries become refc binaries.
pub const ERL_ONHEAP_BINARY_LIMIT: usize = 64;

/// `ERL_ONHEAP_BITS_LIMIT` (`erl_bits.h:168`): the largest bitstring (in **bits**)
/// stored on the heap as an `ErlHeapBits`; larger bitstrings become sub-bitstrings.
pub const ERL_ONHEAP_BITS_LIMIT: usize = ERL_ONHEAP_BINARY_LIMIT * 8;

// ---------------------------------------------------------------------------
// BinRef
// ---------------------------------------------------------------------------

/// Writes a `BinRef` block at `hp` and returns the tagged root.
///
/// Layout pinned to `erl_bits.h:135-144`: `[HEADER_BIN_REF, val, next]` (3 words),
/// with the root `make_boxed(hp)`. In ERTS a `BinRef` is `{thing_word, Binary *val,
/// off_heap_header *next}`; per the frozen inter-crate contract (see the module docs)
/// `val` here holds a raw pointer to the payload's **first data byte** and `next`
/// threads the process's off-heap (MSO) chain.
///
/// # Safety
///
/// `hp` must point at [`bin_ref_size_words`] (3) exclusively-owned, writable words,
/// which stay live (and unmoved) for as long as the returned term is used. `val` must
/// stay valid for that long, and `next` must be a valid off-heap chain link (or null).
pub unsafe fn write_bin_ref(hp: *mut Eterm, val: *mut Eterm, next: *mut Eterm) -> Eterm {
    // SAFETY: the caller reserved the 3-word `BinRef` block at `hp`; every word is
    // written before the pointer escapes.
    unsafe {
        *hp = HEADER_BIN_REF;
        *hp.add(1) = Eterm::from_raw(val as usize);
        *hp.add(2) = Eterm::from_raw(next as usize);
    }
    make_boxed(hp)
}

/// Splits a `BinRef` term into `(val, next)`.
///
/// The exact inverse of [`write_bin_ref`] (`erl_bits.h:135-139`). `val` points at the
/// payload's first data byte and `next` at the next off-heap chain link.
///
/// # Safety
///
/// `t` must be a live `BinRef` term (a boxed term whose header is `HEADER_BIN_REF`),
/// kept alive by the caller.
pub unsafe fn bin_ref_parts(t: Eterm) -> (*mut Eterm, *mut Eterm) {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees `t` is a live `BinRef`, so words 1 and 2 are
    // initialised by [`write_bin_ref`].
    unsafe {
        (
            (*p.add(1)).raw() as *mut Eterm,
            (*p.add(2)).raw() as *mut Eterm,
        )
    }
}

// ---------------------------------------------------------------------------
// The combined refc-binary term (BinRef + ErlSubBits)
// ---------------------------------------------------------------------------

/// Writes the full 8-word refc-binary pair at `hp` and returns the tagged root (the
/// `ErlSubBits`, at `hp + `[`bin_ref_size_words`]).
///
/// The arrangement is pinned to `erts_wrap_refc_bitstring`
/// (`erl_bits.c:2672-2712`): the `BinRef` at `hp[0..3]` (see [`write_bin_ref`]) and
/// the `ErlSubBits` at `hp[3..8]` (see [`crate::bitstring::write_sub_bits`]), whose
/// `orig` slot holds `make_boxed(hp)` — a boxed pointer back to the `BinRef`.
/// `base` is the off-heap payload's first data byte, `offset_bits`/`size_bits` the
/// window into it (a full binary uses `offset_bits = 0`).
///
/// # Safety
///
/// `hp` must point at [`ERL_REFC_BITS_SIZE`] (8) exclusively-owned, writable words,
/// which stay live (and unmoved) for as long as the returned term is used. `val` must
/// point at the off-heap payload's first data byte (kept alive by the caller),
/// `next` must be a valid off-heap chain link (or null), and `base` must point at
/// storage covering bits `offset_bits..offset_bits + size_bits` and be aligned so
/// that the low [`ERL_SUB_BITS_FLAG_MASK`] bits are clear (word-aligned, as the
/// `beam_alloc` payload contract guarantees).
pub unsafe fn write_refc_binary(
    hp: *mut Eterm,
    val: *mut Eterm,
    next: *mut Eterm,
    base: *const u8,
    offset_bits: usize,
    size_bits: usize,
) -> Eterm {
    debug_assert_eq!(
        base as usize & ERL_SUB_BITS_FLAG_MASK,
        0,
        "the off-heap payload base must be word-aligned (frozen contract)"
    );
    // SAFETY: the caller reserved `ERL_REFC_BITS_SIZE` (8) words: the `BinRef` at
    // `hp[0..3]` and the `ErlSubBits` at `hp[3..8]`. Every word is written before the
    // returned pointer escapes. `orig = make_boxed(hp)` is a boxed pointer to the
    // `BinRef`, exactly as `erts_wrap_refc_bitstring` sets it.
    unsafe {
        *hp = HEADER_BIN_REF;
        *hp.add(1) = Eterm::from_raw(val as usize);
        *hp.add(2) = Eterm::from_raw(next as usize);

        let sb = hp.add(ERL_BIN_REF_SIZE);
        *sb = HEADER_SUB_BITS;
        *sb.add(1) = Eterm::from_raw(base as usize);
        *sb.add(2) = Eterm::from_raw(offset_bits);
        *sb.add(3) = Eterm::from_raw(offset_bits + size_bits);
        *sb.add(4) = make_boxed(hp);
        make_boxed(sb)
    }
}

/// Whether `t` is a full refc binary: an `ErlSubBits` whose `orig` is a `BinRef`.
///
/// Reproduces the `*boxed_val(orig) == HEADER_BIN_REF` test
/// (`erl_bits.c:117`, `ERTS_GET_BITSTRING_REF`). An on-heap bitstring, a
/// match-context `ErlSubBits` (whose `orig` is an on-heap bitstring or a non-bin-ref
/// term), or any non-bitstring yields `false`.
///
/// # Safety
///
/// If `t` is a boxed term, its header word must be a live, aligned term word and — if
/// it is an `ErlSubBits` — its `orig` slot must be a live term.
pub unsafe fn is_refc_binary(t: Eterm) -> bool {
    if !is_boxed_term(t) {
        return false;
    }
    let p = boxed_val(t);
    // SAFETY: `t` is boxed, so its header word is live.
    if unsafe { *p } != HEADER_SUB_BITS {
        return false;
    }
    // SAFETY: the header fixed the block at the 5-word `ErlSubBits` layout, so `orig`
    // (word 4) is a live term.
    let orig = unsafe { *p.add(4) };
    // SAFETY: `orig` is a live term; `is_bin_ref` dereferences only a boxed header.
    unsafe { is_bin_ref(orig) }
}

/// The start of a refc binary's window, in **bits**.
///
/// This is the `ErlSubBits::start` slot (`erl_bits.h:76`) — a **bit** offset, not a
/// byte offset (the `ErlSubBits` layout stores bit offsets; a full binary has
/// `start == 0`). For the whole-binary view the payload's data base is the
/// `base_flags & !ERL_SUB_BITS_FLAG_MASK` pointer.
///
/// # Safety
///
/// `t` must be a live `ErlSubBits` term (e.g. produced by [`write_refc_binary`] or
/// [`crate::bitstring::write_sub_bits`]).
pub unsafe fn refc_offset(t: Eterm) -> usize {
    // SAFETY: `t` is a live `ErlSubBits`, so its `start` slot is initialised.
    unsafe { sub_bits_parts(t).1.raw() }
}

/// The size of a refc binary's window, in **bits** (`ErlSubBits::end - start`).
///
/// This is a **bit** count, matching [`refc_bytes`]'s
/// `ceil(refc_size(t) / 8)`-byte view (`erl_bits.h:53-55`,
/// `ERTS_GET_BITSTRING`'s `size`).
///
/// # Safety
///
/// `t` must be a live `ErlSubBits` term with `end >= start`.
pub unsafe fn refc_size(t: Eterm) -> usize {
    // SAFETY: `t` is a live `ErlSubBits`, so its `start`/`end` slots are initialised.
    let (_base_flags, start, end, _orig) = unsafe { sub_bits_parts(t) };
    end.raw() - start.raw()
}

/// A borrowed byte view of a refc binary's payload.
///
/// The slice starts at the byte containing bit [`refc_offset`] and spans through the
/// byte containing the last referenced bit, i.e. its length is
/// `ceil((offset + size) / 8) - floor(offset / 8)`. For a full binary
/// (`offset == 0`) that is exactly `ceil(refc_size(t) / 8)` bytes of the off-heap
/// payload.
///
/// # Safety
///
/// `t` must be a live `ErlSubBits` term whose underlying off-heap payload is alive
/// and will not move for `'a` (the caller holds a reference to the refcounted
/// payload — e.g. a `beam_alloc` binary handle). `end` must not precede `start`.
pub unsafe fn refc_bytes<'a>(t: Eterm) -> &'a [u8] {
    // SAFETY: `t` is a live `ErlSubBits`, so words 1..4 are initialised.
    let (base_flags, start, end, _orig) = unsafe { sub_bits_parts(t) };
    let base = (base_flags.raw() & !ERL_SUB_BITS_FLAG_MASK) as *const u8;
    let start = start.raw();
    let end = end.raw();
    let first_byte = start >> 3;
    let last_byte = crate::bitstring::bit_count_to_bytes(end);
    // SAFETY: the caller guarantees the payload covers bits `start..end`, so the bytes
    // `first_byte..last_byte` are inside it and stay live for `'a`.
    unsafe { core::slice::from_raw_parts(base.add(first_byte), last_byte - first_byte) }
}

/// The largest window the **heap** arm builds, in bytes: one
/// `ERL_ONHEAP_BITS_LIMIT`-bit `ErlHeapBits` payload.
const MAX_HEAP_WINDOW_BYTES: usize = crate::bitstring::bit_count_to_bytes(ERL_ONHEAP_BITS_LIMIT);

/// A windowable parent, decoded once by [`parent_window`]: `base`/`start`/`size` are the
/// frozen `ERTS_GET_BITSTRING_REF` triple (the parent's bit `i` is bit `start + i` of the
/// storage at `base`), and `payload` is `Some((val, next))` only for a **normal** refc
/// window — no flag bits, `orig` a live `BinRef` — the only shape a child may inherit.
struct ParentWindow {
    base: *const u8,
    start: usize,
    size: usize,
    payload: Option<(*mut Eterm, *mut Eterm)>,
}

/// Decodes `parent` into the frozen bitstring triple, validating its shape: an `ErlHeapBits`
/// header must price **exactly** the words its `size` word needs
/// (`arityval(hdr) == heap_bits_size_words(size) - 1`), so a size word contradicting its
/// header is a typed refusal rather than a read of storage the block does not own.
///
/// # Safety
///
/// `parent` must be a live boxed term (for a `SUB_BITS` one, also a live `orig` slot).
unsafe fn parent_window(parent: Eterm) -> TermResult<ParentWindow> {
    if !is_boxed_term(parent) {
        return Err(TermError::BadBitstring);
    }
    // SAFETY: `parent` is a live boxed block, whose word 0 is its header word.
    let p = boxed_val(parent);
    let hdr = unsafe { *p };
    if hdr == HEADER_SUB_BITS {
        // SAFETY: the header fixes the block at the 5-word `ErlSubBits` layout, so words
        // 1..4 are live (`erl_bits.h:60-82`).
        let base_flags = unsafe { *p.add(SUB_BITS_BASE_WORD) }.raw();
        let start = unsafe { *p.add(SUB_BITS_START_WORD) }.raw();
        let orig = unsafe { *p.add(SUB_BITS_ORIG_WORD) };
        // `end < start` is a malformed window (`bitstring_size` would wrap).
        let size = unsafe { *p.add(SUB_BITS_END_WORD) }
            .raw()
            .checked_sub(start)
            .ok_or(TermError::BadBitstring)?;
        // Only a *normal* sub-bits (`erl_sub_bits_is_normal`, `erl_bits.h:104`) may be
        // inherited: its flags say the context or its data may change under us (`MUTABLE` =
        // match context, `VOLATILE` = writable), which this crate cannot reproduce.
        let payload = if base_flags & ERL_SUB_BITS_FLAG_MASK != 0 {
            None
        } else {
            // SAFETY: `orig` is a live term; `is_bin_ref` tests only a boxed header.
            if unsafe { is_bin_ref(orig) } {
                // SAFETY: `is_bin_ref` proved `orig` is a live 3-word `BinRef`.
                let (val, next) = unsafe { bin_ref_parts(orig) };
                if val.is_null() {
                    return Err(TermError::BadBitstring);
                }
                Some((val, next))
            } else {
                None
            }
        };
        Ok(ParentWindow {
            base: (base_flags & !ERL_SUB_BITS_FLAG_MASK) as *const u8,
            start,
            size,
            payload,
        })
    } else if header_subtag(hdr) == HEAP_BITS_SUBTAG as u32 {
        // SAFETY: the header says `ErlHeapBits`, so word 1 is the bit-size word.
        let size = unsafe { *p.add(1) }.raw();
        // `heap_bits_size_words` is overflow-free and always >= 2, so the subtraction
        // cannot wrap; a mismatch means the block does not own the words it claims.
        if arityval(hdr) != crate::bitstring::heap_bits_size_words(size) - 1 {
            return Err(TermError::BadBitstring);
        }
        Ok(ParentWindow {
            // SAFETY: the arity check proved the block is `2 + ceil(n_bytes/8)` words, so
            // `p + 2` is the first payload byte of a live block.
            base: unsafe { p.add(crate::boxed::ERL_HEAP_BITS_SIZE) }.cast::<u8>(),
            start: 0,
            size,
            payload: None,
        })
    } else {
        // Not a bitstring at all (a tuple, a float, an immediate handled above, …).
        Err(TermError::BadBitstring)
    }
}

/// The words a [`write_sub_binary`] of `bit_size` bits needs, given a validated parent.
fn window_words(window: &ParentWindow, bit_size: usize) -> TermResult<usize> {
    if bit_size <= ERL_ONHEAP_BITS_LIMIT {
        Ok(crate::bitstring::heap_bits_size_words(bit_size))
    } else if window.payload.is_some() {
        Ok(ERL_REFC_BITS_SIZE)
    } else {
        // Above the on-heap limit only a refc parent can be windowed: an `ErlHeapBits`
        // parent (or a match context, or a flagged window) has no payload to share.
        Err(TermError::BadBitstring)
    }
}

/// How many words [`write_sub_binary`] will write for a `bit_size`-bit window of
/// `parent`: [`crate::bitstring::heap_bits_size_words`]`(bit_size)` for the heap arm,
/// else [`ERL_REFC_BITS_SIZE`] (8).
///
/// Reproduces `erts_extracted_bitstring_size` (`erl_bits.h:585-594`) for this crate's
/// 8-word pair — the C reserves `ERL_SUB_BITS_SIZE` (5) because it *reuses* the parent's
/// `BinRef`; this layer writes the whole pair.
///
/// The parent's shape is validated exactly as [`write_sub_binary`] validates it, so the two
/// can never disagree about the arm; the window's **bounds** need `bit_offset`, so the
/// writer's refusal is the authoritative one.
///
/// # Safety
///
/// As [`parent_window`]: `parent` must be a live boxed term.
///
/// # Errors
///
/// [`TermError::BadBitstring`] when `parent` is not a well-formed bitstring, or when the
/// window would exceed [`ERL_ONHEAP_BITS_LIMIT`] but `parent` is not a refc binary to share.
pub unsafe fn sub_binary_words(parent: Eterm, bit_size: usize) -> TermResult<usize> {
    // SAFETY: the caller guarantees `parent` is a live boxed term.
    let window = unsafe { parent_window(parent) }?;
    window_words(&window, bit_size)
}

/// Builds a sub-binary window of `bit_size` bits at `bit_offset` of `parent` at `hp`,
/// returning the tagged root — the crate's port of `erts_build_sub_bitstring`
/// (`erl_bits.c:2637-2665`).
///
/// The arm is chosen on the **window's** bit size, exactly as the C chooses it:
///
/// * `bit_size <= ERL_ONHEAP_BITS_LIMIT` ⇒ a fresh `ErlHeapBits` at `hp` holding a copy of
///   the window's bits, re-aligned MSB-first when the source offset is not byte aligned,
///   written by [`crate::bitstring::write_heap_bits`] so the header, the zero-filled padding
///   and the zeroed trailing bits are that writer's, not a second implementation's.
/// * otherwise ⇒ the 8-word `[BinRef][ErlSubBits]` pair ([`write_refc_binary`]) over the
///   parent's payload: `val`/`next` are the parent's, `base` is its data base, `start` is its
///   bit offset + `bit_offset`, `end` is `start + bit_size` (`erl_bits.c:2714-2745`).
///
/// The child's `orig` is its **own** `BinRef` (how [`write_refc_binary`] shapes a pair, and
/// what `beam_gc`'s copier fixes up), not the parent's word: two pairs naming one payload is
/// the frozen convention and `BinRef.val` is the identity consumers key on. A host that
/// refcounts rather than sweeping by reachability must retain the payload first, and must not
/// treat the child's `BinRef` as a *new* intrusive chain node (its `next` is the parent's).
///
/// # Safety
///
/// `hp` must point at the words `sub_binary_words(parent, bit_size)` reports —
/// exclusively-owned, writable and unmoved while the returned term is used; `parent` must be
/// a live boxed term; and the payload behind a refc parent must stay alive and unmoved (the
/// caller's retain).
///
/// # Errors
///
/// [`TermError::BadBitstring`] when `parent` is not a well-formed bitstring, when
/// `bit_offset + bit_size` overflows or exceeds the parent's bit size, or when the window
/// exceeds [`ERL_ONHEAP_BITS_LIMIT`] and `parent` is not a refc binary to share. Nothing is
/// written on any of those paths.
pub unsafe fn write_sub_binary(
    hp: *mut Eterm,
    parent: Eterm,
    bit_offset: usize,
    bit_size: usize,
) -> TermResult<Eterm> {
    // SAFETY: the caller guarantees `parent` is a live boxed term.
    let window = unsafe { parent_window(parent) }?;
    // The window must fit the parent: checked, so no wrapping add can produce an
    // in-range-looking offset.
    let end = bit_offset
        .checked_add(bit_size)
        .ok_or(TermError::BadBitstring)?;
    if end > window.size {
        return Err(TermError::BadBitstring);
    }
    let words = window_words(&window, bit_size)?;
    let src_bit = window
        .start
        .checked_add(bit_offset)
        .ok_or(TermError::BadBitstring)?;
    if words == ERL_REFC_BITS_SIZE {
        // `window_words` proved the parent is a normal refc window over a live payload.
        let (val, next) = window.payload.ok_or(TermError::BadBitstring)?;
        // SAFETY: the caller reserved `ERL_REFC_BITS_SIZE` (8) exclusively-owned words at
        // `hp`; `val` is the live payload's first data byte (word-aligned, per the
        // `beam_alloc` payload contract) and `next` its chain link; `window.base` is that
        // payload's data base, and `src_bit .. src_bit + bit_size` is inside the parent's
        // window, hence inside the payload.
        return Ok(unsafe {
            write_refc_binary(hp, val, next, window.base, src_bit, bit_size)
        });
    }
    let n_bytes = crate::bitstring::bit_count_to_bytes(bit_size);
    // `bit_size <= ERL_ONHEAP_BITS_LIMIT` here (the heap arm), so the buffer cannot overflow.
    debug_assert!(n_bytes <= MAX_HEAP_WINDOW_BYTES);
    let mut buf = [0u8; MAX_HEAP_WINDOW_BYTES];
    // SAFETY: `buf` is a `MAX_HEAP_WINDOW_BYTES` (64)-byte destination, `n_bytes <= 64`,
    // and `window.base` covers the parent's bits `window.start .. window.start + window.size`,
    // of which `src_bit .. src_bit + bit_size` are read (and `src_bit + bit_size <=
    // window.start + window.size` by the bounds check above).
    unsafe { copy_window_bits(buf.as_mut_ptr(), window.base, src_bit, bit_size) };
    // SAFETY: the caller reserved `words = heap_bits_size_words(bit_size)` words at `hp`,
    // and `buf[..n_bytes]` holds exactly `bit_size` bits MSB-first.
    unsafe { crate::bitstring::write_heap_bits(hp, &buf[..n_bytes], bit_size) }
}

/// Copies `size` bits starting at bit `src_bit` of `src` to bit 0 of `dst`, MSB-first.
///
/// The C's `copy_binary_to_buffer(dst, 0, src, src_bit, size)` (`erl_bits.h:541-557`): a
/// plain `memcpy` when the source offset is byte aligned, else the `erts_copy_bits_fwd`
/// shift. Bits of the last destination byte beyond `size` are left unspecified —
/// [`crate::bitstring::write_heap_bits`] masks them.
///
/// # Safety
///
/// `dst` must point at `ceil(size / 8)` writable bytes, and `src` at the storage holding
/// bits `src_bit .. src_bit + size` (the byte containing the last bit included).
unsafe fn copy_window_bits(dst: *mut u8, src: *const u8, src_bit: usize, size: usize) {
    let n_bytes = crate::bitstring::bit_count_to_bytes(size);
    // SAFETY: the caller guarantees `src` covers `src_bit .. src_bit + size`, so the byte
    // holding bit `src_bit` is inside it and `n_bytes` destination bytes are writable.
    let sb = unsafe { src.add(src_bit >> 3) };
    let off = src_bit & 7;
    if off == 0 {
        if n_bytes > 0 {
            // SAFETY: `n_bytes <= ceil(size/8)` source bytes from `sb` are the window's own
            // bytes and `dst` is a disjoint, writable buffer of that length.
            unsafe { core::ptr::copy_nonoverlapping(sb, dst, n_bytes) };
        }
        return;
    }
    for i in 0..n_bytes {
        // SAFETY: destination byte `i` holds window bits `8i .. 8i + 8`, all of which live
        // in source byte `i` (its low `8 - off` bits, shifted up) plus, when the window
        // reaches into it, source byte `i + 1` (its high `off` bits). `8 * (i + 1) <
        // size + off` is exactly "the window reaches into byte `i + 1`" — the C's
        // `soffs + n > 8` guard — so the second read never leaves the window's bytes.
        let hi = u16::from(unsafe { *sb.add(i) });
        let lo = if 8 * (i + 1) < size + off {
            u16::from(unsafe { *sb.add(i + 1) })
        } else {
            0
        };
        // SAFETY: `dst` has `n_bytes > i` writable bytes (the caller's contract).
        unsafe { *dst.add(i) = ((hi << off) | (lo >> (8 - off))) as u8 };
    }
}

/// The off-heap payload a refc binary references: the `BinRef.val` word — the key a
/// consumer looks up in its off-heap table to read the payload (and its allocated size).
///
/// The C's `br` half of `ERTS_GET_BITSTRING_REF` (`erl_bits.c:117`): `Ok` exactly when `t`
/// is an `ErlSubBits` whose `orig` is a live `BinRef` — a window built by
/// [`write_refc_binary`]/[`write_sub_binary`] **or** a plain refc binary (the same thing with
/// `start == 0`), the arm `binary:referenced_byte_size/1` answers with `(br->val)->orig_size`
/// (`erl_bif_binary.c:2691-2717`). Flag bits are *not* consulted: the C tests `orig` alone.
/// `Err(TermError::BadBitstring)` is the C's `br == NULL` (an on-heap `ErlHeapBits`, a match
/// context, or a non-bitstring) — never a guessed pointer; the module docs say why this
/// crate cannot answer the size itself.
///
/// # Safety
///
/// `t` must be a live boxed term, and the payload a returned pointer names (kept alive by
/// `t`, per the frozen `BinRef.val == RefcBinary::data_ptr()` contract) must stay alive
/// while the caller uses the pointer.
pub unsafe fn refc_payload(t: Eterm) -> TermResult<*mut Eterm> {
    // SAFETY: the caller guarantees `t` is a live boxed term; `is_refc_binary`
    // dereferences only its header and, for an `ErlSubBits`, its live `orig` slot.
    if !unsafe { is_refc_binary(t) } {
        return Err(TermError::BadBitstring);
    }
    // SAFETY: `is_refc_binary` just proved `t` is a live 5-word `ErlSubBits`, so word 4
    // (`orig`) is initialised and is a live `BinRef`.
    let orig = unsafe { *boxed_val(t).add(SUB_BITS_ORIG_WORD) };
    // SAFETY: `orig` is a live `BinRef`, so [`bin_ref_parts`] reads two initialised words.
    let (val, _next) = unsafe { bin_ref_parts(orig) };
    if val.is_null() {
        return Err(TermError::BadBitstring);
    }
    Ok(val)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstring::{bit_at, bitstring_parts, write_sub_bits};
    use core::mem::size_of;
    use crate::boxed::{arityval, is_bitstring, make_header};
    use crate::heap::Heap;
    use crate::immediate::{make_small, NIL};
    use crate::tag::{BIN_REF_SUBTAG, SUB_BITS_SUBTAG};

    fn new_heap() -> Heap {
        Heap::new(64)
    }

    #[test]
    fn sizes_and_limits_match_erl_bits_h() {
        assert_eq!(bin_ref_size_words(), 3);
        assert_eq!(sub_bits_size_words(), 5);
        assert_eq!(ERL_REFC_BITS_SIZE, 8);
        assert_eq!(ERL_ONHEAP_BINARY_LIMIT, 64);
        assert_eq!(ERL_ONHEAP_BITS_LIMIT, 512);
        // `HEADER_BIN_REF = _make_header(2, 0x28)`, `HEADER_SUB_BITS = _make_header(4, 0x24)`.
        assert_eq!(HEADER_BIN_REF.raw(), make_header(2, BIN_REF_SUBTAG).raw());
        assert_eq!(HEADER_BIN_REF.raw(), 0xA8);
        assert_eq!(HEADER_SUB_BITS.raw(), make_header(4, SUB_BITS_SUBTAG).raw());
        assert_eq!(HEADER_SUB_BITS.raw(), 0x124);
        assert_eq!(arityval(HEADER_BIN_REF), 2);
        assert_eq!(arityval(HEADER_SUB_BITS), 4);
    }

    #[test]
    fn bin_ref_block_layout_is_pinned() {
        let mut heap = new_heap();
        // SAFETY: 3 words reserved for the `BinRef`.
        let term = unsafe {
            let hp = heap.alloc_words(bin_ref_size_words()).unwrap();
            write_bin_ref(hp, 0x1000 as *mut Eterm, core::ptr::null_mut())
        };
        // SAFETY: `term` is the `BinRef` just written.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, HEADER_BIN_REF);
            assert_eq!((*p.add(1)).raw(), 0x1000);
            assert_eq!((*p.add(2)).raw(), 0);

            let (val, next) = bin_ref_parts(term);
            assert_eq!(val as usize, 0x1000);
            assert!(next.is_null());
        }
    }

    #[test]
    fn combined_block_layout_is_pinned() {
        #[repr(align(8))]
        struct Storage([u8; 8]);
        let storage = Storage([0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0]);

        let mut heap = new_heap();
        // SAFETY: `ERL_REFC_BITS_SIZE` (8) words reserved for the pair.
        let root = unsafe {
            let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
            write_refc_binary(
                hp,
                0x2000 as *mut Eterm,
                core::ptr::null_mut(),
                storage.0.as_ptr(),
                0,
                32,
            )
        };
        assert_eq!(heap.used(), 8);

        // SAFETY: `root` is the pair just written; all 8 words are live.
        unsafe {
            let br = boxed_val(root).sub(bin_ref_size_words());
            let sb = boxed_val(root);
            // BinRef at the lower address...
            assert_eq!(*br, HEADER_BIN_REF);
            assert_eq!((*br.add(1)).raw(), 0x2000);
            assert_eq!((*br.add(2)).raw(), 0);
            // ... and the ErlSubBits at `hp + ERL_BIN_REF_SIZE`, holding the root.
            assert_eq!(sb, br.add(bin_ref_size_words()));
            assert_eq!(*sb, HEADER_SUB_BITS);
            assert_eq!((*sb.add(1)).raw(), storage.0.as_ptr() as usize);
            assert_eq!((*sb.add(2)).raw(), 0);
            assert_eq!((*sb.add(3)).raw(), 32);
            assert_eq!(
                *sb.add(4),
                make_boxed(br),
                "orig is a boxed pointer to the BinRef"
            );
        }

        assert!(unsafe { is_refc_binary(root) });
        assert_eq!(unsafe { refc_offset(root) }, 0);
        assert_eq!(unsafe { refc_size(root) }, 32);
        assert_eq!(unsafe { refc_bytes(root) }, &storage.0[..4]);
        // SAFETY: `root` is a live bitstring with byte-aligned size.
        assert!(unsafe { is_bitstring(root) });
        assert_eq!(unsafe { bitstring_parts(root) }.2, 32);
        assert!(unsafe { bit_at(root, 0) }); // 0xDE = 0b1101_1110
        assert!(!unsafe { bit_at(root, 2) });
    }

    #[test]
    fn windowed_payload_reads_through_the_bit_offset() {
        #[repr(align(8))]
        struct Storage([u8; 4]);
        let storage = Storage([0b1010_1010, 0b1100_0000, 0, 0]);

        let mut heap = new_heap();
        // SAFETY: 8 words reserved; a 9-bit window from bit 3.
        let root = unsafe {
            let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
            write_refc_binary(
                hp,
                0x3000 as *mut Eterm,
                core::ptr::null_mut(),
                storage.0.as_ptr(),
                3,
                9,
            )
        };
        assert_eq!(unsafe { refc_offset(root) }, 3);
        assert_eq!(unsafe { refc_size(root) }, 9);
        // Bits 3..12 are `0101_0110_0`; `refc_bytes` spans bytes 0..2 (2 bytes).
        assert_eq!(unsafe { refc_bytes(root) }, &storage.0[..2]);
        let bits: Vec<bool> = (0..9).map(|i| unsafe { bit_at(root, i) }).collect();
        assert_eq!(
            bits,
            [false, true, false, true, false, true, true, false, false]
        );
    }

    #[test]
    fn is_refc_binary_rejects_everything_else() {
        let mut heap = new_heap();
        assert!(!unsafe { is_refc_binary(make_small(1)) });
        assert!(!unsafe { is_refc_binary(NIL) });

        // An on-heap bitstring is not a refc binary.
        // SAFETY: 3 words for a 2-byte heap binary.
        let heap_bin = unsafe {
            let hp = heap
                .alloc_words(crate::bitstring::heap_bits_size_words(16))
                .unwrap();
            crate::bitstring::write_heap_bits(hp, &[1, 2], 16).unwrap()
        };
        assert!(!unsafe { is_refc_binary(heap_bin) });

        // An `ErlSubBits` whose `orig` is not a `BinRef` is a match context, not a
        // refc binary.
        // SAFETY: 5 words for the `ErlSubBits`; `orig = NIL` is a live term.
        let sub = unsafe {
            let hp = heap.alloc_words(sub_bits_size_words()).unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(0x4000),
                Eterm::from_raw(0),
                Eterm::from_raw(8),
                NIL,
            )
        };
        assert!(!unsafe { is_refc_binary(sub) });

        // A plain tuple is not a bitstring at all.
        let tup = heap.tuple(&[make_small(1)]).unwrap();
        assert!(!unsafe { is_refc_binary(tup) });
    }
    // -----------------------------------------------------------------------
    // Sub-binary windows (`write_sub_binary` / `sub_binary_words`)
    // -----------------------------------------------------------------------
    // The public-surface matrix lives in `tests/refc_window.rs`; these pin the unit-level
    // properties the integration file cannot see.

    /// A word-aligned payload buffer ([`write_refc_binary`]'s base-alignment contract).
    #[repr(align(8))]
    struct Payload<const N: usize>([u8; N]);

    /// A deterministic payload whose bytes never repeat a neighbour (so an unshifted or
    /// off-by-one-byte copy cannot pass by luck).
    fn payload<const N: usize>() -> Payload<N> {
        let mut bytes = [0u8; N];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(0x5B);
        }
        Payload(bytes)
    }

    /// Builds a refc binary over `payload` at bits `offset..offset + size`.
    fn refc_parent(heap: &mut Heap, payload: &[u8], offset: usize, size: usize) -> Eterm {
        // SAFETY: 8 words reserved; `payload` outlives every use of the term here and its
        // pointer is word aligned (the `Payload` attribute).
        unsafe {
            let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
            write_refc_binary(
                hp,
                payload.as_ptr() as *mut Eterm,
                core::ptr::null_mut(),
                payload.as_ptr(),
                offset,
                size,
            )
        }
    }

    /// The first `n` bits of a bitstring term.
    fn bits(t: Eterm, n: usize) -> Vec<bool> {
        (0..n).map(|i| unsafe { bit_at(t, i) }).collect()
    }

    #[test]
    fn heap_arm_copies_a_window_and_zeroes_its_tail() {
        let payload = payload::<64>();
        let mut heap = Heap::new(4096);
        let parent = refc_parent(&mut heap, &payload.0, 0, 64 * 8);
        let parent_bits = bits(parent, 64 * 8);
        // Byte-aligned and non-byte-aligned offsets, odd and even sizes, all at or below
        // `ERL_ONHEAP_BITS_LIMIT` — so all the heap arm.
        for (off, size) in [(0usize, 0usize), (0, 512), (3, 21), (7, 505), (37, 129), (509, 3)]
        {
            let used = heap.used();
            // SAFETY: `parent` is a live refc binary; the reservation is the reported count.
            let words = unsafe { sub_binary_words(parent, size) }.unwrap();
            assert_eq!(words, crate::bitstring::heap_bits_size_words(size));
            let hp = unsafe { heap.alloc_words(words) }.unwrap();
            let child = unsafe { write_sub_binary(hp, parent, off, size) }.unwrap();
            assert_eq!(heap.used() - used, words, "writer used what it reserved");
            // An `ErlHeapBits` header (never a pair) whose arity prices exactly its words.
            assert_eq!(
                unsafe { *boxed_val(child) },
                make_header(words - 1, HEAP_BITS_SUBTAG)
            );
            assert!(!unsafe { is_refc_binary(child) });
            assert_eq!(bits(child, size), parent_bits[off..off + size].to_vec());
            // The bits past the window are zero: the last byte's low bits and the rest of
            // the block's payload bytes.
            let (base, _, _) = unsafe { bitstring_parts(child) };
            let n_bytes = crate::bitstring::bit_count_to_bytes(size);
            for i in n_bytes..(words - 2) * 8 {
                // SAFETY: the block's data words are `(words - 2) * 8` bytes wide.
                assert_eq!(unsafe { *base.add(i) }, 0, "padding byte {i}");
            }
            if size % 8 != 0 {
                // SAFETY: `base` covers the `n_bytes` payload bytes just written.
                assert_eq!(
                    unsafe { *base.add(n_bytes - 1) } & (0xFFu8 >> (size % 8)),
                    0
                );
            }
        }
    }

    #[test]
    fn refc_arm_shares_the_parent_payload() {
        let payload = payload::<128>();
        let mut heap = Heap::new(512);
        // A window that does not start at the payload base, so `start` is inherited.
        let parent = refc_parent(&mut heap, &payload.0, 40, 1000);
        let (off, size) = (400usize, 600usize);
        // SAFETY: `parent` is a live refc binary.
        let words = unsafe { sub_binary_words(parent, size) }.unwrap();
        assert_eq!(words, ERL_REFC_BITS_SIZE);
        let used = heap.used();
        let hp = unsafe { heap.alloc_words(words) }.unwrap();
        let child = unsafe { write_sub_binary(hp, parent, off, size) }.unwrap();
        assert_eq!(heap.used() - used, ERL_REFC_BITS_SIZE);

        assert!(unsafe { is_refc_binary(child) });
        assert_eq!(bits(child, size), bits(parent, 1000)[off..].to_vec());
        // `start` is the parent's start + the offset (the C's `erts_make_sub_bitstring`
        // convention) and `end` is `start + size`.
        assert_eq!(unsafe { refc_offset(child) }, 40 + off);
        assert_eq!(unsafe { refc_size(child) }, size);
        // The child names the very same payload, so the referenced size stays reachable.
        let (child_val, child_next) = unsafe { bin_ref_parts(sub_bits_parts(child).3) };
        let (parent_val, parent_next) = unsafe { bin_ref_parts(sub_bits_parts(parent).3) };
        assert_eq!(child_val, payload.0.as_ptr() as *mut Eterm);
        assert_eq!((child_val, child_next), (parent_val, parent_next));
        assert_eq!(unsafe { refc_payload(child) }.unwrap(), child_val);
    }

    #[test]
    fn threshold_boundary_picks_the_arm_on_the_window_size() {
        let payload = payload::<128>();
        let mut heap = Heap::new(512);
        let parent = refc_parent(&mut heap, &payload.0, 0, 1024);
        // SAFETY: `parent` is a live refc binary.
        let heap_words = unsafe { sub_binary_words(parent, ERL_ONHEAP_BITS_LIMIT) }.unwrap();
        // A 512-bit `ErlHeapBits`: 2 head words plus 64 / `size_of::<Eterm>()` data words.
        assert_eq!(MAX_HEAP_WINDOW_BYTES, 64);
        assert_eq!(heap_words, 2 + MAX_HEAP_WINDOW_BYTES / size_of::<Eterm>());
        let hp = unsafe { heap.alloc_words(heap_words) }.unwrap();
        let at_limit = unsafe { write_sub_binary(hp, parent, 0, ERL_ONHEAP_BITS_LIMIT) }.unwrap();
        assert!(!unsafe { is_refc_binary(at_limit) });

        // ... and one bit more is the refc arm (pinned end to end by
        // `tests/refc_window.rs`).
        assert_eq!(
            unsafe { sub_binary_words(parent, ERL_ONHEAP_BITS_LIMIT + 1) },
            Ok(ERL_REFC_BITS_SIZE)
        );
    }

    #[test]
    fn windows_refuse_malformed_parents_and_out_of_range_spans() {
        let payload = payload::<128>();
        let mut heap = Heap::new(256);
        let parent = refc_parent(&mut heap, &payload.0, 0, 1024);
        let tup = heap.tuple(&[make_small(1)]).unwrap();
        // A non-bitstring parent.
        for bad in [make_small(1), NIL, tup] {
            assert_eq!(
                unsafe { write_sub_binary(heap.alloc_words(10).unwrap(), bad, 0, 8) },
                Err(TermError::BadBitstring)
            );
            assert_eq!(
                unsafe { sub_binary_words(bad, 8) },
                Err(TermError::BadBitstring)
            );
        }
        // A heap parent whose size word contradicts its header arity: a 3-word block
        // claiming 512 bits (which needs 10 words).
        // SAFETY: 3 words reserved, then only the size word is rewritten.
        let lying = unsafe {
            let hp = heap.alloc_words(crate::bitstring::heap_bits_size_words(16)).unwrap();
            let t = crate::bitstring::write_heap_bits(hp, &[1, 2], 16).unwrap();
            *hp.add(1) = Eterm::from_raw(ERL_ONHEAP_BITS_LIMIT);
            t
        };
        assert_eq!(
            unsafe { write_sub_binary(heap.alloc_words(10).unwrap(), lying, 0, 8) },
            Err(TermError::BadBitstring)
        );
        // Spans past the end of the parent, plus two that would wrap `usize`.
        for (off, size) in [(1024, 1), (1000, 100), (usize::MAX, 1), (usize::MAX - 7, 8)] {
            // SAFETY: a refc-arm reservation, never written on the refusal path.
            let hp = unsafe { heap.alloc_words(ERL_REFC_BITS_SIZE) }.unwrap();
            assert_eq!(
                unsafe { write_sub_binary(hp, parent, off, size) },
                Err(TermError::BadBitstring),
                "{off}+{size} past a 1024-bit parent"
            );
        }
        // A match-context parent (`ERL_SUB_BITS_FLAG_MUTABLE`, `orig` not a `BinRef`) cannot
        // be shared above the limit, but the heap arm copies its bits.
        // SAFETY: 5 words for the `ErlSubBits`; `base_flags` packs a live data base.
        let ctx = unsafe {
            let hp = heap.alloc_words(sub_bits_size_words()).unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(
                    payload.0.as_ptr() as usize | crate::bitstring::ERL_SUB_BITS_FLAG_MUTABLE,
                ),
                Eterm::from_raw(0),
                Eterm::from_raw(1024),
                NIL,
            )
        };
        assert_eq!(
            unsafe { sub_binary_words(ctx, ERL_ONHEAP_BITS_LIMIT + 1) },
            Err(TermError::BadBitstring)
        );
        // SAFETY: 10 words is the 512-bit heap-arm reservation.
        let copied = unsafe { write_sub_binary(heap.alloc_words(10).unwrap(), ctx, 8, 512) }.unwrap();
        assert!(!unsafe { is_refc_binary(copied) });
        assert_eq!(bits(copied, 512), bits(ctx, 520)[8..].to_vec());
        // An `ErlHeapBits` parent cannot be shared above the limit either.
        assert_eq!(
            unsafe { sub_binary_words(copied, ERL_ONHEAP_BITS_LIMIT + 1) },
            Err(TermError::BadBitstring)
        );
    }
}
