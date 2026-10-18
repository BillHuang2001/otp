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

//! Raw-heap accessors for bitstring match contexts and heap bitstrings — the
//! bitstring family's **single** `#[allow(unsafe_code)]` module.
//!
//! Everything the `bs_*` opcodes need to *dereference* is here: reading a heap
//! bitstring's data words, reading/writing the `ErlSubBits` match-context fields,
//! allocating a fresh match context, materialising a result bitstring with an
//! explicit bit count, and — for the `bs_init_writable` accumulator — writing the
//! payload **in place** ([`InPlace`], C's writable `private_append`). Every `unsafe`
//! block carries a `// SAFETY:` rationale; every caller hands in a term this module
//! (or `beam_term`) already validated.//!
//! # Frozen layouts (C ground truth, `erts/emulator/beam/erl_bits.h:60-164`)
//!
//! ```text
//! ErlHeapBits  [thing_word, size_in_bits, data…]        data begins at word 2
//! ErlSubBits   [thing_word, base_flags, start, end, orig]
//! ```
//!
//! * `ErlHeapBits.thing_word` is `_make_header(heap_bits_size(bits) - 1,
//!   HEAP_BITS_SUBTAG)`, i.e. `HEAP_BITS_SUBTAG` after masking.
//! * `ErlSubBits` is the match-context form: word 2 is the current **read
//!   position** in bits and word 3 is the subject size in bits, so `end - start`
//!   is exactly `bitstring_size/1` for a context (`erl_bits.h:52-56`). The engine
//!   keeps `base_flags` at 0 (the C `ERL_SUB_BITS_FLAG_MUTABLE` bit only gates the
//!   native match-context reuse path, which the interpreter reproduces by re-reading
//!   the term each opcode instead).
//! * An `ErlSubBits` whose `orig` is a boxed `BinRef` is a **refc binary** — the
//!   off-heap form of a binary bigger than `ERL_ONHEAP_BINARY_LIMIT` bytes
//!   ([`beam_term::refc`]). `heap_bits` accepts it as a readable subject (its
//!   base, window offset and size come from the frozen `ErlSubBits`), which is what
//!   lets a `bs_create_bin` `append` seed itself from a previously built refc binary
//!   and lets a match run over a window that does not start on a byte boundary.
//!
//! `orig` is the subject term, kept so the context stays self-contained and its
//! byte slice can be re-derived at every opcode.
#![allow(unsafe_code)]

use beam_common::Result;
use beam_term::{
    boxed_val, Eterm, ERL_SUB_BITS_SIZE, HEADER_BIN_REF, HEADER_SUB_BITS,
};
use beam_term::refc::ERL_ONHEAP_BINARY_LIMIT;

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::machine::Machine;
use crate::terms::{reserve, Slack};

/// The data pointer, **bit** size and **bit** offset of a readable bitstring
/// subject: `(base, size, off)`.
///
/// The subject's bit `i` (`0 <= i < size`) is bit `off + i` of the byte payload
/// starting at `base` (MSB-first within each byte). `off` is `0` for an on-heap
/// bitstring (`ErlHeapBits`) and the frozen window start modulo 8 for a refc
/// binary (`beam_term::refc`), whose window need **not** be byte-aligned — the
/// family's bit readers thread `off` into their own bit index (see
/// [`subject_bytes`]).
///
/// Returns `None` for anything that is not a readable bitstring subject: a
/// non-boxed term, an engine match context ([`HEADER_SUB_BITS`] whose `orig` is not
/// a `BinRef`, i.e. the term [`make_ctx`] builds) or a bare off-heap binary
/// reference ([`HEADER_BIN_REF`]). Callers turn `None` into their own failure
/// (a `bs_start_match3` match failure, or a typed `unsupported`).
///
/// A **refc binary** — the off-heap form `beam_term::refc` builds for a result
/// larger than [`ERL_ONHEAP_BINARY_LIMIT`] bytes — *is* accepted: its base, window
/// offset and size come from the frozen `ErlSubBits`
/// ([`beam_term::bitstring_parts`]), so the rest of the family reads it exactly like
/// a heap binary, at whatever bit offset its window starts.
pub(super) fn heap_bits(t: Eterm) -> Option<(*const u8, usize, usize)> {
    // SAFETY: `is_bitstring` checks `is_boxed` before it dereferences the header.
    if !unsafe { beam_term::is_bitstring(t) } {
        return None;
    }
    let p = boxed_val(t);
    // SAFETY: `is_bitstring` established that `t` is a live bitstring block, so
    // word 0 is its header.
    let hdr = unsafe { *p };
    if hdr == HEADER_SUB_BITS {
        // SAFETY: `t` is a live bitstring whose header is `ErlSubBits`, so its
        // `orig` slot (word 4) is a live term.
        if !unsafe { beam_term::refc::is_refc_binary(t) } {
            // An engine match context (or a non-refc sub-bitstring): not a
            // readable subject here.
            return None;
        }
        // SAFETY: `t` is a live refc-binary bitstring (an `ErlSubBits` over a
        // `BinRef`), so `bitstring_parts` recovers its frozen base/offset/size.
        let (base, start, size) = unsafe { beam_term::bitstring_parts(t) };
        // SAFETY: `base` points at the byte holding bit 0 of the off-heap payload,
        // so the byte holding the window's first bit is `base + start / 8` and the
        // window's bit `i` is bit `start % 8 + i` of the payload from there. The
        // payload covers `ceil((start + size) / 8)` bytes, which from
        // `base + start / 8` is exactly `ceil((start % 8 + size) / 8)` bytes — the
        // slice [`subject_bytes`] hands out. (The caller keeps the payload alive.)
        return Some((unsafe { base.add(start / 8) }, size, start % 8));
    }
    if hdr == HEADER_BIN_REF {
        return None;
    }
    // An on-heap `ErlHeapBits`: `beam_term::bitstring_parts` (the frozen-layout owner,
    // `erl_bits.h:150-164`) recovers its data byte base, its zero bit offset and its
    // bit size, so this module never re-derives the `[thing_word, size, data…]` layout.
    // SAFETY: the header is neither `ErlSubBits` nor a bare `BinRef`, so `t` is a live
    // `ErlHeapBits` and `bitstring_parts` reads only its own `size`/data words.
    let (base, off, bits) = unsafe { beam_term::bitstring_parts(t) };
    Some((base, bits, off))
}
/// The byte payload of a bitstring **subject** term and its bit offset within it:
/// `(base, off)`.
///
/// The subject's bit `i` is bit `off + i` of `base` (MSB-first), so a bit reader
/// takes the subject's window-relative bit index `s` and reads `base` at `off + s`.
/// The slice spans `ceil((off + size) / 8)` bytes, i.e. every bit of the window is
/// covered (a byte-aligned subject starts at `off == 0`, exactly as before).
///
/// A subject that is not a readable bitstring (a match context, a bare off-heap
/// binary reference, a non-bitstring) is the documented M1 gap and yields
/// [`crate::error::unsupported`]. The returned slice borrows the subject's storage,
/// so the caller must keep the owning heap (or off-heap payload) alive and unmoved
/// for as long as it is used (the [`ExecutionContext`] ownership contract; an
/// off-heap payload is held by the [`Machine`] that built it).
pub(super) fn subject_bytes<'a>(t: Eterm) -> Result<(&'a [u8], usize)> {
    let (ptr, bits, off) = heap_bits(t)
        .ok_or_else(|| error::unsupported("bitstring: subject is not a readable binary"))?;
    let num_bytes = (off + bits).div_ceil(8);
    if num_bytes == 0 {
        // A 0-bit, byte-aligned window has no payload bytes (`off` is 0 here).
        return Ok((&[], 0));
    }
    // SAFETY: a live subject covers `ceil(num_bytes / word_bytes)` words' worth of
    // storage from `ptr` (a heap-bits block's data region at word 2, or the
    // byte-aligned floor of an off-heap window whose payload extends through
    // `ceil((off + bits) / 8)` bytes), so the `num_bytes` data bytes starting there
    // are in bounds and readable; the caller keeps that storage alive and unmoved for
    // `'a`.
    Ok((unsafe { core::slice::from_raw_parts(ptr, num_bytes) }, off))
}
/// The `(orig, start, end)` triple of an `ErlSubBits` match context.
///
/// `None` when `t` is not a boxed `ErlSubBits` term (`erl_bits.h:60-82`).
pub(super) fn ctx_parts(t: Eterm) -> Option<(Eterm, usize, usize)> {
    // `is_boxed` reads only the tag bits, never dereferences.
    if !beam_term::is_boxed(t) {
        return None;
    }
    let p = boxed_val(t);
    // SAFETY: `is_boxed` established that word 0 is this block's live header.
    if unsafe { *p } != HEADER_SUB_BITS {
        return None;
    }
    // SAFETY: an `ErlSubBits` is `[thing_word, base_flags, start, end, orig]`, so
    // words 2..=4 are the live `start`/`end`/`orig` fields.
    let start = unsafe { *p.add(2) }.raw();
    let end = unsafe { *p.add(3) }.raw();
    let orig = unsafe { *p.add(4) };
    Some((orig, start, end))
}

/// The inner subject (`orig`, word 4) of a match-context term, or `None` when `t`
/// is not one.
///
/// The readers that must *re-derive* the `orig` of a fresh subject (the
/// `bs_start_match*` reuse path and the `bs_get_position` snapshot) use this after
/// re-reading the context's operand, so the re-wrapped context keeps the inner
/// subject the C `reuse` path keeps — never the outer context.
pub(super) fn inner_orig(t: Eterm) -> Option<Eterm> {
    ctx_parts(t).map(|(orig, _, _)| orig)
}

