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

//! Bit-level term payloads: the `ErlHeapBits` / `ErlSubBits` block writers and
//! readers, plus the crate's public bit- and float-payload helpers.
//!
//! This module owns the *third* interior layout `beam_term` is responsible for: a
//! **bitstring**. A bitstring is a boxed term ([`crate::boxed`]) carrying one of two
//! headers:
//!
//! * an **on-heap bitstring** (`HEAP_BITS_SUBTAG`, the C `ErlHeapBits`) —
//!   `[thing_word, n_bits, data…]`, written by [`write_heap_bits`]; the tagged root
//!   is `make_boxed(hp)`;
//! * a **sub-bitstring** (`SUB_BITS_SUBTAG`, the C `ErlSubBits`) —
//!   `[HEADER_SUB_BITS, base_flags, start, end, orig]`, written by
//!   [`write_sub_bits`]; it is a *window* into other storage (an off-heap
//!   refcounted binary — see [`crate::refc`] — or, for a match context, an on-heap
//!   bitstring).
//!
//! Ground truth: `erts/emulator/beam/erl_bits.h:60-176` (the `ErlSubBits` /
//! `ErlHeapBits` definitions, `NBYTES`, `heap_bits_size`) and `erl_bits.c:490-519`
//! (`erl_sub_bits_init`, which fixes `start`/`end`/`base_flags`).
//!
//! # Bit order
//!
//! Bits are stored and read **most-significant-bit first inside each byte**, with no
//! shifting: byte 0 holds bits 0..8 of the string from its high bit down, so
//! `<<1:3>>` is the single byte `0b0010_0000` with `n_bits = 3`. The payload is a
//! plain byte copy into the first data word (byte 0 at the lowest address of that
//! word); the unused trailing bits of the last byte are zero-filled, so the term
//! carries no garbage from the caller's buffer and a round-trip is canonical. This
//! reproduces `beam_codec`'s oracle-verified `make_bitstring` exactly.
//!
//! # Heap-agnostic contract
//!
//! `beam_term` is a dependency *sink* and cannot allocate: the caller reserves the
//! words (production: `beam_alloc`; tests: [`crate::heap::Heap`]), sizes them with
//! [`heap_bits_size_words`] (or [`crate::refc::sub_bits_size_words`]), passes the
//! `hp: *mut Eterm` base and receives back the tagged root word.
//!
//! # The crate's single public float reader
//!
//! [`float_bits`] / [`float_value`] — the boxed-float payload readers — are defined
//! here as the crate's *only* public definition of the float layout;
//! [`crate::compare`], [`crate::hashing`] and [`crate::thing`] all forward to them
//! (the layout has exactly one owner; `crate::thing` re-exports them so
//! `beam_term::thing`'s accessor surface is unchanged).
//!
//! # Checked readers (shape validation)
//!
//! [`sub_bits_parts`] / [`bitstring_parts`] / [`bit_at`] are the *unchecked* readers: an
//! `ErlSubBits`'s `base_flags` word becomes a `*const u8` with no test at all, so a
//! corrupt or stale payload word turns into an arbitrary-address read. The `try_*`
//! siblings — [`try_sub_bits_parts`] / [`try_bitstring_parts`] / [`try_bit_at`] — run
//! the same reads *after* a shape check and answer a typed error instead, and they are
//! what a copier, a GC pass or a message encoder should call on a term whose provenance
//! it does not own.
//!
//! The invariant the check enforces is exactly this:
//!
//! * a word that is not a term ([`crate::tag::is_term_word`] false — `THE_NON_VALUE`, a
//!   bare `HEADER` word) and a non-boxed term (an immediate or a cons cell) is
//!   [`TermError::NotATerm`];
//! * an `ErlSubBits` (`HEADER_SUB_BITS`) must have `start <= end`, a **non-null** masked
//!   base pointer, and an `orig` that is either a boxed `BinRef` with a non-null `val` or
//!   a boxed **on-heap** bitstring whose header arity agrees with its own bit-size word
//!   (the C's `ASSERT(erl_sub_bits_is_match_context(_sb) || _br)`,
//!   `erts/emulator/beam/erl_bits.h:385`, as a typed refusal);
//! * an on-heap bitstring (`HEAP_BITS_SUBTAG`) must satisfy
//!   `arityval(header) + 1 == heap_bits_size_words(size_word)` — its own arity must agree
//!   with its own bit count, which is what stops a word misclassified as an `ErlHeapBits`
//!   from becoming a base pointer into arbitrary storage;
//! * every other boxed header (tuple, map, float, bignum, fun, record, `BinRef`, …) is
//!   [`TermError::BadBitstring`], and [`try_bit_at`] additionally refuses `i >= size_bits`.
//!
//! **The honest limit is provenance.** The check covers *shape* only: it can prove that
//! a readable block never yields a wild pointer or a bogus bit, but a block that has
//! been freed, recycled or moved is word-for-word indistinguishable from a live one —
//! this crate has no region registry and none of its accessors validates provenance
//! (`beam_term/CONTEXT.md`, *Notes for Agents*). That is why the `try_*` forms stay
//! `unsafe`: their `# Safety` is narrowed to "the boxed pointer must be readable".
//!
//! # `unsafe` contract
//!
//! Every reader is `unsafe`: an `Eterm` being a live, well-formed term of the
//! expected kind is a caller invariant, and reading a raw heap word has no safe
//! form. Each documents, under `# Safety`, that invariant and the lifetime of any
//! borrowed data. A writer that can detect a bad *argument* (a byte count too small
//! for the bit count) returns a typed [`TermError`] instead of writing a corrupt
//! term (workspace `CONTEXT.md` §3).

use core::mem::size_of;

use crate::boxed::{
    arityval, boxed_val, header_subtag, is_boxed_term, make_boxed, make_header, ERL_HEAP_BITS_SIZE,
    HEADER_BIN_REF, HEADER_SUB_BITS,
};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::tag::{is_term_word, HEAP_BITS_SUBTAG};

/// Number of bytes in one term word (`size_of::<Eterm>()`, 8 on the frozen
/// `ARCH_64` layout).
const WORD_BYTES: usize = size_of::<Eterm>();

// The frozen bitstring layout is the 64-bit `ARCH_64` ABI of the C ground truth
// (`heap_bits_size` divides the byte count by `sizeof(Eterm)`).
const _: () = assert!(WORD_BYTES == 8, "the frozen term layout assumes ARCH_64");

/// Word index `1` of an `ErlSubBits`: the `base_flags` word (the packed data base
/// pointer + the low-bit flags).
pub const SUB_BITS_BASE_WORD: usize = 1;
/// Word index `2` of an `ErlSubBits`: the `start` bit offset.
pub const SUB_BITS_START_WORD: usize = 2;
/// Word index `3` of an `ErlSubBits`: the `end` bit offset.
pub const SUB_BITS_END_WORD: usize = 3;
/// Word index `4` of an `ErlSubBits`: the `orig` term — the *only* word of the block
/// that is itself a term reference (a boxed `BinRef` for a refc binary, or an on-heap
/// bitstring for a match context). This is the slot `crate::layout::for_each_child_slot`
/// reports to a copier.
pub const SUB_BITS_ORIG_WORD: usize = 4;