/// Build a fresh match-context term positioned at bit `start` with subject size
/// `end` bits, over the `orig` the `read` closure returns.
///
/// The five words are `ERL_SUB_BITS_SIZE` (`erl_bits.h:128`), matching the C
/// `HEADER_SUB_BITS` block layout exactly; `base_flags` stays 0 (see the module
/// docs). The block is reserved **rooted** first — one collecting allocation a
/// moving collector may run under — and only then does `read` supply the context's
/// `orig` from the machine's *post-collection* state, so the stored `orig` is never
/// a stale copy (the frozen **reserve, then read, then write** ordering,
/// [`crate::terms`]). `extra` words of spare tail are handed to the reader for a
/// bignum operand.
pub(super) fn make_ctx<R>(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    start: usize,
    end: usize,
    extra: usize,
    read: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &mut Slack) -> Result<Eterm>,
{
    let hp = reserve(machine, ctx, ERL_SUB_BITS_SIZE + extra, RootSet::ALL_LIVE)?;
    // SAFETY: the rooted reserve returned `ERL_SUB_BITS_SIZE + extra`
    // exclusively-owned, writable words; the `extra`-word tail at
    // `hp + ERL_SUB_BITS_SIZE` is exactly what the slack hands the reader (it is not
    // aliased by the machine).
    let mut slack = unsafe { Slack::new(hp.add(ERL_SUB_BITS_SIZE), extra) };
    let orig = read(machine, &mut slack)?;
    // SAFETY: the reserve block is live and exclusively owned; every word is written
    // below (header, `base_flags`, `start`, `end`, `orig`) before the block is turned
    // into a term.
    unsafe {
        *hp = HEADER_SUB_BITS;
        *hp.add(1) = Eterm::from_raw(0);
        *hp.add(2) = Eterm::from_raw(start);
        *hp.add(3) = Eterm::from_raw(end);
        *hp.add(4) = orig;
    }
    Ok(beam_term::make_boxed(hp))
}

/// Overwrite the read position (bit `start`) of a match context in place.
///
/// The caller must pass a term [`ctx_parts`] accepted (a live `ErlSubBits`); the
/// context is mutated in place, mirroring the C `sb->start = …` assignments.
pub(super) fn set_ctx_start(t: Eterm, start: usize) {
    // SAFETY: the caller passed a term `ctx_parts` accepted as a live `ErlSubBits`
    // block, so word 2 is its mutable `start` field.
    unsafe {
        *boxed_val(t).add(2) = Eterm::from_raw(start);
    }
}

/// Overwrite the **window end** (bit `end`) of an `ErlSubBits` in place — C's
/// `sb->end = new_position` (`erl_bits.c:1975`, the writable `private_append` arm).
///
/// The caller must pass a term [`ctx_parts`] accepted (a live `ErlSubBits`) whose
/// bits `[0, end)` are the ones it means to publish, and must keep
/// `start <= end <= `the payload's capacity in bits.
pub(super) fn set_ctx_end(t: Eterm, end: usize) {
    // SAFETY: the caller passed a term `ctx_parts` accepted as a live `ErlSubBits`
    // block, so word 3 is its mutable `end` field.
    unsafe {
        *boxed_val(t).add(3) = Eterm::from_raw(end);
    }
}
/// Build a bignum term from `digits` (least significant first) with the given sign.
///
/// `digits` must be non-empty and normalised (no leading zero digit) — exactly the
/// shape [`beam_term::write_big`] requires (`big.h:96-97`,
/// `BIG_NEED_SIZE(n) = n + 1`). The block is reserved through the **rooted** seam
/// ([`crate::terms::reserve`] → `Machine::alloc_rooted`), so a host with a moving
/// collector may relocate the machine's root set as part of this allocation; the
/// term is then valid until the next collecting allocation.
pub(super) fn make_big(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    negative: bool,
    digits: &[u64],
) -> Result<Eterm> {
    if digits.is_empty() {
        // An internal invariant (the caller normalises zero to a small); a
        // zero-width bignum would be a wrong result, so surface it.
        return Err(error::unsupported("bitstring: zero-width bignum"));
    }
    let hp = reserve(
        machine,
        ctx,
        beam_term::big_size_words(digits.len()),
        RootSet::ALL_LIVE,
    )?;
    // SAFETY: the rooted reserve returned exactly `big_size_words(n) = n + 1`
    // exclusively-owned, writable words — the block `write_big` requires and fully
    // writes (header + `n` `ErtsDigit`s, `big.h:68-73`). The context owns the arena
    // and keeps it alive for as long as the term is used.
    Ok(unsafe { beam_term::write_big(hp, negative, digits) })
}

/// The `(negative, digits)` of a bignum term **copied out** (digits are
/// `ErtsDigit`s, least significant first).
///
/// `None` when `t` is not a bignum. This is the safe read side of the wide
/// integer *build* path (`fmt_big_*`, `erl_bits.c:724`/`:811`); copying keeps the
/// caller free of `unsafe` and of a borrow on the term's storage.
pub(super) fn big_parts_safe(t: Eterm) -> Option<(bool, Vec<u64>)> {
    // SAFETY: `is_big` checks `is_boxed` before it dereferences the header.
    if !unsafe { beam_term::is_big(t) } {
        return None;
    }
    // SAFETY: `is_big` established that `t` is a live bignum block written by
    // `write_big`, which is exactly the precondition `big_parts` documents.
    let (negative, digits) = unsafe { beam_term::big_parts(t) };
    Some((negative, digits.to_vec()))
}

/// Whether `t` is an **off-heap refc binary** ([`beam_term::refc`]) — the frozen
/// representation this family's result builders choose above
/// [`ERL_ONHEAP_BINARY_LIMIT`] bytes.
///
/// The safe read side of [`beam_term::refc::is_refc_binary`], mirroring
/// [`big_parts_safe`]: the family's `#[cfg(test)]` modules live under the crate-wide
/// `#![deny(unsafe_code)]` and so cannot call the `unsafe` predicate themselves, so the
/// single dereference stays here in the node's marked module.
#[cfg(test)]
pub(super) fn is_refc_binary(t: Eterm) -> bool {
    // SAFETY: `is_refc_binary` checks `is_boxed` before it dereferences the header and
    // only reads the `ErlSubBits.orig` slot when the header says `ErlSubBits`, so a
    // non-refc term (including a non-bitstring) is answered `false` without any
    // dereference of a non-live word.
    unsafe { beam_term::refc::is_refc_binary(t) }
}

/// Build an **on-heap-only** bitstring of exactly `nbits` bits from the leading bits
/// of `bytes` (MSB-first).
///
/// The produced `ErlHeapBits` never carries the heap's previous contents: the whole
/// data region is zeroed before the payload is copied in (so the trailing padding
/// bytes of the last data word are zero) and the unused low bits of the final payload
/// byte are cleared — the invariants
/// [`beam_term::bitstring::write_heap_bits`] establishes (this builder is a thin
/// wrapper over it, so the frozen `[thing_word, size, data…]` layout has exactly one
/// definition).
///
/// This is the on-heap `ErlHeapBits` builder and nothing more: it is the branch
/// [`make_binary`] takes at/below the frozen limit, and it refuses a larger result
/// with [`crate::error::unsupported`]. Callers that need the size-dispatching entry
/// (anything that can build a result of arbitrary size — the `bs_get_binary2`/
/// `bs_get_tail`/`bs_match binary` result builders) must call [`make_binary`] instead.
///
/// `bytes` must hold at least `ceil(nbits / 8)` bytes; a longer slice is fine (the
/// surplus is ignored). More than [`ERL_ONHEAP_BINARY_LIMIT`] bytes yields
/// [`crate::error::unsupported`].
pub(super) fn make_bits(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    bytes: &[u8],
    nbits: usize,
) -> Result<Eterm> {
    let num_bytes = nbits.div_ceil(8);
    if num_bytes > ERL_ONHEAP_BINARY_LIMIT {
        return Err(error::unsupported("bitstring: result binary > 64 bytes"));
    }
    if bytes.len() < num_bytes {
        // An internal invariant: every caller passes a buffer covering `nbits`.
        return Err(error::unsupported("bitstring: short byte buffer"));
    }
    let hp = reserve(
        machine,
        ctx,
        beam_term::bitstring::heap_bits_size_words(nbits),
        RootSet::ALL_LIVE,
    )?;
    // SAFETY: the rooted reserve returned exactly
    // `beam_term::bitstring::heap_bits_size_words(nbits)` exclusively-owned, writable
    // words — the block `write_heap_bits` requires — and `bytes` is a Rust-owned
    // buffer (never a live term's storage), so the reserve cannot invalidate it. The
    // writer establishes the whole data region (padding bytes zeroed, the unused
    // trailing bits of the final payload byte masked), so this builder never re-derives
    // the frozen heap-bits layout.
    Ok(unsafe { beam_term::bitstring::write_heap_bits(hp, bytes, nbits)? })
}