/// `ERL_SUB_BITS_FLAG_MASK` (`erl_bits.h:95`): the low two bits of an
/// `ErlSubBits::base_flags` word hold the mutable/volatile flags, so the base
/// pointer must be masked with `!ERL_SUB_BITS_FLAG_MASK` before use
/// (`erl_sub_bits_get_base`, `erl_bits.h:124-125`).
pub const ERL_SUB_BITS_FLAG_MASK: usize = 0x3;
/// `ERL_SUB_BITS_FLAG_MUTABLE` (`erl_bits.h:91`): the `ErlSubBits` itself may be
/// modified in place (match contexts, private-append binaries).
pub const ERL_SUB_BITS_FLAG_MUTABLE: usize = 1 << 0;
/// `ERL_SUB_BITS_FLAG_VOLATILE` (`erl_bits.h:94`): the underlying data may move
/// (a writable binary), so the stored base pointer must be re-followed.
pub const ERL_SUB_BITS_FLAG_VOLATILE: usize = 1 << 1;

/// The mask to apply to an `ErlSubBits::base_flags` word to recover the data base
/// pointer. Alias of [`ERL_SUB_BITS_FLAG_MASK`] under the name the bitstring shims
/// use; the two are the same value by construction.
pub const BITSTRING_POINTER_FLAG_MASK: usize = ERL_SUB_BITS_FLAG_MASK;

/// `ceil(n / d)`, written with divisions only so it can never overflow (the
/// `(n + d - 1) / d` form wraps for a corrupt/huge `n`).
#[inline]
const fn ceil_div(n: usize, d: usize) -> usize {
    // `q * d <= n`, so the multiply cannot overflow — unlike `(n + d - 1) / d`.
    let q = n / d;
    if q * d == n {
        q
    } else {
        q + 1
    }
}

/// `NBYTES(n_bits)` (`erl_bits.h:25`): the whole bytes a bit count occupies, i.e.
/// `ceil(n_bits / 8)`. Overflow-free for every bit count.
#[inline]
pub const fn bit_count_to_bytes(n_bits: usize) -> usize {
    ceil_div(n_bits, 8)
}

/// `heap_bits_size(num_bits)` (`erl_bits.h:156-164`): the word count of the
/// `ErlHeapBits` block holding `n_bits` bits —
/// `2 + ceil(ceil(n_bits / 8) / sizeof(Eterm))` (`heap_bin_size__(NBYTES(n))`,
/// where `2` is the `[thing_word, size]` head).
///
/// Overflow-free for every bit count, so a corrupt `size` word is rejected by the
/// header-arity check in [`bitstring_parts`]-style callers rather than wrapping.
#[inline]
pub const fn heap_bits_size_words(n_bits: usize) -> usize {
    ERL_HEAP_BITS_SIZE + ceil_div(bit_count_to_bytes(n_bits), WORD_BYTES)
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// Writes an on-heap bitstring (`ErlHeapBits`) at `hp` and returns the tagged root.
///
/// Layout pinned to `erl_bits.h:150-164`: `[thing_word, n_bits, data…]`, where
/// `thing_word = make_header(heap_bits_size(n_bits) - 1, HEAP_BITS_SUBTAG)`, the
/// size word holds the **bit** count, and the root is `make_bitstring(hp)` ==
/// `make_boxed(hp)`. The `n_bytes = ceil(n_bits / 8)` payload bytes are copied in
/// with byte 0 at the lowest address of the first data word, the trailing padding
/// bytes of the last data word are zeroed, and the unused trailing bits of the last
/// byte are cleared (so the term never carries the caller's garbage).
///
/// A `bytes` slice shorter than `ceil(n_bits / 8)` yields
/// [`TermError::BadBitstring`] and writes nothing; extra bytes past
/// `ceil(n_bits / 8)` are ignored (callers should pass exactly that many).
///
/// # Safety
///
/// `hp` must point at `heap_bits_size_words(n_bits)` exclusively-owned, writable
/// words, which stay live (and unmoved) for as long as the returned term is used.
pub unsafe fn write_heap_bits(hp: *mut Eterm, bytes: &[u8], n_bits: usize) -> TermResult<Eterm> {
    let n_bytes = bit_count_to_bytes(n_bits);
    if bytes.len() < n_bytes {
        return Err(TermError::BadBitstring);
    }
    let words = heap_bits_size_words(n_bits);
    // SAFETY: the caller reserved `words = 2 + ceil(n_bytes / WORD_BYTES)` words: word
    // 0 receives the header, word 1 the bit size and words `2..` the data — exactly the
    // `ErlHeapBits` layout (`erl_bits.h:150-164`), so the header arity is `words - 1`
    // and the root is `make_bitstring(hp) == make_boxed(hp)`. The data words are zeroed
    // first (initialising the trailing padding bytes), then the `n_bytes` payload bytes
    // are copied in.
    unsafe {
        *hp = make_header(words - 1, HEAP_BITS_SUBTAG);
        *hp.add(1) = Eterm::from_raw(n_bits);
        let data = hp.add(ERL_HEAP_BITS_SIZE).cast::<u8>();
        let data_bytes = (words - ERL_HEAP_BITS_SIZE) * WORD_BYTES;
        core::ptr::write_bytes(data, 0, data_bytes);
        if n_bytes > 0 {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, n_bytes);
        }
        let tail = n_bits % 8;
        if tail != 0 {
            // Clear the unused low bits of the last payload byte (MSB-first order).
            *data.add(n_bytes - 1) &= 0xFFu8 << (8 - tail);
        }
    }
    Ok(make_boxed(hp))
}

/// Writes a **binary** (a bitstring of `bytes.len() * 8` bits) at `hp`.
///
/// A thin wrapper over [`write_heap_bits`], matching the C `HEAP_BITSTRING` /
/// `make_binary` path. A byte count whose bit count overflows `usize` yields
/// [`TermError::BadBitstring`] rather than wrapping.
///
/// # Safety
///
/// As [`write_heap_bits`], with `n_bits = bytes.len() * 8`.
pub unsafe fn write_binary(hp: *mut Eterm, bytes: &[u8]) -> TermResult<Eterm> {
    match bytes.len().checked_mul(8) {
        // SAFETY: the caller reserved `heap_bits_size_words(bytes.len() * 8)` words.
        Some(n_bits) => unsafe { write_heap_bits(hp, bytes, n_bits) },
        // A byte count of more than `usize::MAX / 8` bits is unrepresentable.
        None => Err(TermError::BadBitstring),
    }
}