/// Build a bitstring of exactly `nbits` bits from the leading bits of `bytes`,
/// choosing the representation the frozen limits prescribe.
///
/// `<= ERL_ONHEAP_BINARY_LIMIT` bytes become an on-heap `ErlHeapBits`
/// ([`make_bits`]); a larger result becomes an **off-heap refc binary** — the
/// 8-word `[BinRef][ErlSubBits]` pair of `beam_term::refc` whose `BinRef.val` word
/// points at a fresh ref-counted payload allocated through `machine`
/// ([`Machine::alloc_refc_binary`]). That is exactly the C threshold
/// (`bs_instrs.tab:562-566` picks `ERL_REFC_BITS_SIZE` above `ERL_ONHEAP_BITS_LIMIT`
/// bits), so a `bs_create_bin` result of any size is representable.
///
/// `bytes` must hold at least `ceil(nbits / 8)` bytes; a longer slice is fine (the
/// surplus is ignored) and the unused trailing bits of the final byte are zeroed
/// (the canonical "unused low bits are zero" invariant).
pub(super) fn make_binary(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    bytes: &[u8],
    nbits: usize,
) -> Result<Eterm> {
    let num_bytes = nbits.div_ceil(8);
    if num_bytes <= ERL_ONHEAP_BINARY_LIMIT {
        return make_bits(machine, ctx, bytes, nbits);
    }
    if bytes.len() < num_bytes {
        // An internal invariant: every caller passes a buffer covering `nbits`.
        return Err(error::unsupported("bitstring: short byte buffer"));
    }
    let mut payload = bytes[..num_bytes].to_vec();
    if nbits % 8 != 0 {
        let keep = nbits % 8;
        payload[num_bytes - 1] &= 0xFFu8 << (8 - keep);
    }
    // Reserve the 8-word `[BinRef][ErlSubBits]` pair **rooted** first — the single
    // collecting allocation of this build may relocate the machine's root set — and
    // only then allocate the off-heap payload and write the pair. The payload is
    // `beam_alloc`'s and is never moved by a collection, so the pre-reserve pointer
    // hazard the frozen ordering guards against cannot arise here.
    let hp = reserve(
        machine,
        ctx,
        beam_term::refc::ERL_REFC_BITS_SIZE,
        RootSet::ALL_LIVE,
    )?;
    let data = machine.alloc_refc_binary(&payload)?;
    // SAFETY: the rooted reserve returned exactly `ERL_REFC_BITS_SIZE` (8)
    // exclusively-owned words — the `[BinRef][ErlSubBits]` pair `write_refc_binary`
    // fully writes — and `data` points at the first byte of a live off-heap payload
    // the machine keeps alive (and unmoved) for at least as long as the term is
    // used; `beam_alloc` guarantees it is `UNIT_SIZE`-aligned, so the low
    // `ERL_SUB_BITS_FLAG_MASK` bits of the base pointer are clear.
    Ok(unsafe {
        beam_term::refc::write_refc_binary(
            hp,
            data.cast::<Eterm>(),
            core::ptr::null_mut(),
            data,
            0,
            nbits,
        )
    })
}

/// Build the empty writable binary `bs_init_writable` produces.
///
/// C's `erts_bs_init_writable` (`erl_bits.c:2021`) allocates a fresh ref-counted
/// payload of `capacity` bytes and returns an `ErlSubBits` whose window is
/// **zero-length** (`build_writable_bitstring` passes `current_size = 0`), carrying
/// the capacity only in the off-heap binary's `apparent_size`. The engine
/// reproduces the observable part exactly: a 0-bit refc binary over a
/// `capacity`-byte payload, which the first `bs_create_bin` `append`/`private_append`
/// segment then extends.
///
/// # The payload carries a capacity, not a length
///
/// The payload is allocated by [`Machine::alloc_refc_binary_with_capacity`]
/// (`beam_alloc::RefcBinary::with_capacity`): its writable capacity is the requested
/// `capacity` bytes, on top of a 4x reservation (`DEFAULT_RESERVATION_FACTOR`, clamped to
/// `MAX_PAYLOAD_BYTES`). That is the shape C's `bin_size = capacity` allocation has
/// (`erl_bits.c:2023`), and it is what lets the sink write several rounds straight into
/// the reservation before [`InPlace::grow`] has to ask for more. The machine records the
/// payload's **real** capacity in the writable registry ([`Machine::mark_writable`]),
/// never the hint.
///
/// The *visible* bits are the term's window — zero of them here (C's `apparent_size = 0`)
/// — while the payload's own logical length is its whole capacity, zero-filled
/// ([`Machine::cover_writable_capacity`]). That length is not a length of *content*: it
/// is the extent of the initialised backing store, and it is what makes a later
/// relocation lossless, because `beam_alloc` copies exactly `len()` bytes when it moves a
/// payload. A payload whose handle claimed `len == 0` would lose every byte the sink had
/// written the first time the reservation ran out.
///
/// `capacity == 0` (the `x(0)` hint really can be a non-negative small integer zero,
/// and C then allocates `bin_size = 0`) is reproduced as a zero-capacity payload; see
/// [`Machine::alloc_refc_binary_with_capacity`] for why growth from it still makes
/// progress.
///
/// The 8-word `[BinRef][ErlSubBits]` pair is reserved **rooted** first, then the
/// off-heap payload is allocated and the pair written (the frozen
/// reserve-then-write ordering; the payload is off-heap and never moves *here* — a
/// later growth may relocate it, which [`InPlace::grow`] republishes).
pub(super) fn make_writable(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    capacity: usize,
) -> Result<Eterm> {
    let hp = reserve(
        machine,
        ctx,
        beam_term::refc::ERL_REFC_BITS_SIZE,
        RootSet::ALL_LIVE,
    )?;
    let (data, real_capacity) = machine.alloc_refc_binary_with_capacity(capacity)?;
    // The payload is the accumulator's backing store: register it so a following
    // `private_append` can write into it in place ([`writable_append_target`]), exactly
    // as C's `build_writable_bitstring` sets `BIN_FLAG_WRITABLE|BIN_FLAG_ACTIVE_WRITER`
    // and threads the binary onto `p->wrt_bins` (`erl_bits.c:1684-1720`).
    machine.mark_writable(data, real_capacity);
    // SAFETY: as `make_binary` — 8 exclusively-owned words for the pair, a live
    // `UNIT_SIZE`-aligned off-heap payload owned by `machine`, and a 0-bit window
    // (`start = end = 0`).
    Ok(unsafe {
        beam_term::refc::write_refc_binary(
            hp,
            data.cast::<Eterm>(),
            core::ptr::null_mut(),
            data,
            0,
            0,
        )
    })
}

// ---------------------------------------------------------------------------
// The in-place writable accumulator (`private_append`)
// ---------------------------------------------------------------------------

/// The in-place append target of `src`, or `None` when `src` must take the
/// copy-rebuild path: `(payload base, capacity in bits, window end in bits)`.
///
/// `Some` exactly when `src`
/// 1. is an **off-heap refc binary** ([`beam_term::refc::is_refc_binary`]) — an
///    `ErlSubBits` over a `BinRef` (an on-heap `ErlHeapBits` has no payload to write
///    into),
/// 2. has `start == 0` — C **asserts** this for both append forms
///    (`erl_bits.c:1943-1948`), because an in-place append writes at absolute payload
///    offsets;
/// 3. has a payload **this machine allocated for appending**
///    ([`Machine::writable_capacity`] — the writable-flag registry).
///
/// The capacity is *only* ever taken from that registry: the payload roster
/// ([`Machine::refc_binaries`]) knows every payload's length, and an ordinary binary's
/// length is emphatically not a writable capacity.
pub(super) fn writable_append_target(
    machine: &Machine,
    src: Eterm,
) -> Option<(*mut u8, usize, usize)> {
    // SAFETY: `is_refc_binary` checks `is_boxed` before it dereferences the header and
    // only reads the `ErlSubBits.orig` slot when the header says `ErlSubBits`.
    if !unsafe { beam_term::refc::is_refc_binary(src) } {
        return None;
    }
    // SAFETY: `src` is a live refc-binary `ErlSubBits` over a live `BinRef`, so its
    // frozen `base`/`start`/`size` are initialised and the payload is alive (a
    // `beam_alloc::RefcBinary` handle owned by some machine keeps it so).
    let (base, start, size) = unsafe { beam_term::bitstring_parts(src) };
    if start != 0 || base.is_null() {
        return None;
    }
    let capacity_bits = machine.writable_capacity(base)?.checked_mul(8)?;
    Some((base as *mut u8, capacity_bits, size))
}

// ---------------------------------------------------------------------------
// Growth support: the alias guard and the republish
// ---------------------------------------------------------------------------

/// The `(payload base, on-heap owner block)` pair a **live boxed term** names, when the
/// term is an off-heap-binary shape.
///
/// Three shapes name an off-heap payload:
///
/// * a bare `BinRef` (`HEADER_BIN_REF`) — its `val` word is the payload's first data
///   byte, and the block itself is the owner identity;
/// * a full refc binary (`ErlSubBits` over a `BinRef`) — the window base and `val` are
///   the same payload, the `ErlSubBits` block is the owner identity;
/// * a **match context** `ErlSubBits` over an off-heap base — its `base_flags` word *is*
///   the payload pointer (flags masked off), so it names the payload too, and it is the
///   one shape whose base word is not routed through a `BinRef`.
///
/// An on-heap `ErlHeapBits` names no off-heap payload (its data lives in the heap block
/// itself) and yields `None`.
fn payload_named_by(t: Eterm) -> Option<(*const u8, *mut Eterm)> {
    if !beam_term::is_boxed(t) {
        return None;
    }
    // SAFETY: `t` is boxed, so `boxed_val(t)` is a live block whose header word is
    // initialised — the same one-word read `beam_term`'s own predicates make.
    let root = boxed_val(t);
    let header = unsafe { *root };
    if header == HEADER_BIN_REF {
        // SAFETY: the header word says `BinRef`, so words 1 and 2 are initialised.
        let (val, _next) = unsafe { beam_term::refc::bin_ref_parts(t) };
        return Some((val as *const u8, root));
    }
    if header != HEADER_SUB_BITS {
        return None;
    }
    // SAFETY: the header word says `ErlSubBits`, so words 1..4 are initialised; word 1
    // is `base_flags`, whose low `ERL_SUB_BITS_FLAG_MASK` bits are flags and not part of
    // the pointer.
    let (base_flags, _start, _end, _orig) = unsafe { beam_term::bitstring::sub_bits_parts(t) };
    let base = (base_flags.raw() & !beam_term::bitstring::ERL_SUB_BITS_FLAG_MASK) as *const u8;
    Some((base, root))
}