/// Writes an `ErlSubBits` block at `hp` and returns the tagged root.
///
/// Layout pinned to `erl_bits.h:60-82,130`:
/// `[HEADER_SUB_BITS, base_flags, start, end, orig]` (5 words), with the root
/// `make_bitstring(hp)` == `make_boxed(hp)`. `base_flags` packs the data base
/// pointer with the low [`ERL_SUB_BITS_FLAG_MASK`] bits used as flags (see
/// [`ERL_SUB_BITS_FLAG_MUTABLE`] / [`ERL_SUB_BITS_FLAG_VOLATILE`]); `start`/`end`
/// are **bit** offsets into that base and the size is `end - start`; `orig` is the
/// originating term (a `BinRef` for a refc binary, or an on-heap bitstring for a
/// match context).
///
/// # Safety
///
/// `hp` must point at `crate::refc::sub_bits_size_words()` (5) exclusively-owned,
/// writable words, which stay live (and unmoved) for as long as the returned term is
/// used. `base_flags` must point at storage covering bits `start..end`.
pub unsafe fn write_sub_bits(
    hp: *mut Eterm,
    base_flags: Eterm,
    start: Eterm,
    end: Eterm,
    orig: Eterm,
) -> Eterm {
    // SAFETY: the caller reserved the 5-word `ErlSubBits` block at `hp`; every word is
    // written before the pointer escapes.
    unsafe {
        *hp = HEADER_SUB_BITS;
        *hp.add(SUB_BITS_BASE_WORD) = base_flags;
        *hp.add(SUB_BITS_START_WORD) = start;
        *hp.add(SUB_BITS_END_WORD) = end;
        *hp.add(SUB_BITS_ORIG_WORD) = orig;
    }
    make_boxed(hp)
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

/// Splits an `ErlSubBits` into `(base_flags, start, end, orig)`.
///
/// The exact inverse of [`write_sub_bits`] (`erl_bits.h:60-82`). `start`/`end` are
/// **bit** offsets; the size is `end - start`. `base_flags` still carries the flag
/// bits — mask with [`ERL_SUB_BITS_FLAG_MASK`] before using it as a pointer.
///
/// # Safety
///
/// `t` must be a live `ErlSubBits` term (a bitstring whose header is
/// `HEADER_SUB_BITS`), kept alive by the caller.
///
/// **The block's shape is not validated here.** A corrupt header, or an `orig` slot
/// that is not a `BinRef`/on-heap bitstring, is read as if it were well formed. Use
/// [`try_sub_bits_parts`] when the term's provenance is in doubt: it refuses every
/// mis-shaped block with a typed error, though — like this reader — it cannot tell a
/// freed block from a live one (module docs, *The honest limit is provenance*).
pub unsafe fn sub_bits_parts(t: Eterm) -> (Eterm, Eterm, Eterm, Eterm) {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees `t` is a live `ErlSubBits`, so words 1..4 are
    // initialised by [`write_sub_bits`].
    unsafe {
        (
            *p.add(SUB_BITS_BASE_WORD),
            *p.add(SUB_BITS_START_WORD),
            *p.add(SUB_BITS_END_WORD),
            *p.add(SUB_BITS_ORIG_WORD),
        )
    }
}

/// Decomposes a bitstring into `(data base byte pointer, start bit offset, bit size)`.
///
/// Both representations are handled (`erl_bits.h:53-56`, `ERTS_GET_BITSTRING_REF`):
///
/// * `ErlHeapBits` — base `= boxed_val(t) + 2`, offset `0`, size = the `size` word;
/// * `ErlSubBits` — base `= base_flags & !ERL_SUB_BITS_FLAG_MASK`, offset `= start`,
///   size `= end - start`.
///
/// The base points at the **byte** containing bit 0 of the underlying storage, so
/// bit `i` of the bitstring is bit `(offset + i) % 8` (MSB-first) of
/// `base[(offset + i) / 8]`.
///
/// A `size` word that is not a bit count makes this a garbage window: the arithmetic
/// is **panic-free** (`end < start` saturates to `0`, so a corrupt sub-bits reports an
/// empty window instead of underflowing), but the base pointer and the size are
/// otherwise reported verbatim. Nothing here is a substitute for
/// [`try_bitstring_parts`], which refuses a mis-shaped block outright.
///
/// # Safety
///
/// `t` must be a live bitstring (on-heap or sub) term. For a sub-bitstring the base
/// pointer must still be valid (not a volatile writable binary that has moved). The
/// returned base pointer borrows the term's storage, so the caller must keep the
/// owning heap (or the underlying binary) alive and unmoved while it is used.
///
/// **The block's shape is not validated** — only [`try_bitstring_parts`] does that.
pub unsafe fn bitstring_parts(t: Eterm) -> (*const u8, usize, usize) {
    let p = boxed_val(t);
    // SAFETY: `t` is a live boxed bitstring, so its header word is initialised.
    let hdr = unsafe { *p };
    if hdr == HEADER_SUB_BITS {
        // SAFETY: the header fixes the block at the 5-word `ErlSubBits` layout, so
        // words 1..3 are live (`erl_bits.h:60-82`).
        let base_flags = unsafe { (*p.add(SUB_BITS_BASE_WORD)).raw() };
        let start = unsafe { (*p.add(SUB_BITS_START_WORD)).raw() };
        let end = unsafe { (*p.add(SUB_BITS_END_WORD)).raw() };
        (
            (base_flags & !ERL_SUB_BITS_FLAG_MASK) as *const u8,
            start,
            // Saturating, not `-`: a corrupt `end < start` must not panic in a debug
            // profile on runtime data (it reports an empty window instead).
            end.saturating_sub(start),
        )
    } else {
        // SAFETY: an `ErlHeapBits` is `[thing_word, size, data…]`, so word 1 is the bit
        // size and word 2 the first payload byte (`erl_bits.h:150-164`).
        let size = unsafe { (*p.add(1)).raw() };
        (unsafe { p.add(ERL_HEAP_BITS_SIZE) }.cast::<u8>(), 0, size)
    }
}

/// Reads bit `i` (0-based, most-significant bit of the first byte first) of a
/// bitstring term.
///
/// Reproduces the bit test of `erts_cmp_bits` / `erts_bs_get_*` (`erl_bits.c`):
/// `(base[(offset + i) / 8] >> (7 - ((offset + i) % 8))) & 1`.
///
/// # Safety
///
/// `t` must be a live bitstring term and `i` must be less than its bit size. For a
/// sub-bitstring the base storage must still be valid.
///
/// An `i` that is out of range, or an `offset + i` that overflows, answers `false`
/// instead of reading out of bounds — a defined answer, never a panic. That is *not* a
/// substitute for [`try_bit_at`], which validates the block's shape and refuses an
/// out-of-range `i` with a typed error.
pub unsafe fn bit_at(t: Eterm, i: usize) -> bool {
    // SAFETY: `t` is a live bitstring; bit `i` is within its size by the caller.
    let (base, offset, _size) = unsafe { bitstring_parts(t) };
    match offset.checked_add(i) {
        // SAFETY: the caller guarantees bit `offset + i` is inside `base`'s storage.
        Some(bit) => unsafe { raw_bit(base, bit) != 0 },
        // A corrupt `start` word: report "not set" rather than addressing past the end.
        None => false,
    }
}

/// Read bit `bit` (0 = most significant bit of the base byte) of a bitstring base.
///
/// The one bit-level reader of the crate: [`bit_at`] applies it to a term's base and
/// offset, and the comparison arm ([`crate::compare`]'s `erts_cmp_bits` port) applies
/// it to a base/offset pair it already resolved.
///
/// # Safety
///
/// `base` must point at bit data covering bit `bit`.
#[inline]
pub(crate) unsafe fn raw_bit(base: *const u8, bit: usize) -> u8 {
    // SAFETY: the caller guarantees the byte is in range.
    unsafe { (*base.add(bit >> 3) >> (7 - (bit & 7))) & 1 }
}

// ---------------------------------------------------------------------------
// Checked readers (shape validation)
// ---------------------------------------------------------------------------

/// The validated shape of a bitstring: where its bits live and how many there are.
///
/// Produced only by [`checked_bits`], whose arms are the only constructors, so a
/// `CheckedBits` value *is* the proof that the shape rules of the module docs hold.
enum CheckedBits {
    /// `[HEADER_SUB_BITS, base_flags, start, end, orig]` with `start <= end`, a non-null
    /// masked base pointer and a recognised `orig`.
    Sub {
        /// `base_flags & !ERL_SUB_BITS_FLAG_MASK` — never null.
        base: *const u8,
        /// The first bit of the window, inside the base.
        start: usize,
        /// One past the last bit of the window; `start <= end`.
        end: usize,
    },
    /// `[thing_word, n_bits, data…]` whose header arity agrees with `n_bits`.
    Heap {
        /// The first payload byte (word `ERL_HEAP_BITS_SIZE` of the block).
        base: *const u8,
        /// The bit count — the block's own `size` word.
        size: usize,
    },
}

/// Whether `orig` is one of the two origins an `ErlSubBits` may name.
///
/// That is the C's assertion `ASSERT(erl_sub_bits_is_match_context(_sb) || _br)`
/// (`erts/emulator/beam/erl_bits.h:385`) turned into a predicate: `orig` is either a
/// boxed on-heap bitstring (a match context, `erts_bs_start_match_3`, `erl_bits.c:95-102`)
/// or a boxed `BinRef` whose `val` word is the off-heap payload's data pointer. Anything
/// else means the block is not a bitstring this runtime produced.
///
/// # Safety
///
/// If `orig` is a boxed term, the header word it points at must be readable. Shape beyond
/// the header word is not validated (the provenance limit, module docs).
unsafe fn is_recognised_origin(orig: Eterm) -> bool {
    if !is_boxed_term(orig) {
        return false;
    }
    let p = boxed_val(orig);
    // SAFETY: `orig` is a boxed term, so its header word is initialised.
    let hdr = unsafe { *p };
    if hdr == HEADER_BIN_REF {
        // SAFETY: the `BinRef` header fixes the 3-word block, so word 1 (`val`) is live.
        return unsafe { *p.add(1) }.raw() != 0;
    }
    if header_subtag(hdr) as usize == HEAP_BITS_SUBTAG {
        // SAFETY: a heap-bits header fixes `[thing_word, n_bits, data…]`, so word 1 is
        // the bit-size word.
        let size = unsafe { *p.add(1) }.raw();
        return arityval(hdr) + 1 == heap_bits_size_words(size);
    }
    false
}

/// Validates a bitstring term's **shape** and reports where its bits live.
///
/// The single place the shape rules live; every `try_*` reader below routes here, so the
/// invariant can never drift between them.
///
/// # Safety
///
/// If `t` is a boxed term, the header word it points at must be readable. (That the
/// header word *is* readable is the caller's invariant — the provenance limit, module
/// docs — but every word read after it is checked against it.)
unsafe fn checked_bits(t: Eterm) -> TermResult<CheckedBits> {
    if !is_term_word(t) || !is_boxed_term(t) {
        // A non-term word (`THE_NON_VALUE`, a bare `HEADER`) or a non-boxed term (an
        // immediate, a cons cell) has no header word to read at all.
        return Err(TermError::NotATerm);
    }
    let p = boxed_val(t);
    // SAFETY: `t` is a boxed term, so its header word is initialised.
    let hdr = unsafe { *p };
    if hdr == HEADER_SUB_BITS {
        // SAFETY: the canonical `HEADER_SUB_BITS` word fixes the 5-word `ErlSubBits`
        // block (`erl_bits.h:60-82`), so words 1..4 are live.
        let base_flags = unsafe { *p.add(SUB_BITS_BASE_WORD) }.raw();
        let start = unsafe { *p.add(SUB_BITS_START_WORD) }.raw();
        let end = unsafe { *p.add(SUB_BITS_END_WORD) }.raw();
        let orig = unsafe { *p.add(SUB_BITS_ORIG_WORD) };
        let base = base_flags & !ERL_SUB_BITS_FLAG_MASK;
        // `base == 0` means "no storage" and `start > end` would make the window size
        // underflow; both are refused here, *before* the base is ever used as a pointer.
        // (`start == end` is a legal zero-bit window.)
        if base == 0 || start > end {
            return Err(TermError::BadBitstring);
        }
        // SAFETY: `orig` is the block's word 4, a term slot the caller says is live.
        if !unsafe { is_recognised_origin(orig) } {
            return Err(TermError::BadBitstring);
        }
        Ok(CheckedBits::Sub {
            base: base as *const u8,
            start,
            end,
        })
    } else if header_subtag(hdr) as usize == HEAP_BITS_SUBTAG {
        // SAFETY: a heap-bits header fixes `[thing_word, n_bits, data…]`, so word 1 is
        // the bit-size word.
        let size = unsafe { *p.add(1) }.raw();
        // The block's own arity must agree with its own bit count — that is what stops a
        // word misclassified as an `ErlHeapBits` from becoming a base pointer into
        // arbitrary storage. `heap_bits_size_words` is overflow-free for every input, so
        // a corrupt `size` word cannot wrap its way to a match.
        if arityval(hdr) + 1 != heap_bits_size_words(size) {
            return Err(TermError::BadBitstring);
        }
        // SAFETY: the arity check proves the block is `2 + ceil(n_bytes / 8)` words, so
        // word `ERL_HEAP_BITS_SIZE` is its first payload byte.
        Ok(CheckedBits::Heap {
            base: unsafe { p.add(ERL_HEAP_BITS_SIZE) }.cast::<u8>(),
            size,
        })
    } else {
        // Every other boxed header — tuple, map, float, bignum, fun, record, `BinRef`,
        // an `ErlSubBits` whose header arity is not the canonical one, … — is not a
        // bitstring this crate wrote.
        Err(TermError::BadBitstring)
    }
}

/// Splits an `ErlSubBits` into `(base_flags, start, end, orig)` after **validating** the
/// block's shape.
///
/// The checked counterpart of [`sub_bits_parts`]: same answer, same word order, but a
/// mis-shaped block is a typed error instead of a tuple built from garbage. It requires
/// the canonical `HEADER_SUB_BITS` word — a **heap** bitstring has no such words, so it
/// is [`TermError::BadBitstring`] here (use [`try_bitstring_parts`] for either form).
///
/// `base_flags` still carries the flag bits; mask with [`ERL_SUB_BITS_FLAG_MASK`] before
/// using it as a pointer (`start <= end` is guaranteed, but the base's *contents* are the
/// caller's responsibility — see the module docs' provenance limit).
///
/// # Errors
///
/// [`TermError::NotATerm`] if `t` is not a boxed term at all;
/// [`TermError::BadBitstring`] if it is boxed but not a well-shaped `ErlSubBits` (see the
/// module docs for the exact rules) — including a **heap** bitstring, which has no such
/// words (use [`try_bitstring_parts`] for either form).
///
/// # Safety
///
/// `t` must — if it is a boxed term — point at a readable header word. This reader
/// validates **shape**, never **provenance**: a freed or recycled block whose words still
/// look well formed is indistinguishable from a live one (module docs).
pub unsafe fn try_sub_bits_parts(t: Eterm) -> TermResult<(Eterm, Eterm, Eterm, Eterm)> {
    match unsafe { checked_bits(t) }? {
        CheckedBits::Sub { .. } => {
            // SAFETY: `checked_bits` proved word 0 is the canonical `HEADER_SUB_BITS`, so
            // words 1..4 are this block's `base_flags`/`start`/`end`/`orig`.
            unsafe {
                let p = boxed_val(t);
                Ok((
                    *p.add(SUB_BITS_BASE_WORD),
                    *p.add(SUB_BITS_START_WORD),
                    *p.add(SUB_BITS_END_WORD),
                    *p.add(SUB_BITS_ORIG_WORD),
                ))
            }
        }
        CheckedBits::Heap { .. } => Err(TermError::BadBitstring),
    }
}

/// Decomposes a bitstring into `(data base byte pointer, start bit offset, bit size)`
/// after **validating** the block's shape.
///
/// The checked counterpart of [`bitstring_parts`]: both representations are handled and
/// the answer has the same meaning (bit `i` is bit `(offset + i) % 8`, MSB-first, of
/// `base[(offset + i) / 8]`), but a mis-shaped block is a typed error rather than a
/// pointer taken from a garbage word. `offset + size` never overflows for a validated
/// sub-bits (it is bounded by the block's own `end`).
///
/// # Errors
///
/// As [`try_sub_bits_parts`], plus: a boxed header that is neither the canonical
/// `HEADER_SUB_BITS` nor a `HEAP_BITS_SUBTAG` header whose arity agrees with its bit-size
/// word is [`TermError::BadBitstring`].
///
/// # Safety
///
/// `t` must — if it is a boxed term — point at a readable header word. **Shape is
/// validated, not provenance**: the returned base still borrows storage that a freed or
/// moved block no longer owns, so the caller must keep the owning heap (or the underlying
/// payload) alive and unmoved while it uses the pointer.
pub unsafe fn try_bitstring_parts(t: Eterm) -> TermResult<(*const u8, usize, usize)> {
    match unsafe { checked_bits(t) }? {
        // SAFETY: no further read — `end >= start` was validated, so the subtraction
        // cannot underflow (saturating keeps even a future drift panic-free).
        CheckedBits::Sub { base, start, end } => Ok((base, start, end.saturating_sub(start))),
        CheckedBits::Heap { base, size } => Ok((base, 0, size)),
    }
}

/// Reads bit `i` (0-based, most significant bit of the first byte first) of a bitstring
/// after **validating** the block's shape and the index.
///
/// The checked counterpart of [`bit_at`]: identical bit order
/// (`(base[(offset + i) / 8] >> (7 - ((offset + i) % 8))) & 1`), but the term must be a
/// well-shaped bitstring and `i` must be inside its bit range, so the byte the read
/// touches is proven to belong to the block's own window.
///
/// # Errors
///
/// [`TermError::NotATerm`] / [`TermError::BadBitstring`] from [`try_bitstring_parts`],
/// plus [`TermError::BadBitstring`] when `i >= size_bits` (including `i == size_bits`, one
/// past the last bit) — refused *before* the byte is touched.
///
/// # Safety
///
/// `t` must — if it is a boxed term — point at a readable header word, and the storage
/// the block names must still be live and unmoved (**shape** is validated, not
/// provenance; module docs).
pub unsafe fn try_bit_at(t: Eterm, i: usize) -> TermResult<bool> {
    let (base, offset, size) = unsafe { try_bitstring_parts(t) }?;
    if i >= size {
        return Err(TermError::BadBitstring);
    }
    // `i < size` and `offset + size <= end` (a validated sub-bits) or `offset == 0` (heap
    // bits), so this cannot overflow; saturating keeps it panic-free anyway.
    let bit = offset.saturating_add(i);
    // SAFETY: bit `offset + i` is inside the validated `start..end` window (or inside the
    // heap-bits block for `offset == 0, i < size`), so the byte is in range.
    unsafe { Ok(raw_bit(base, bit) != 0) }
}

/// The raw `f64` bits stored in a boxed float term.
///
/// The 64-bit flonum is a 1-word thing (`erl_term.h:479`, `HEADER_FLONUM`), so the
/// value word is `boxed_val(t)[1]`. This is the crate's single public definition of
/// the float payload read; [`crate::compare`], [`crate::hashing`] and
/// [`crate::thing`] forward to it.
///
/// # Safety
///
/// `t` must be a live boxed float (`is_float(t)`).
pub unsafe fn float_bits(t: Eterm) -> u64 {
    // SAFETY: the caller guarantees `t` is a live boxed float, so its block is the
    // 2-word `[HEADER_FLONUM, bits]` and word 1 is initialised.
    unsafe { (*boxed_val(t).add(1)).raw() as u64 }
}

/// The `f64` value stored in a boxed float term.
///
/// The exact inverse of [`crate::thing::write_float`], delegating to [`float_bits`].
///
/// # Safety
///
/// As [`float_bits`]: `t` must be a live boxed float.
pub unsafe fn float_value(t: Eterm) -> f64 {
    // SAFETY: the caller guarantees `t` is a live boxed float.
    f64::from_bits(unsafe { float_bits(t) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{arityval, bitstring_size, is_binary, is_bitstring, make_arityval};
    use crate::heap::Heap;
    use crate::immediate::{make_small, NIL};
    use crate::ops::THE_NON_VALUE;
    use crate::thing::write_float;

    fn new_heap() -> Heap {
        Heap::new(256)
    }

    #[test]
    fn size_helpers_hit_the_boundaries() {
        for (n, b) in [
            (0usize, 0usize),
            (1, 1),
            (7, 1),
            (8, 1),
            (9, 2),
            (16, 2),
            (63, 8),
            (64, 8),
            (65, 9),
            (512, 64),
        ] {
            assert_eq!(bit_count_to_bytes(n), b, "NBYTES({n})");
        }
        // `heap_bits_size(n) = 2 + ceil(NBYTES(n) / 8)`.
        for (n, w) in [
            (0usize, 2usize),
            (1, 3),
            (8, 3),
            (63, 3),
            (64, 3),
            (65, 4),
            (512, 10),
            (513, 11),
        ] {
            assert_eq!(heap_bits_size_words(n), w, "heap_bits_size({n})");
        }
        // Overflow-free: the largest bit count must not wrap the byte/word math.
        assert!(bit_count_to_bytes(usize::MAX) < usize::MAX);
        assert!(heap_bits_size_words(usize::MAX) > 2);
    }

    #[test]
    fn heap_bits_block_layout_is_pinned() {
        let mut heap = new_heap();
        // 16 bits => 2 payload bytes => 1 data word => 3 words total.
        let words = heap_bits_size_words(16);
        assert_eq!(words, 3);
        // SAFETY: `alloc_words` returns `words` exclusively-owned, writable words.
        let hp = unsafe { heap.alloc_words(words).unwrap() };
        // SAFETY: `hp` owns `words` words.
        let term = unsafe { write_heap_bits(hp, &[0xAB, 0xCD], 16).unwrap() };
        assert_eq!(heap.used(), 3);
        // SAFETY: `term` is the bitstring this test just built; its 3 words are live.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(p, hp, "the root points at the block base");
            assert_eq!(*p, make_header(words - 1, HEAP_BITS_SUBTAG));
            assert_eq!(arityval(*p), words - 1);
            assert_eq!(*p.add(1), Eterm::from_raw(16)); // the bit size word
            assert_eq!(bitstring_size(term), 16);
            assert!(is_bitstring(term));
            assert!(is_binary(term), "16 bits is byte aligned");
        }
        let (base, off, size) = unsafe { bitstring_parts(term) };
        assert_eq!((off, size), (0, 16));
        // SAFETY: `base` covers the 2 payload bytes just written.
        let payload = unsafe { core::slice::from_raw_parts(base, 2) };
        assert_eq!(payload, &[0xAB, 0xCD]);
    }

    #[test]
    fn non_byte_aligned_bits_round_trip() {
        // `<<1:3>>` is the single byte `0b0010_0000`, MSB-first.
        let mut heap = new_heap();
        // SAFETY: 1 payload byte fits in the reserved 3 words.
        let term = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(3)).unwrap();
            write_heap_bits(hp, &[0b0010_0000], 3).unwrap()
        };
        assert_eq!(unsafe { bitstring_parts(term) }.2, 3);
        assert!(!unsafe { bit_at(term, 0) });
        assert!(!unsafe { bit_at(term, 1) });
        assert!(unsafe { bit_at(term, 2) });
        assert!(!unsafe { is_binary(term) }, "3 bits is not byte aligned");

        // Every bit length 1..=64 round-trips a known pattern.
        for n_bits in 1..=64usize {
            heap.reset();
            let n_bytes = bit_count_to_bytes(n_bits);
            // A pattern that exercises both a set and a clear bit in every byte.
            let bytes: Vec<u8> = (0..n_bytes).map(|i| (!(i as u8) & 0x55) | 0x80).collect();
            // SAFETY: `heap_hp` owns `heap_bits_size_words(n_bits)` words.
            let term = unsafe {
                let hp = heap.alloc_words(heap_bits_size_words(n_bits)).unwrap();
                write_heap_bits(hp, &bytes, n_bits).unwrap()
            };
            let (base, off, size) = unsafe { bitstring_parts(term) };
            assert_eq!((off, size), (0, n_bits), "n_bits={n_bits}");
            // SAFETY: `base` covers `ceil(n_bits / 8)` bytes.
            let stored = unsafe { core::slice::from_raw_parts(base, n_bytes) };
            for i in 0..n_bits {
                let want = (bytes[i >> 3] >> (7 - (i & 7))) & 1 == 1;
                assert_eq!(unsafe { bit_at(term, i) }, want, "n_bits={n_bits} bit={i}");
            }
            // The trailing bits of the last byte are zeroed.
            let tail = n_bits % 8;
            if tail != 0 {
                assert_eq!(
                    stored[n_bytes - 1] & (0xFFu8 >> tail),
                    0,
                    "n_bits={n_bits}: trailing bits must be zero"
                );
            }
        }
    }

    #[test]
    fn write_heap_bits_rejects_a_short_byte_count() {
        let mut heap = new_heap();
        // SAFETY: no word is written on the error paths, so a small reservation is fine.
        unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(512)).unwrap();
            assert_eq!(write_heap_bits(hp, &[0], 9), Err(TermError::BadBitstring));
            assert_eq!(write_heap_bits(hp, &[], 1), Err(TermError::BadBitstring));
            // Exactly enough is accepted.
            assert!(write_heap_bits(hp, &[0], 8).is_ok());
            // Extra bytes are ignored (only `ceil(n_bits / 8)` are copied).
            assert!(write_heap_bits(hp, &[0, 0], 3).is_ok());
        }
    }

    #[test]
    fn write_binary_matches_the_bit_count() {
        let mut heap = new_heap();
        // SAFETY: the reservation matches `heap_bits_size_words(len * 8)`.
        let term = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(4 * 8)).unwrap();
            write_binary(hp, &[1, 2, 3, 4]).unwrap()
        };
        assert_eq!(unsafe { bitstring_parts(term) }.2, 32);
        assert!(unsafe { is_binary(term) });
        // SAFETY: the base covers the 4 payload bytes.
        let payload = unsafe { core::slice::from_raw_parts(bitstring_parts(term).0, 4) };
        assert_eq!(payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn sub_bits_block_layout_is_pinned() {
        // A byte-aligned base so the low flag bits are free.
        #[repr(align(8))]
        struct Storage([u8; 2]);
        let storage = Storage([0b1010_1010, 0b1100_0000]);
        let base = storage.0.as_ptr() as usize;

        let mut heap = new_heap();
        // SAFETY: 5 words reserved for the `ErlSubBits`.
        let term = unsafe {
            let hp = heap
                .alloc_words(crate::refc::sub_bits_size_words())
                .unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(base),
                Eterm::from_raw(3),
                Eterm::from_raw(12),
                make_small(0),
            )
        };
        // SAFETY: `term` is the `ErlSubBits` just written.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, HEADER_SUB_BITS);
            assert_eq!((*p.add(1)).raw() as *const u8, storage.0.as_ptr());
            assert_eq!((*p.add(2)).raw(), 3);
            assert_eq!((*p.add(3)).raw(), 12);
            assert_eq!(*p.add(4), make_small(0));

            let (base_flags, start, end, orig) = sub_bits_parts(term);
            assert_eq!(base_flags.raw() as *const u8, storage.0.as_ptr());
            assert_eq!((start.raw(), end.raw(), orig), (3, 12, make_small(0)));

            let (pbase, off, size) = bitstring_parts(term);
            assert_eq!(pbase, storage.0.as_ptr());
            assert_eq!((off, size), (3, 9));
            // Bits 3..12 of `0b1010_1010_1100_0000` are `0101_0110_0`.
            let bits: Vec<bool> = (0..9).map(|i| bit_at(term, i)).collect();
            assert_eq!(
                bits,
                [false, true, false, true, false, true, true, false, false]
            );
        }

        // Flag bits in `base_flags` are masked off the base pointer.
        let mut heap = new_heap();
        // SAFETY: 5 words reserved; the flags do not change the layout.
        let term = unsafe {
            let hp = heap
                .alloc_words(crate::refc::sub_bits_size_words())
                .unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(base | ERL_SUB_BITS_FLAG_VOLATILE),
                Eterm::from_raw(0),
                Eterm::from_raw(8),
                NIL,
            )
        };
        assert_eq!(unsafe { bitstring_parts(term) }.0, storage.0.as_ptr());
    }

    #[test]
    fn float_readers_wrap_the_thing_layout() {
        let mut heap = new_heap();
        for value in [
            0.0f64,
            -0.0,
            1.5,
            f64::NAN,
            f64::INFINITY,
            f64::MIN,
            f64::MAX,
        ] {
            heap.reset();
            // SAFETY: 2 words reserved for the `[HEADER_FLONUM, bits]` block.
            let term = unsafe {
                let hp = heap.alloc_words(crate::thing::FLOAT_SIZE_WORDS).unwrap();
                write_float(hp, value)
            };
            // Bit-pattern comparison so NaN and -0.0 round-trip.
            assert_eq!(unsafe { float_bits(term) }, value.to_bits());
            assert_eq!(unsafe { float_value(term) }.to_bits(), value.to_bits());
        }
    }

    // -----------------------------------------------------------------------
    // Checked (shape-validating) readers
    // -----------------------------------------------------------------------

    /// The poison base the corruption tests use: it must never become a read.
    const POISON: usize = 0xDEAD_BEEF_DEAD_BEEF;

    /// Writes an `ErlSubBits` with the caller's four words and returns its root.
    fn sub_bits(heap: &mut Heap, base_flags: usize, start: usize, end: usize, orig: Eterm) -> Eterm {
        // SAFETY: `sub_bits_size_words()` (5) words are reserved and all are written.
        unsafe {
            let hp = heap
                .alloc_words(crate::refc::sub_bits_size_words())
                .unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(base_flags),
                Eterm::from_raw(start),
                Eterm::from_raw(end),
                orig,
            )
        }
    }

    /// A real on-heap bitstring `<<0b1010_1010>>` — the `orig` a *match context* carries.
    fn match_origin(heap: &mut Heap) -> Eterm {
        // SAFETY: the block is sized by `heap_bits_size_words(8)`.
        unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(8)).unwrap();
            write_heap_bits(hp, &[0b1010_1010], 8).unwrap()
        }
    }

    /// A 2-tuple `{1, 2}` — a boxed term that is neither a `BinRef` nor a bitstring.
    fn tuple_term_in_heap(heap: &mut Heap) -> Eterm {
        // SAFETY: 3 words for `[arityval(2), 1, 2]`.
        unsafe {
            let hp = heap.alloc_words(3).unwrap();
            *hp = make_arityval(2);
            *hp.add(1) = make_small(1);
            *hp.add(2) = make_small(2);
            crate::tuple::make_tuple(hp)
        }
    }

    #[test]
    fn checked_readers_refuse_a_poison_base_before_using_it() {
        let mut heap = new_heap();
        // A poisoned `base_flags` with a non-bitstring `orig`: the *origin* test refuses
        // the block, so `0xDEAD_BEEF_DEAD_BEEF` is never turned into a read.
        let t = sub_bits(&mut heap, POISON, 0, 8, make_small(7));
        assert_eq!(unsafe { try_sub_bits_parts(t) }, Err(TermError::BadBitstring));
        assert_eq!(unsafe { try_bitstring_parts(t) }, Err(TermError::BadBitstring));
        assert_eq!(unsafe { try_bit_at(t, 0) }, Err(TermError::BadBitstring));

        // With a *recognised* origin the same poison base passes: the check covers shape,
        // not provenance (module docs). Nothing below dereferences that pointer.
        let orig = match_origin(&mut heap);
        let t = sub_bits(&mut heap, POISON, 0, 8, orig);
        let (base, off, size) = unsafe { try_bitstring_parts(t) }.unwrap();
        assert_eq!(
            (base as usize, off, size),
            (POISON & !ERL_SUB_BITS_FLAG_MASK, 0, 8)
        );

        // A null masked base is refused on its own, whatever the origin says.
        let orig = match_origin(&mut heap);
        let t = sub_bits(&mut heap, ERL_SUB_BITS_FLAG_MUTABLE, 0, 8, orig);
        assert_eq!(unsafe { try_bitstring_parts(t) }, Err(TermError::BadBitstring));
    }

    #[test]
    fn checked_readers_refuse_end_before_start_and_the_unchecked_ones_do_not_panic() {
        #[repr(align(8))]
        struct Storage([u8; 4]);
        let storage = Storage([0b1010_1010, 0b1100_1111, 0, 0]);
        let mut heap = new_heap();
        let orig = match_origin(&mut heap);
        let t = sub_bits(&mut heap, storage.0.as_ptr() as usize, 16, 8, orig);
        assert_eq!(unsafe { try_sub_bits_parts(t) }, Err(TermError::BadBitstring));
        assert_eq!(unsafe { try_bitstring_parts(t) }, Err(TermError::BadBitstring));
        assert_eq!(unsafe { try_bit_at(t, 0) }, Err(TermError::BadBitstring));
        // The unchecked readers stay panic-free on the same block (a debug-profile panic on
        // runtime data is forbidden): the window saturates to empty.
        assert_eq!(unsafe { bitstring_parts(t) }.2, 0);
        assert!(!unsafe { bit_at(t, 0) });

        // `start == end` is a legal zero-bit window, not a corruption.
        let orig = match_origin(&mut heap);
        let t = sub_bits(&mut heap, storage.0.as_ptr() as usize, 8, 8, orig);
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.1, p.2)),
            Ok((8, 0))
        );
        assert_eq!(unsafe { try_bit_at(t, 0) }, Err(TermError::BadBitstring));
    }

    #[test]
    fn checked_readers_refuse_a_sub_bits_whose_origin_is_not_recognised() {
        let mut heap = new_heap();
        // A plain tuple, a float, an immediate: none is a `BinRef` or a heap bitstring.
        let tuple = tuple_term_in_heap(&mut heap);
        // SAFETY: the float block is sized by `FLOAT_SIZE_WORDS`.
        let float = unsafe {
            let hp = heap.alloc_words(crate::thing::FLOAT_SIZE_WORDS).unwrap();
            write_float(hp, 1.5)
        };
        for orig in [tuple, float, make_small(0), NIL] {
            let t = sub_bits(&mut heap, POISON, 0, 8, orig);
            assert_eq!(
                unsafe { try_bitstring_parts(t) },
                Err(TermError::BadBitstring),
                "orig {orig:?}"
            );
        }
        // A `BinRef` whose `val` (the payload data pointer) is null is refused too.
        // SAFETY: 3 words for `[HEADER_BIN_REF, val, next]`.
        let null_bin_ref = unsafe {
            let hp = heap.alloc_words(crate::refc::bin_ref_size_words()).unwrap();
            crate::refc::write_bin_ref(hp, core::ptr::null_mut(), core::ptr::null_mut())
        };
        let t = sub_bits(&mut heap, POISON, 0, 8, null_bin_ref);
        assert_eq!(unsafe { try_bitstring_parts(t) }, Err(TermError::BadBitstring));
    }

    #[test]
    fn checked_readers_refuse_a_heap_bitstring_whose_arity_disagrees() {
        let mut heap = new_heap();
        // A well-formed 16-bit block: 3 words, header arity 2 — accepted.
        // SAFETY: `heap_bits_size_words(16)` (3) words are reserved and all written.
        let t = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(16)).unwrap();
            *hp = make_header(heap_bits_size_words(16) - 1, HEAP_BITS_SUBTAG);
            *hp.add(1) = Eterm::from_raw(16);
            *hp.add(2) = Eterm::from_raw(0xABCD);
            make_boxed(hp)
        };
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.1, p.2)),
            Ok((0, 16))
        );
        // Header arity 5 for a 3-word block: refused, so a misclassified word can never
        // become a base pointer.
        // SAFETY: the block is the live 3-word one just written; only word 0 changes.
        unsafe { *boxed_val(t) = make_header(5, HEAP_BITS_SUBTAG) };
        assert_eq!(unsafe { try_bitstring_parts(t) }, Err(TermError::BadBitstring));
        // A size word the arity does not cover: refused as well.
        // SAFETY: as above, words 0..1 of the same live block.
        unsafe {
            *boxed_val(t) = make_header(2, HEAP_BITS_SUBTAG);
            *boxed_val(t).add(1) = Eterm::from_raw(1000);
        }
        assert_eq!(unsafe { try_bitstring_parts(t) }, Err(TermError::BadBitstring));
        assert_eq!(unsafe { try_bit_at(t, 0) }, Err(TermError::BadBitstring));
    }

    #[test]
    fn checked_readers_refuse_every_non_bitstring_word() {
        let mut heap = new_heap();
        let tuple = tuple_term_in_heap(&mut heap);
        // SAFETY: the float block is sized by `FLOAT_SIZE_WORDS`.
        let float = unsafe {
            let hp = heap.alloc_words(crate::thing::FLOAT_SIZE_WORDS).unwrap();
            write_float(hp, 1.5)
        };
        // An empty flat map is the 3-word `[arityval(2), MAP_HEADER_FLATMAP, 0]`-shaped
        // block (`flatmap_size_words(0)`).
        // SAFETY: `map_words_capacity(0)` (3) words are reserved for the writer.
        let map = unsafe {
            let hp = heap.alloc_words(crate::map::map_words_capacity(0)).unwrap();
            crate::map::write_map(hp, &[], &crate::atom::AtomTable::new()).unwrap()
        };
        // A `FUN_SUBTAG` header word: a fun, not a bitstring.
        // SAFETY: a zero-arity fun header is a one-word block.
        let fun = unsafe {
            let hp = heap.alloc_words(1).unwrap();
            *hp = make_header(0, crate::tag::FUN_SUBTAG);
            make_boxed(hp)
        };
        for t in [tuple, float, map, fun] {
            assert_eq!(
                unsafe { try_bitstring_parts(t) },
                Err(TermError::BadBitstring),
                "{t:?}"
            );
            assert_eq!(
                unsafe { try_sub_bits_parts(t) },
                Err(TermError::BadBitstring),
                "{t:?}"
            );
            assert_eq!(
                unsafe { try_bit_at(t, 0) },
                Err(TermError::BadBitstring),
                "{t:?}"
            );
        }

        // Words that are not terms at all, and terms that are not boxed ones.
        // SAFETY: 2 words for the `[CAR, CDR]` cons cell, both written before escape.
        let cons = unsafe {
            let hp = heap.alloc_words(2).unwrap();
            *hp.add(crate::list::CAR_OFFSET) = make_small(1);
            *hp.add(crate::list::CDR_OFFSET) = NIL;
            crate::list::make_list(hp)
        };
        for t in [NIL, THE_NON_VALUE, HEADER_SUB_BITS, make_small(1), cons] {
            assert_eq!(
                unsafe { try_bitstring_parts(t) },
                Err(TermError::NotATerm),
                "{t:?}"
            );
            assert_eq!(
                unsafe { try_sub_bits_parts(t) },
                Err(TermError::NotATerm),
                "{t:?}"
            );
            assert_eq!(unsafe { try_bit_at(t, 0) }, Err(TermError::NotATerm), "{t:?}");
        }
    }

    #[test]
    fn checked_readers_accept_a_real_heap_bitstring() {
        let mut heap = new_heap();
        let t = match_origin(&mut heap); // `<<0b1010_1010>>`
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.1, p.2)),
            Ok((0, 8))
        );
        assert_eq!(unsafe { try_bit_at(t, 0) }, Ok(true));
        assert_eq!(unsafe { try_bit_at(t, 1) }, Ok(false));
        assert_eq!(unsafe { try_bit_at(t, 2) }, Ok(true));
        assert_eq!(unsafe { try_bit_at(t, 7) }, Ok(false));
        // `i == size` is one past the last bit: refused, never a read.
        assert_eq!(unsafe { try_bit_at(t, 8) }, Err(TermError::BadBitstring));
        assert_eq!(unsafe { try_bit_at(t, usize::MAX) }, Err(TermError::BadBitstring));
        // A heap bitstring has no `ErlSubBits` words to split.
        assert_eq!(unsafe { try_sub_bits_parts(t) }, Err(TermError::BadBitstring));
    }

    #[test]
    fn checked_readers_accept_a_refc_window_and_a_match_context() {
        #[repr(align(8))]
        struct Payload([u8; 8]);
        let payload = Payload([0b1010_1010, 0b1100_1111, 0, 0, 0, 0, 0, 0]);
        let mut heap = new_heap();
        // SAFETY: `ERL_REFC_BITS_SIZE` (8) words are reserved and fully written; the
        // payload outlives the term and is word-aligned.
        let t = unsafe {
            let hp = heap.alloc_words(crate::refc::ERL_REFC_BITS_SIZE).unwrap();
            crate::refc::write_refc_binary(
                hp,
                payload.0.as_ptr() as *mut Eterm,
                core::ptr::null_mut(),
                payload.0.as_ptr(),
                3,
                9,
            )
        };
        let (base_flags, start, end, _orig) = unsafe { try_sub_bits_parts(t) }.unwrap();
        assert_eq!(base_flags.raw(), payload.0.as_ptr() as usize);
        assert_eq!((start.raw(), end.raw()), (3, 12));
        assert!(unsafe { crate::refc::is_refc_binary(t) }, "the origin is the pair's BinRef");
        let (base, off, size) = unsafe { try_bitstring_parts(t) }.unwrap();
        assert_eq!((base as usize, off, size), (payload.0.as_ptr() as usize, 3, 9));
        // Non-byte-aligned window: bits 3..12 of `1010_1010 1100_1111`.
        let bits: Vec<bool> = (0..9)
            .map(|i| unsafe { try_bit_at(t, i) }.unwrap())
            .collect();
        assert_eq!(
            bits,
            [false, true, false, true, false, true, true, false, false]
        );
        assert_eq!(unsafe { try_bit_at(t, 9) }, Err(TermError::BadBitstring));

        // A match context: `orig` is an on-heap bitstring, the window starts at bit 0.
        let hb = match_origin(&mut heap);
        let t = sub_bits(&mut heap, payload.0.as_ptr() as usize, 0, 8, hb);
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.1, p.2)),
            Ok((0, 8))
        );
        assert_eq!(unsafe { try_bit_at(t, 0) }, Ok(true));
        assert_eq!(unsafe { try_bit_at(t, 1) }, Ok(false));

        // Flag bits stay masked off the reported base.
        let hb = match_origin(&mut heap);
        let flags = payload.0.as_ptr() as usize | ERL_SUB_BITS_FLAG_VOLATILE;
        let t = sub_bits(&mut heap, flags, 0, 4, hb);
        assert_eq!(
            unsafe { try_bitstring_parts(t) }.map(|p| (p.0 as usize, p.1, p.2)),
            Ok((payload.0.as_ptr() as usize, 0, 4))
        );
    }
}