/// Whether any term the machine's **root set** holds names the payload at `base` through
/// a *different* on-heap pair than `own`'s — the guard a payload relocation must pass
/// before `beam_alloc` frees the old block.
///
/// # Why this is required
///
/// [`beam_alloc::RefcBinary::ensure_capacity`] refuses to grow a payload that any other
/// *handle* shares (`refcount() != 1`), which covers every **escaped** copy: a message
/// or a runtime store adopts a cloned handle, so the payload can only be relocated while
/// this machine holds the only handle. What that check cannot see is a **second on-heap
/// pair in this very machine** naming the same payload without a handle of its own — a
/// GC copy of the pair taken in an earlier collection, a bare `BinRef` a family built, or
/// a match context whose `base_flags` is this payload. A move frees the old block, so
/// such a pair's word would dangle: a **use-after-free**, not merely a stale value.
///
/// # Scope
///
/// The scan covers the three files a collection relocates — the x-register file, the FR
/// file and the whole Erlang stack — plus the parked raise reason
/// ([`Machine::pending_exception`]). Those are the terms this machine can reach; a term
/// already handed to a host is covered by the refcount gate above, not by this scan.
///
/// Refusing costs only performance: the caller promotes to the copy-rebuild form, which
/// is always byte-correct. A *false* refusal is therefore safe, which is why the scan
/// over-approximates (a dead-but-uncleared word only makes it refuse).
fn payload_aliased_elsewhere(machine: &Machine, base: *const u8, own: Eterm) -> bool {
    // The accumulator's own two blocks — its `ErlSubBits` root (the term itself) and the
    // `BinRef` that root points at — are the pair being relocated, not aliases of it.
    // SAFETY: `own` is the caller's live refc-binary accumulator, so its `orig` slot is
    // the live `BinRef` of the same pair.
    let (_flags, _start, _end, orig) = unsafe { beam_term::bitstring::sub_bits_parts(own) };
    let own_blocks = [boxed_val(own), boxed_val(orig)];
    let aliases = |t: Eterm| match payload_named_by(t) {
        Some((named, block)) => named == base && !own_blocks.contains(&block),
        None => false,
    };
    let parked = machine.pending_exception.map(|(_class, reason)| reason);
    machine.x.iter().copied().any(aliases)
        || machine.fr.iter().copied().any(aliases)
        || machine.stack.iter().copied().any(aliases)
        || parked.is_some_and(aliases)
}

/// Republish the accumulator's `[BinRef][ErlSubBits]` pair over the payload's **new**
/// address after a relocation, preserving its window (`start`/`end`) and its
/// `BinRef.next` chain word.
///
/// Only the two words that name the payload change — `BinRef.val` (word 1 of the pair)
/// and the `ErlSubBits.base_flags` word (which *is* the payload pointer, `erl_bits.h:95`)
/// — because `beam_term::refc::write_refc_binary` rewrites the whole pair from the
/// arguments and `beam_term` owns that frozen layout, so this never hand-rolls it. The
/// pair's address and the returned `Eterm` are unchanged: every alias of the *term* (the
/// same `Eterm` in a register, a Y slot, a copied pair) reads the new payload.
fn republish_over(src: Eterm, new_base: *mut u8) {
    // SAFETY: `src` is a live refc-binary pair (the caller's accumulator), so its
    // `ErlSubBits` words 1..4 are initialised: `orig` is the live `BinRef` of the pair and
    // its 8 words (`ERL_REFC_BITS_SIZE`) are the exclusively-owned block
    // `write_refc_binary` rewrites. `new_base` is the payload the machine's own handle now
    // names, `UNIT_SIZE`-aligned (so the flag bits are clear) and covering the same window
    // the pair already had (a relocation copies the live bytes and never changes the
    // window).
    unsafe {
        let (_base_flags, start, end, orig) = beam_term::bitstring::sub_bits_parts(src);
        let (_val, next) = beam_term::refc::bin_ref_parts(orig);
        let (start, end) = (start.raw(), end.raw());
        let pair = boxed_val(orig);
        let republished = beam_term::refc::write_refc_binary(
            pair,
            new_base.cast::<Eterm>(),
            next,
            new_base,
            start,
            end - start,
        );
        debug_assert_eq!(
            republished, src,
            "a republish keeps the pair's address and its `ErlSubBits` root"
        );
    }
}

/// An **in-place bit sink** over a writable accumulator payload — C's writable
/// `private_append` arm (`erts_bs_private_append_checked`, `erl_bits.c:1943-2020`),
/// which bumps `sb->end` to the new position instead of copying.
///
/// The sink writes bits MSB-first into the payload starting at the accumulator's own
/// window end (the sink's `start` field), exactly like the copy-rebuild writer it
/// replaces, but the payload is written **where it already is**: no `Vec`, no fresh
/// payload, no `[BinRef][ErlSubBits]` reservation, and the produced term is the
/// **same `Eterm`** the accumulator already was ([`InPlace::finish`] only rewrites the
/// frozen `ErlSubBits.end` word).
///
/// # The invariant that makes the mutation unobservable
///
/// Writes always land at bit offsets `>= start` — the accumulator's `end` at the time
/// the sink was created — and never past the payload's capacity
/// ([`InPlace::remaining_bits`]). The capacity is **growable**
/// ([`InPlace::grow`], the `beam_alloc` capacity word), so a build that outgrows the
/// current capacity either widens the payload (in place, or by a relocation the sink
/// republishes) or **promotes** to the copy-rebuild form when the payload cannot be
/// grown. Every alias of the payload (a term copied before the append, another
/// process's `ErlSubBits` over the same payload, a message's shared binary) has a window
/// ending at or before that offset, so it reads exactly the bytes it read before. The
/// only term whose *window* changes is the accumulator itself, whose `end` is bumped on
/// success — precisely C's `sb->end = new_position`.
///
/// # The relocation guard (the one case where a byte buffer moves)
///
/// [`beam_alloc::RefcBinary`]'s growth refuses to touch a payload any other **handle**
/// shares (`refcount() == 1` is required), which already covers every payload that
/// escaped this machine: a message, a send, or a runtime store adopts a *clone* of the
/// handle. What that gate cannot see is a second on-heap **pair** in this machine naming
/// the same payload (a GC copy of the pair, a bare `BinRef`, a match context whose
/// `base_flags` is this payload). A move frees the old block, so such a word would be a
/// use-after-free; [`InPlace::grow`] therefore scans the machine's root set
/// ([`payload_aliased_elsewhere`]) and refuses the move (promoting instead) when one
/// exists. See that function for the scope and why an over-approximation is safe.
///
/// # Safety / aliasing contract (a deliberate, scoped relaxation)
///
/// [`beam_alloc::RefcBinary`] documents its payload as immutable, and this sink writes
/// through a raw pointer into it. That is sound *for the accumulator only* because
/// (a) only a payload the machine explicitly registered as writable
/// ([`Machine::mark_writable`]) is ever written — an ordinary refc binary never is,
/// and (b) no Rust reference to those bytes is alive across a write: the engine's
/// builder holds raw pointers, never a `&[u8]`, while appending (a segment source that
/// *does* alias the payload makes the writer promote to a fresh `Vec` first). It is the
/// Rust counterpart of C's `BIN_FLAG_WRITABLE` binary, whose `orig_bytes` are likewise
/// mutable exactly while that flag is set.
pub(crate) struct InPlace {
    /// The payload's first data byte (the `ErlSubBits` base / `BinRef.val`).
    base: *mut u8,
    /// The first bit this build may write: the accumulator's window end (`start`).
    start: usize,
    /// The payload capacity in bits.
    capacity: usize,
    /// The next bit to write, absolute in the payload.
    cur: usize,
    /// The accumulator term whose `ErlSubBits.end` a successful build rewrites.
    src: Eterm,
}

impl InPlace {
    /// A sink over the payload at `base`, writing from bit `start_bits` of a
    /// `capacity_bits`-bit payload, publishing through `src`.
    ///
    /// The caller must have obtained `(base, capacity_bits, start_bits)` from
    /// [`writable_append_target`] for this very `src`, so the payload is alive and
    /// writable and `0 <= start_bits <= capacity_bits`.
    pub(super) fn new(base: *mut u8, capacity_bits: usize, start_bits: usize, src: Eterm) -> InPlace {
        debug_assert!(start_bits <= capacity_bits, "the window end fits the payload");
        InPlace {
            base,
            start: start_bits,
            capacity: capacity_bits,
            cur: start_bits,
            src,
        }
    }

    /// The bits written so far: the **whole** result's bit count (the accumulator's
    /// window end plus everything this build appended).
    pub(super) fn bit_len(&self) -> usize {
        self.cur
    }

    /// How many more bits fit in the payload.
    pub(super) fn remaining_bits(&self) -> usize {
        self.capacity.saturating_sub(self.cur)
    }

    /// Widen the payload so that this sink can write `need_bits` **more** bits in place,
    /// returning whether it can.
    ///
    /// `false` is the writer's "promote to the copy-rebuild form" signal — it is the
    /// always-available fallback, so a refusal never loses bytes. Growth is refused when
    ///
    /// * `beam_alloc` will not relocate the payload (it is **shared**: some host or
    ///   another process adopted a handle, C's `erts_pin_writable_binary` situation;
    ///   or the request exceeds the payload's `max_capacity`; or the allocator could not
    ///   serve the block) — [`Machine::grow_writable_payload`] folds all three into
    ///   `Ok(None)`; or
    /// * the payload is **aliased inside this machine** by a second on-heap pair
    ///   ([`payload_aliased_elsewhere`]), which a move would leave dangling.
    ///
    /// # `Moved` — the republish
    ///
    /// `beam_alloc` frees the old block on a move, so the accumulator's own pair is
    /// rewritten over the new address ([`republish_over`]): the `BinRef.val` word and the
    /// `ErlSubBits.base_flags` pointer word move, the pair's address, its `start`/`end`
    /// window and the returned `Eterm` (it is `self.src`, untouched) do not. `self.base`
    /// follows, and `self.capacity` is re-read from the bytes `beam_alloc` reports, so the
    /// invariant below holds for the widened payload too.
    ///
    /// The bytes this sink already wrote survive the move because the machine keeps the
    /// payload's logical length at its whole capacity
    /// ([`Machine::cover_writable_capacity`]) — a relocation copies exactly `len()` bytes,
    /// and here `len()` is the entire backing store, of which the written prefix is a
    /// part.
    ///
    /// # Safety
    ///
    /// The republish writes through `self.src`'s frozen pair, and it is sound for exactly
    /// the reasons the payload mutation is (see the struct's `SAFETY` section), plus two
    /// more this method establishes: (a) the pair is the accumulator's own — its `ErlSubBits`
    /// root and `BinRef` block are the ones `InPlace` was built for, so the write touches
    /// no other term's words, and (b) no **other** on-heap pair names the old payload
    /// ([`payload_aliased_elsewhere`] over the machine's root set), so no word outside this
    /// pair is left pointing at the block `beam_alloc` freed.
    ///
    /// # `need_bits == 0`
    ///
    /// A zero-growth request that already fits is answered `true` without touching the
    /// payload, so a caller may consult the sink unconditionally.
    pub(super) fn grow(&mut self, machine: &mut Machine, need_bits: usize) -> Result<bool> {
        let wanted_bits = self.cur.saturating_add(need_bits);
        if wanted_bits <= self.capacity {
            return Ok(true);
        }
        let wanted_bytes = wanted_bits.div_ceil(8);
        if payload_aliased_elsewhere(machine, self.base, self.src) {
            return Ok(false);
        }
        let Some((new_base, capacity_bytes)) = machine.grow_writable_payload(self.base, wanted_bytes)?
        else {
            return Ok(false);
        };
        self.capacity = capacity_bytes.saturating_mul(8);
        if new_base != self.base {
            self.base = new_base;
            republish_over(self.src, new_base);
        }
        // `wanted_bytes == ceil(wanted_bits / 8)` and `capacity_bytes >= wanted_bytes`, so
        // this only fails on a saturating multiply; refuse rather than write past the
        // capacity.
        Ok(wanted_bits <= self.capacity)
    }
    /// Whether the byte range `[ptr, ptr + len)` overlaps this payload.
    ///
    /// A segment source that overlaps the accumulator is the one shape that would read
    /// and write the same bytes; the writer promotes to a fresh `Vec` when this answers
    /// `true` (see [`super::BitWriter`]).
    pub(super) fn overlaps(&self, ptr: *const u8, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let payload_start = self.base as usize;
        let payload_end = payload_start + self.capacity.div_ceil(8);
        let src_start = ptr as usize;
        let src_end = src_start + len;
        src_start < payload_end && payload_start < src_end
    }

    /// Append one bit (MSB-first within each byte).
    ///
    /// The caller must have checked [`InPlace::remaining_bits`]. Entering a byte clears
    /// it (or, for the one byte the accumulator's window ends inside, masks its fresh
    /// low bits away while keeping the accumulated high bits), so the bytes this build
    /// writes never inherit the residue of an earlier **failed** attempt at the same
    /// offset — a failure leaves those bytes invisible, but not necessarily zero.
    pub(super) fn push_bit(&mut self, bit: u8) {
        debug_assert!(self.cur < self.capacity, "the caller checked the capacity");
        let byte = self.cur / 8;
        let position = self.cur % 8;
        // SAFETY: `cur < capacity`, so `byte = cur / 8 < ceil(capacity / 8)` — inside the
        // payload, whose `capacity` bytes the machine allocated and keeps alive.
        unsafe {
            if position == 0 {
                // A byte boundary: everything from here on is fresh.
                *self.base.add(byte) = 0;
            } else if self.cur == self.start {
                // The accumulator's window ends inside this byte: keep its bits (the
                // high `position` ones) and clear the fresh low bits first.
                *self.base.add(byte) &= 0xFFu8 << (8 - position);
            }
            if bit & 1 != 0 {
                *self.base.add(byte) |= 1 << (7 - position);
            }
        }
        self.cur += 1;
    }

    /// Bulk-append whole bytes from `src` (the byte-aligned fast path).
    ///
    /// The caller guarantees `src.len() * 8 <= `[`InPlace::remaining_bits`] (checked by
    /// [`super::BitWriter`], which promotes on a shortfall) and that `src` does **not**
    /// overlap the payload ([`InPlace::overlaps`]).
    pub(super) fn copy_bytes(&mut self, src: &[u8]) {
        debug_assert!(
            src.len() * 8 <= self.remaining_bits(),
            "the writer checked the capacity"
        );
        debug_assert!(self.cur % 8 == 0, "the fast path is byte-aligned");
        if src.is_empty() {
            return;
        }
        // SAFETY: the destination starts at byte `cur / 8` and spans `src.len()` bytes,
        // which the capacity check above keeps inside the payload; `src` is a live
        // non-overlapping slice (see the caller's contract), and its validity for
        // `src.len()` reads is a `&[u8]` guarantee.
        unsafe {
            core::ptr::copy_nonoverlapping(
                src.as_ptr(),
                self.base.add(self.cur / 8),
                src.len(),
            );
        }
        self.cur += src.len() * 8;
    }

    /// Copy the accumulated bits out as `(bytes, bit count)` — the **promotion** half
    /// (the build outgrew the payload, or a segment source aliases it).
    ///
    /// The copied bytes are exactly what the copy-rebuild path would have produced for
    /// the same bits: `ceil(bits / 8)` bytes with the unused low bits of the final byte
    /// masked to zero ([`make_binary`]'s canonical invariant), which is also what the
    /// payload itself holds.
    pub(super) fn into_bytes(self) -> (Vec<u8>, usize) {
        let nbits = self.cur;
        let num_bytes = nbits.div_ceil(8);
        let mut out = vec![0u8; num_bytes];
        if num_bytes != 0 {
            // SAFETY: `num_bytes = ceil(cur / 8) <= ceil(capacity / 8)`, so the source
            // range is inside the payload; `out` has room for exactly those bytes and the
            // two allocations are distinct (a fresh `Vec` versus the machine's payload).
            unsafe {
                core::ptr::copy_nonoverlapping(self.base, out.as_mut_ptr(), num_bytes);
            }
            if nbits % 8 != 0 {
                let keep = nbits % 8;
                out[num_bytes - 1] &= 0xFFu8 << (8 - keep);
            }
        }
        (out, nbits)
    }

    /// Publish the build: bump the accumulator's `ErlSubBits.end` to the new position
    /// (`sb->end = new_position`, `erl_bits.c:1975`) and return the **same** term.
    ///
    /// A build that fails after some in-place writes never reaches here, so the window
    /// the caller can still see keeps its old end and the bytes beyond it stay invisible
    /// — the write is simply dropped with the sink.
    pub(super) fn finish(self) -> Eterm {
        debug_assert!(
            self.cur >= self.start && self.cur <= self.capacity,
            "the published window is the written range, inside the payload"
        );
        set_ctx_end(self.src, self.cur);
        self.src
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{ExecutionContext, MinimalContext};

    /// `make_binary` picks the frozen representation: an on-heap `ErlHeapBits` at
    /// the 64-byte limit, an off-heap refc binary above it — and the read path
    /// (`heap_bits`/`subject_bytes`) recovers the same bytes either way.
    #[test]
    fn make_binary_picks_the_frozen_representation() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let heap_bytes: Vec<u8> = (0..64u8).collect();
        let heap = make_binary(&mut machine, &mut ctx, &heap_bytes, 64 * 8).unwrap();
        // SAFETY: `heap` is a live bitstring term built on the line above.
        assert!(!unsafe { beam_term::refc::is_refc_binary(heap) });
        assert_eq!(subject_bytes(heap).unwrap(), (heap_bytes.as_slice(), 0));
        assert_eq!(heap_bits(heap).unwrap().1, 64 * 8);

        let refc_bytes: Vec<u8> = (1..=65u8).collect();
        let refc = make_binary(&mut machine, &mut ctx, &refc_bytes, 65 * 8).unwrap();
        // SAFETY: `refc` is a live bitstring term built on the line above.
        assert!(unsafe { beam_term::refc::is_refc_binary(refc) });
        assert_eq!(subject_bytes(refc).unwrap(), (refc_bytes.as_slice(), 0));
        assert_eq!(heap_bits(refc).unwrap().1, 65 * 8);
        // The generic reader (`terms::binary_bytes`) is refc-aware too.
        assert_eq!(
            crate::terms::binary_bytes(refc).unwrap(),
            refc_bytes.as_slice()
        );
    }

    /// A non-byte-aligned refc result keeps its exact bit count and zeroes the
    /// unused low bits of the final byte.
    #[test]
    fn make_binary_masks_the_trailing_bits_of_a_refc_result() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let bytes = vec![0xFFu8; 65];
        let nbits = 65 * 8 - 5;
        let term = make_binary(&mut machine, &mut ctx, &bytes, nbits).unwrap();
        // SAFETY: `term` is a live bitstring term built on the line above.
        assert!(unsafe { beam_term::refc::is_refc_binary(term) });
        assert_eq!(heap_bits(term).unwrap().1, nbits);
        let (read, off) = subject_bytes(term).unwrap();
        assert_eq!(off, 0, "a full refc binary's window is byte-aligned");
        assert_eq!(read.len(), 65);
        assert_eq!(read[64], 0b1110_0000);
    }

    /// `make_writable` is the 0-bit refc window `bs_init_writable` returns: its
    /// payload is live (so a following append can seed from it) but it covers no
    /// bits.
    #[test]
    fn make_writable_returns_a_zero_bit_refc_window() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let term = make_writable(&mut machine, &mut ctx, 256).unwrap();
        // SAFETY: `term` is a live bitstring term built on the line above.
        assert!(unsafe { beam_term::refc::is_refc_binary(term) });
        let (_, bits, _) = heap_bits(term).unwrap();
        assert_eq!(bits, 0);
        assert_eq!(subject_bytes(term).unwrap(), (&[] as &[u8], 0));
    }

    /// A refc binary whose window starts on a **non-byte-aligned** bit is a readable
    /// subject: `heap_bits` reports its window offset (`start % 8`) and
    /// `subject_bytes` hands out a slice that starts at the byte holding the window's
    /// first bit and covers every window bit.
    ///
    /// The payload bytes `[0b1010_1100, 0b0011_0101, 0b1000_0000]` (MSB-first) with a
    /// window of bits `1..17` (offset 1, size 16) hold `0b0101_1000_0110_1011`.
    #[test]
    fn a_non_byte_aligned_refc_window_is_readable() {
        let mut ctx = MinimalContext::new();
        // A word-aligned payload buffer (the frozen `write_refc_binary` base-alignment
        // contract); the bytes are little-endian in the buffer, so write them byte by
        // byte rather than relying on the word's memory order.
        let mut payload: Vec<Eterm> = vec![Eterm::default(); 2];
        let want_bytes = [0b1010_1100u8, 0b0011_0101u8, 0b1000_0000u8];
        // SAFETY: `payload` holds 2 words (16 bytes) and `want_bytes` is 3 bytes, so
        // the copy is in bounds within `payload`'s own allocation.
        unsafe {
            core::ptr::copy_nonoverlapping(
                want_bytes.as_ptr(),
                payload.as_mut_ptr().cast::<u8>(),
                want_bytes.len(),
            );
        }
        let base = payload.as_ptr().cast::<u8>();
        let val = payload.as_mut_ptr();
        let hp = ctx
            .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
            .expect("the arena hands out 8 words");
        // SAFETY: `hp` points at `ERL_REFC_BITS_SIZE` exclusively-owned words in the
        // context's pointer-stable arena, `val`/`base` point at the live, word-aligned
        // `payload`, and `next` is null. The window is bits `1..17` — deliberately not
        // byte-aligned. The term is used only while `ctx`/`payload` live.
        let refc = unsafe {
            beam_term::refc::write_refc_binary(hp, val, core::ptr::null_mut(), base, 1, 16)
        };
        let (bits, off) = {
            let (_, bits, off) = heap_bits(refc).unwrap();
            (bits, off)
        };
        assert_eq!(bits, 16);
        assert_eq!(off, 1, "the window starts one bit into the first byte");
        let (bytes, read_off) = subject_bytes(refc).unwrap();
        assert_eq!(read_off, 1);
        // `ceil((1 + 16) / 8) == 3` bytes from `base`.
        assert_eq!(bytes, want_bytes.as_slice());
        // Reading the window's 16 bits at `off` reproduces the window bits.
        let mut got = 0u16;
        for i in 0..16usize {
            let bit_index = read_off + i;
            let bit = (bytes[bit_index / 8] >> (7 - (bit_index % 8))) & 1;
            got = (got << 1) | u16::from(bit);
        }
        assert_eq!(got, 0b0101_1000_0110_1011);
    }

    /// A bare off-heap binary reference (`HEADER_BIN_REF`) is not a readable subject:
    /// the family must never read it as raw payload bytes.
    #[test]
    fn a_bare_bin_ref_is_not_a_readable_subject() {
        let mut ctx = MinimalContext::new();
        let hp = ctx
            .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
            .expect("the arena hands out 8 words");
        // SAFETY: `hp` points at `ERL_REFC_BITS_SIZE` words; `write_refc_binary` fully
        // writes the `[BinRef][ErlSubBits]` pair, and `make_boxed(hp)` is the bare
        // `BinRef` term under test (a live block whose header is `HEADER_BIN_REF`).
        let bin_ref = unsafe {
            beam_term::refc::write_refc_binary(
                hp,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                hp.cast::<u8>(),
                0,
                0,
            );
            beam_term::make_boxed(hp)
        };
        assert!(heap_bits(bin_ref).is_none());
        assert!(subject_bytes(bin_ref).is_err());
    }

    /// A machine that has built no refc term enumerates no payload handle: the
    /// accessor a host uses to seed a copy's source off-heap table starts empty.
    #[test]
    fn refc_binaries_is_empty_on_a_fresh_machine() {
        let machine = Machine::new();
        assert!(
            machine.refc_binaries().is_empty(),
            "a fresh machine has built no off-heap payload"
        );
    }

    /// Every off-heap payload the machine builds is **enumerable**: the refc term
    /// `make_binary` returns appears in the accessor, and the exposed handle's
    /// `data_ptr` is exactly the pointer the term's `BinRef.val` word holds — the
    /// lookup key a host's copy resolves the payload by.
    #[test]
    fn a_built_refc_binary_is_enumerable_and_the_handle_is_the_bin_ref_payload() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let bytes: Vec<u8> = (1..=65u8).collect();
        let term = make_binary(&mut machine, &mut ctx, &bytes, 65 * 8).unwrap();
        // SAFETY: `term` is a live bitstring term built on the line above.
        assert!(unsafe { beam_term::refc::is_refc_binary(term) });

        let handles = machine.refc_binaries();
        assert_eq!(handles.len(), 1, "the one refc term the machine built");
        assert_eq!(handles[0].len(), 65);
        assert_eq!(handles[0].as_bytes(), bytes.as_slice());

        // The frozen representation: the boxed root is the `ErlSubBits`, whose `orig`
        // slot (word 4) is the boxed `BinRef` holding the payload pointer in its `val`
        // word (word 1) — `write_refc_binary`'s layout.
        // SAFETY: `term` is a live boxed `ErlSubBits`, so word 4 is the live term
        // `write_refc_binary` wrote there.
        let orig = unsafe { *boxed_val(term).add(4) };
        // SAFETY: `orig` is the live `BinRef` term read out of `orig` above.
        let (val, _next) = unsafe { beam_term::refc::bin_ref_parts(orig) };
        assert_eq!(
            val as *mut u8,
            handles[0].data_ptr(),
            "the term's BinRef.val is the enumerated handle's payload"
        );
    }

    /// Cloning an exposed handle takes a second reference to the payload, so the
    /// payload outlives the machine that built it: dropping the whole machine leaves
    /// the clone's bytes readable.
    #[test]
    fn a_cloned_refc_binary_handle_outlives_the_machine() {
        let handle = {
            let mut machine = Machine::new();
            let mut ctx = MinimalContext::new();
            let _term = make_writable(&mut machine, &mut ctx, 65).unwrap();
            let handles = machine.refc_binaries();
            assert_eq!(handles.len(), 1);
            handles[0].clone()
        }; // the machine is dropped here, releasing its own handle.

        // The writable accumulator's payload is **capacity-carrying** with its whole
        // backing store live (`len == capacity`, the C's `bin_size`): the accumulator
        // covers no *bits* (the term's window is 0 bits), but every byte the payload
        // offers is initialised, because that is what makes a later relocation copy the
        // bytes an appender wrote through the payload pointer.
        assert_eq!(
            handle.len(),
            handle.capacity(),
            "the whole backing store is live"
        );
        assert_eq!(handle.capacity(), 65, "the capacity is the requested hint");
        assert!(
            handle.reserved_bytes() >= handle.capacity(),
            "the payload reserves at least its capacity"
        );
        assert_eq!(
            handle.as_bytes(),
            &[0u8; 65][..],
            "the live bytes are zero-filled, never uninitialised"
        );
        drop(handle);
    }

    // -----------------------------------------------------------------------
    // The capacity-carrying writable payload (`bs_init_writable`) and its growth
    // -----------------------------------------------------------------------

    /// `make_writable` builds a **capacity-carrying** payload: its capacity is the
    /// requested byte count, its reservation covers that capacity (`beam_alloc`'s 4x
    /// policy) and carries the growth slack, its *live* length is that whole capacity
    /// (zero-filled — the initialised backing store a relocation copies), the writable
    /// registry answers that *real* capacity in bytes, and the term is still the frozen
    /// 0-bit refc window over that payload.
    #[test]
    fn make_writable_builds_a_capacity_carrying_live_payload() {
        for hint in [0usize, 1, 8, 256, 1024] {
            let mut machine = Machine::new();
            let mut ctx = MinimalContext::new();
            let term = make_writable(&mut machine, &mut ctx, hint).unwrap();

            let handles = machine.refc_binaries();
            assert_eq!(handles.len(), 1, "one payload backs the accumulator");
            let payload = &handles[0];
            assert_eq!(
                payload.len(),
                payload.capacity(),
                "hint {hint}: the whole backing store is live"
            );
            assert_eq!(payload.capacity(), hint, "hint {hint}: the capacity is the hint");
            assert!(
                payload.reserved_bytes() >= payload.capacity(),
                "hint {hint}: the reservation covers the capacity"
            );
            assert!(
                payload.reserved_bytes() >= hint.saturating_mul(beam_alloc::DEFAULT_RESERVATION_FACTOR)
                    || payload.reserved_bytes() == beam_alloc::MAX_PAYLOAD_BYTES,
                "hint {hint}: the reservation carries the accumulator's growth slack"
            );
            assert!(
                payload.as_bytes().iter().all(|byte| *byte == 0),
                "hint {hint}: the live bytes are zero-filled"
            );
            // The registry is keyed on the payload, and answers its real capacity in bytes.
            assert_eq!(
                machine.writable_capacity(payload.data_ptr()),
                Some(payload.capacity()),
                "hint {hint}: the registry answers the payload's capacity"
            );

            // The term is the frozen 0-bit window over that payload.
            // SAFETY: `term` is the live refc binary `make_writable` just built.
            assert!(unsafe { beam_term::refc::is_refc_binary(term) });
            // SAFETY: as above — a live bitstring term.
            let (base, start, size) = unsafe { beam_term::bitstring_parts(term) };
            assert_eq!(
                base,
                payload.data_ptr() as *const u8,
                "hint {hint}: the window's base is the payload"
            );
            assert_eq!((start, size), (0, 0), "hint {hint}: a 0-bit window");
        }
    }

    /// An in-place grow keeps the payload's address and raises the registry's cached
    /// capacity to what `beam_alloc` committed; a request past the reservation **moves**
    /// the payload and re-keys the registry, leaving no stale key behind.
    #[test]
    fn grow_writable_payload_grows_in_place_then_moves_and_rekeys() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let _term = make_writable(&mut machine, &mut ctx, 8).unwrap();
        let base = machine.refc_binaries()[0].data_ptr();
        let reserved = machine.refc_binaries()[0].reserved_bytes();
        assert!(reserved >= 8, "the reservation covers the hint");

        // (a) Still inside the reservation: same address, raised capacity.
        let committed = reserved.min(20).max(9);
        let (in_place_base, in_place_capacity) = machine
            .grow_writable_payload(base, committed)
            .unwrap()
            .expect("a request inside the reservation grows in place");
        assert_eq!(in_place_base, base, "an in-place grow does not move the payload");
        assert!(
            in_place_capacity >= committed,
            "the committed capacity covers the request"
        );
        assert_eq!(in_place_capacity, machine.refc_binaries()[0].capacity());
        assert_eq!(
            machine.refc_binaries()[0].len(),
            in_place_capacity,
            "the growth keeps the whole (now wider) backing store live"
        );
        assert_eq!(
            machine.writable_capacity(base),
            Some(in_place_capacity),
            "the registry's cached capacity followed the grow"
        );

        // (b) Past the reservation: the payload is reallocated and the registry re-keyed.
        let wanted = reserved + 1024;
        let (moved_base, moved_capacity) = machine
            .grow_writable_payload(base, wanted)
            .unwrap()
            .expect("a request past the reservation still grows");
        assert_ne!(moved_base, base, "the payload was relocated");
        assert!(moved_capacity >= wanted, "the new capacity covers the request");
        assert_eq!(
            machine.refc_binaries()[0].data_ptr(),
            moved_base,
            "the machine's handle names the new block"
        );
        assert_eq!(
            machine.refc_binaries()[0].len(),
            moved_capacity,
            "the relocated payload's whole backing store is live"
        );
        assert_eq!(
            machine.writable_capacity(moved_base),
            Some(moved_capacity),
            "the registry was re-keyed to the new base"
        );
        assert_eq!(
            machine.writable_capacity(base),
            None,
            "no stale key is left naming the freed block"
        );
    }

    /// A zero capacity hint is reproduced faithfully (C's `erl_bits.c:2023` really does
    /// allocate `bin_size = 0`), and such a payload still makes **progress**: growing it
    /// asks for one byte, which exceeds a zero reservation, so `beam_alloc` relocates it to
    /// a `UNIT_SIZE` reservation with a one-byte capacity — never a zero-progress loop.
    #[test]
    fn a_zero_capacity_writable_payload_grows_monotonically() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let _term = make_writable(&mut machine, &mut ctx, 0).unwrap();
        let base = machine.refc_binaries()[0].data_ptr();
        assert_eq!(machine.refc_binaries()[0].capacity(), 0);
        assert_eq!(machine.refc_binaries()[0].len(), 0);
        assert_eq!(machine.refc_binaries()[0].reserved_bytes(), 0);

        let (new_base, capacity) = machine
            .grow_writable_payload(base, 1)
            .unwrap()
            .expect("a zero-capacity payload can still grow");
        assert_ne!(new_base, base, "a zero reservation cannot grow in place");
        assert!(capacity >= 1, "the first growth buys at least the requested byte");
        assert_eq!(machine.refc_binaries()[0].capacity(), capacity);
        assert_eq!(
            machine.refc_binaries()[0].len(),
            capacity,
            "the first growth makes its whole backing store live"
        );
        assert_eq!(machine.writable_capacity(new_base), Some(capacity));
    }

    /// A grow is refused — `Ok(None)`, never an error — for a payload the machine does not
    /// own, for one it never registered as writable, and for a **shared** one (a second
    /// handle exists, exactly C's `erts_pin_writable_binary` condition). The payload and
    /// the registry are unchanged by every refusal.
    #[test]
    fn grow_writable_payload_refuses_what_it_cannot_grow() {
        // (a) An ordinary (non-writable) refc binary: never a growth target.
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let bytes = vec![7u8; 80];
        let _ordinary = make_binary(&mut machine, &mut ctx, &bytes, 640).unwrap();
        let ordinary_base = machine.refc_binaries()[0].data_ptr();
        assert_eq!(
            machine.grow_writable_payload(ordinary_base, 4096).unwrap(),
            None,
            "an ordinary binary's payload keeps its immutability"
        );
        assert_eq!(machine.refc_binaries()[0].capacity(), 80);
        assert_eq!(machine.writable_capacity(ordinary_base), None);

        // (b) A base no handle of this machine names.
        assert_eq!(
            machine.grow_writable_payload(core::ptr::null_mut(), 16).unwrap(),
            None
        );

        // (c) A *shared* writable payload: cloning the handle makes the refcount 2, which
        //     `beam_alloc` refuses to grow (a host adopted the payload).
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let _acc = make_writable(&mut machine, &mut ctx, 8).unwrap();
        let shared_base = machine.refc_binaries()[0].data_ptr();
        let adopted = machine.refc_binaries()[0].clone();
        assert_eq!(machine.refc_binaries()[0].refcount(), 2);
        assert_eq!(
            machine.grow_writable_payload(shared_base, 8192).unwrap(),
            None,
            "a payload another handle shares is never relocated"
        );
        assert_eq!(
            machine.refc_binaries()[0].data_ptr(),
            shared_base,
            "the shared payload did not move"
        );
        assert_eq!(machine.refc_binaries()[0].capacity(), 8, "nor did it grow");
        assert_eq!(
            machine.refc_binaries()[0].len(),
            8,
            "nor did its length move"
        );
        assert_eq!(
            machine.writable_capacity(shared_base),
            Some(8),
            "the registry entry is unchanged"
        );
        drop(adopted);
    }

    /// The **republish**: when the sink's growth relocates the payload, the accumulator's
    /// `[BinRef][ErlSubBits]` pair is rewritten over the new block — same `Eterm`, same
    /// window (`start`/`end`), same bytes — and the registry follows it.
    #[test]
    fn a_relocation_republishes_the_accumulator_pair() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let acc = make_writable(&mut machine, &mut ctx, 8).unwrap();

        // Seed three bytes through the sink (in place: 24 bits of a 64-bit capacity).
        let (base, capacity_bits, end_bits) = writable_append_target(&machine, acc).expect("writable");
        let mut sink = InPlace::new(base, capacity_bits, end_bits, acc);
        sink.copy_bytes(&[1, 2, 3]);
        let acc = sink.finish();
        let size_before = beam_term::size::size_object(acc);

        // Grow well past the 8-byte capacity / 32-byte reservation: the payload moves.
        let (base, capacity_bits, end_bits) = writable_append_target(&machine, acc).expect("writable");
        let mut sink = InPlace::new(base, capacity_bits, end_bits, acc);
        assert!(
            sink.grow(&mut machine, 64 * 8).unwrap(),
            "the sink grew past its reservation"
        );
        assert_ne!(sink.base, base, "the payload was relocated");
        assert!(
            sink.remaining_bits() >= 64 * 8,
            "the refreshed capacity is in bits"
        );
        sink.copy_bytes(&[4, 5, 6]);
        let grown = sink.finish();
        assert_eq!(grown, acc, "the republish keeps the accumulator's term");

        // The pair now names the NEW payload, with the same window start.
        let new_base = machine.refc_binaries()[0].data_ptr();
        assert_ne!(new_base, base);
        // SAFETY: `grown` is the live refc-binary accumulator.
        let (window_base, start, size) = unsafe { beam_term::bitstring_parts(grown) };
        assert_eq!(window_base, new_base as *const u8, "the pair names the new block");
        assert_eq!(start, 0, "the republish preserved the window start");
        assert_eq!(size, 48, "and the written bits");
        // The pair's `orig` slot still points at its own `BinRef`, whose `val` word now
        // carries the new payload address.
        // SAFETY: `grown` is the live refc-binary pair, so its `orig` slot is the live
        // `BinRef` of that same pair.
        let (orig, val) = unsafe {
            let orig = *boxed_val(grown).add(4);
            let (val, _next) = beam_term::refc::bin_ref_parts(orig);
            (orig, val)
        };
        assert_eq!(
            val as *const u8, new_base,
            "the `BinRef.val` word names the new block"
        );
        assert_eq!(
            boxed_val(orig),
            boxed_val(grown).wrapping_sub(beam_term::ERL_BIN_REF_SIZE),
            "and it is still this pair's own `BinRef`, at its fixed offset"
        );
        assert_eq!(
            machine.writable_capacity(new_base),
            Some(machine.refc_binaries()[0].capacity()),
            "the registry followed the move"
        );
        assert_eq!(machine.writable_capacity(base), None, "no stale key");

        // The bytes are the ones written, before and after the move.
        assert_eq!(subject_bytes(grown).unwrap(), (&[1u8, 2, 3, 4, 5, 6][..], 0));
        // The move copied them because the payload's logical length is its whole capacity
        // (`beam_alloc`'s relocation copies exactly `len()` bytes), and the live prefix is
        // the accumulator's written bytes.
        let payload = &machine.refc_binaries()[0];
        assert_eq!(
            payload.len(),
            payload.capacity(),
            "the whole backing store is live, so a relocation copies the written bytes"
        );
        assert_eq!(
            &payload.as_bytes()[..6],
            &[1u8, 2, 3, 4, 5, 6],
            "the relocated payload carries the bytes written before the move"
        );
        // The accumulator's on-heap size is the fixed pair, unchanged by the growth: the
        // payload's capacity and reservation never enter the term's size.
        assert_eq!(beam_term::size::size_object(grown), size_before);
        assert_eq!(size_before, beam_term::refc::ERL_REFC_BITS_SIZE);
    }

    /// A second on-heap pair over the **same** payload (a GC copy, a family-built
    /// `BinRef`, a match context) makes the relocation refuse — the old block would be
    /// freed under it — while the accumulator's **own** pair never blocks its growth.
    #[test]
    fn a_second_pair_over_the_payload_refuses_the_move() {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let acc = make_writable(&mut machine, &mut ctx, 8).unwrap();
        let (base, capacity_bits, _end) = writable_append_target(&machine, acc).expect("writable");

        // A second `[BinRef][ErlSubBits]` pair over the very same payload, parked in a
        // register — the shape a GC copy of the pair leaves behind.
        let hp = reserve(
            &mut machine,
            &mut ctx,
            beam_term::refc::ERL_REFC_BITS_SIZE,
            RootSet::ALL_LIVE,
        )
        .unwrap();
        // SAFETY: `hp` is 8 exclusively-owned words; `base` is the live payload the machine
        // keeps alive; the alias covers a 0-bit window at the payload's start.
        let alias = unsafe {
            beam_term::refc::write_refc_binary(
                hp,
                base.cast::<Eterm>(),
                core::ptr::null_mut(),
                base,
                0,
                0,
            )
        };
        machine.set_x(1, alias).unwrap();

        // `cur == capacity`, so the next bit needs a growth the move would have to serve.
        let mut sink = InPlace::new(base, capacity_bits, capacity_bits, acc);
        assert!(
            !sink.grow(&mut machine, 8).unwrap(),
            "an aliased payload must not be relocated"
        );
        assert_eq!(sink.base, base, "the refusal left the sink on the old block");
        assert_eq!(
            machine.refc_binaries()[0].data_ptr(),
            base,
            "the payload did not move"
        );
        assert_eq!(
            machine.writable_capacity(base),
            Some(8),
            "the registry entry is unchanged"
        );

        // The accumulator's OWN pair anywhere in the root set is not an alias.
        machine.set_x(1, beam_term::NIL).unwrap();
        machine.set_x(0, acc).unwrap();
        let mut sink = InPlace::new(base, capacity_bits, capacity_bits, acc);
        assert!(
            sink.grow(&mut machine, 8).unwrap(),
            "the accumulator's own pair never blocks growth"
        );
        assert_eq!(
            sink.base, base,
            "8 bytes + 8 bits still fits the 32-byte reservation"
        );
    }
    /// A test context whose heap is **one pre-filled block**: every allocation returns
    /// the same `words`-word buffer, every byte of which already holds `0xFF`.
    ///
    /// A build then writes into memory that is *dirty* everywhere — the only way to
    /// observe the padding a builder must zero, because a fresh arena is zero-filled
    /// by construction and would hide the omission.
    struct DirtyHeap {
        block: Vec<Eterm>,
    }

    impl DirtyHeap {
        fn new(words: usize) -> Self {
            Self {
                block: vec![Eterm::from_raw(usize::MAX); words],
            }
        }
    }

    impl ExecutionContext for DirtyHeap {
        fn call_bif(
            &mut self,
            module: &str,
            function: &str,
            arity: u32,
            _args: &[Eterm],
        ) -> Result<Eterm> {
            Err(error::unsupported(&format!(
                "dirty heap: no BIF {module}:{function}/{arity}"
            )))
        }

        fn ensure_heap(
            &mut self,
            _heap_need: u32,
            _live: u32,
            _x_regs: &mut [Eterm],
            _stack: &mut [Eterm],
        ) -> Result<()> {
            Ok(())
        }

        fn alloc_words(&mut self, _n: usize) -> Result<*mut Eterm> {
            Ok(self.block.as_mut_ptr())
        }
    }

    /// `make_bits` initialises the **whole** data region of the `ErlHeapBits` block it
    /// builds — including the padding bytes of the last data word, which are not part
    /// of the payload.
    ///
    /// That is the invariant the LCA writer (`beam_term::bitstring::write_heap_bits`)
    /// establishes by zeroing the data region before the payload copy. A builder that
    /// copied only the payload bytes would leave whatever the heap held there: the
    /// context's block is pre-filled with `0xFF`, so the omission is observable, and a
    /// GC word-copy of the block would propagate the garbage.
    #[test]
    fn make_bits_zeroes_the_padding_of_the_last_data_word() {
        // A 1-bit and a 5-bit result both occupy one payload byte of a one-word data
        // region, so 7 of the data word's 8 bytes are padding.
        for nbits in [1usize, 5] {
            let num_bytes = nbits.div_ceil(8);
            // The word count and the data-region offset both come from `beam_term`
            // (the frozen-layout owner), so this test asserts the invariant without
            // re-deriving the layout either.
            let num_words = beam_term::bitstring::heap_bits_size_words(nbits);
            assert_eq!(num_words, 3, "the payload fits one data word");
            let mut ctx = DirtyHeap::new(num_words);
            let mut machine = Machine::new();
            // All-ones source: only the trailing-bit mask decides the payload byte, so
            // the payload assertion cannot pass by accident.
            let source = [0xFFu8];
            let term = make_bits(&mut machine, &mut ctx, &source, nbits).unwrap();

            // SAFETY: `term` is the live boxed `ErlHeapBits` `make_bits` just built in
            // `ctx`'s block, whose data region starts at word `ERL_HEAP_BITS_SIZE` and is
            // `num_words - ERL_HEAP_BITS_SIZE` words wide.
            let data = unsafe { boxed_val(term).add(beam_term::ERL_HEAP_BITS_SIZE).cast::<u8>() };
            let data_bytes =
                (num_words - beam_term::ERL_HEAP_BITS_SIZE) * core::mem::size_of::<Eterm>();

            // The payload byte keeps exactly the leading `nbits` bits ...
            // SAFETY: byte 0 of the freshly built `num_bytes`-byte payload.
            let payload = unsafe { *data };
            assert_eq!(
                payload,
                0xFFu8 << (8 - nbits),
                "the unused low bits of the final payload byte are cleared (nbits = {nbits})"
            );
            // ... and every trailing padding byte of the data word is zero.
            for i in num_bytes..data_bytes {
                // SAFETY: `i < data_bytes`, so the byte is inside the freshly built
                // data region.
                let padding = unsafe { *data.add(i) };
                assert_eq!(
                    padding, 0,
                    "padding byte {i} of the data word must be zero (nbits = {nbits})"
                );
            }
        }
    }
}