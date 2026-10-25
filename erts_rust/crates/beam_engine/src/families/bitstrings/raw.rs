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
///
/// `pub(crate)` on purpose: a later JIT round inlining a match opcode needs the same
/// subject triple the family reads (see `matching::match_ctx`'s seam note); the
/// semantics are unchanged by the visibility.
#[inline]
pub(crate) fn heap_bits(t: Eterm) -> Option<(*const u8, usize, usize)> {
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
#[inline]
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
///
/// In this engine a non-refc `ErlSubBits` is **always** a match context (see
/// [`heap_bits`]), so `ctx_parts(t).is_some() && !is_refc_binary(t)` is exactly C's
/// `erl_sub_bits_is_match_context` (`erl_bits.h:107-108`) — the predicate
/// `bs_start_match3`'s `reuse` arm tests before it re-uses the term untouched.
///
/// `pub(crate)` on purpose: see [`heap_bits`].
#[inline]
pub(crate) fn ctx_parts(t: Eterm) -> Option<(Eterm, usize, usize)> {
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

/// `erlang:bit_size/1` — the bit size of a bitstring **or** of a live match context,
/// or `None` when `t` is neither.
///
/// A heap-bits/binary subject (`ErlHeapBits`, on-heap binary) takes its `size` word; a
/// refc-binary window and an `ErlSubBits` **match context** both take their remaining
/// window `end - start`, which is exactly what `erts_bin_bits_size` (`erl_bits.h`) does
/// for every `SUB_BITS` thing. A bare off-heap `BinRef` (a reference no
/// `bs_start_match` has wrapped yet) and every non-bitstring are `None`, so a caller
/// turns that into `badarg` rather than a substituted size.
///
/// This is the host-facing reader the BIF floors need (a BIF cannot reach this
/// module's `pub(super)` family readers); the family's own readers keep their narrower
/// [`heap_bits`]/[`ctx_parts`] shapes.
#[allow(dead_code)]
pub(crate) fn bit_size(t: Eterm) -> Option<usize> {
    if let Some((_, bits, _)) = heap_bits(t) {
        return Some(bits);
    }
    ctx_parts(t).map(|(_, start, end)| end - start)
}

/// The inner subject (`orig`, word 4) of a match-context term, or `None` when `t`
/// is not one.
///
/// Used by the `bs_get_position` over-large-position fallback, which re-derives the
/// snapshot's `orig` after re-reading the context's operand so the snapshot keeps the
/// inner subject the C keeps — never the outer context.
#[inline]
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
///
/// `pub(crate)` on purpose: see [`heap_bits`].
#[inline]
pub(crate) fn set_ctx_start(t: Eterm, start: usize) {
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
#[inline]
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
#[inline]
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
/// address after a relocation, preserving its window (`start`/`end`), the flag bits of
/// `ErlSubBits.base_flags` and its `BinRef.next` chain word.
///
/// # The two blocks are independent writers
///
/// Only the two words that name the payload change — `BinRef.val` (word 1 of the
/// `[HEADER_BIN_REF, val, next]` block) and the `ErlSubBits.base_flags` word (word 1 of the
/// `[thing_word, base_flags, start, end, orig]` block, which *is* the payload pointer,
/// `erl_bits.h:95`) — and nothing else is written.
///
/// That is not an optimisation: the two blocks are **not** one contiguous allocation and
/// may sit at unrelated addresses. `beam_gc` copies an `ErlSubBits` as its own
/// [`ERL_SUB_BITS_SIZE`] words and forwards its `orig` word, so the 3-word `BinRef` is
/// copied as that block's **child**, i.e. as a separate allocation the collector may place
/// anywhere in the to-space (`beam_gc/src/copy.rs`'s `move_size`; pinned by
/// `beam_gc/tests/stress_cross_generation.rs`). A collection between two appends therefore
/// separates them, and a whole-pair rewrite from the `BinRef`'s address would write five
/// words past the end of a 3-word block.
///
/// `orig` is derived **fresh** from `src` (never cached across an admission point), because
/// a relocated `BinRef` has a new address too.
///
/// The pair's *root* address and the returned `Eterm` are unchanged: every alias of the
/// *term* (the same `Eterm` in a register, a Y slot, a copied pair) reads the new payload.
///
/// # Why this is not delegated to `beam_term`
///
/// `beam_term::refc::write_refc_binary` writes the whole 8-word pair at one address, and
/// nothing upstream exposes a single-word writer for `BinRef.val` or
/// `ErlSubBits.base_flags`. Writing the two frozen words here — in the bitstring family's
/// single raw-heap accessor module — is the local form of that missing setter; the wanted
/// `beam_term` API is `refc::set_bin_ref_val(t, new_base)` +
/// `bitstring::set_sub_bits_base_flags(t, new_base)` (or one
/// `refc::republish_payload(pair_root, new_base)` doing exactly these two words).
fn republish_over(src: Eterm, new_base: *mut u8) {
    let new_payload = new_base as usize;
    // SAFETY: `src` is the caller's live refc-binary pair root, so it is a live
    // `ErlSubBits`: `sub_bits_parts` reads its words 1..4, which `write_refc_binary`
    // initialised. Its `orig` word is the live `BinRef` of that pair (the caller reached
    // here through `writable_append_target`, whose `is_refc_binary` test is exactly
    // "`ErlSubBits` whose `orig` is a `BinRef`"), so word 0 is `HEADER_BIN_REF` and words
    // 1/2 are initialised. Both blocks are kept alive by the caller for the whole call (an
    // `ErlSubBits` on the machine's heap, and its `BinRef` block, which nothing frees in
    // between). `new_base` is the payload the machine's own `beam_alloc` handle now names
    // — `UNIT_SIZE`-aligned by the allocator's payload contract, so its low
    // `ERL_SUB_BITS_FLAG_MASK` bits are clear and ORing the preserved flag bits back in is
    // exact. The relocation copies the live bytes and never changes the window, so
    // `start`/`end`/`orig` are left byte-identical.
    unsafe {
        let (base_flags, start_before, end_before, orig) =
            beam_term::bitstring::sub_bits_parts(src);
        let flags = base_flags.raw() & beam_term::bitstring::ERL_SUB_BITS_FLAG_MASK;
        let root_block = boxed_val(src);
        let bin_ref_block = boxed_val(orig);
        let (_val_before, next_before) = beam_term::refc::bin_ref_parts(orig);

        // Writer 1 — the `BinRef` block: `val` (word 1) is the payload's first data byte.
        // `next` (word 2) is the off-heap chain link and is deliberately NOT written.
        *bin_ref_block.add(1) = Eterm::from_raw(new_payload);

        // Writer 2 — the `ErlSubBits` block: `base_flags` (word 1) is the payload pointer
        // with the two flag bits in its low `ERL_SUB_BITS_FLAG_MASK` bits. `start`/`end`/
        // `orig` (words 2/3/4) are deliberately NOT written: a payload relocation changes
        // neither the window nor the back-pointer to the `BinRef`.
        *root_block.add(beam_term::SUB_BITS_BASE_WORD) = Eterm::from_raw(new_payload | flags);

        // The invariant that actually holds. When the pair *is* still contiguous these two
        // writes are byte-identical to the old whole-pair form for the words it touched;
        // the words it no longer rewrites (`next`, `start`, `end`, `orig`) are the values
        // already in place, which is what the last two assertions pin.
        let (_base_flags_after, start_after, end_after, orig_after) =
            beam_term::bitstring::sub_bits_parts(src);
        let (val_after, next_after) = beam_term::refc::bin_ref_parts(orig_after);
        debug_assert_eq!(val_after as *const u8, new_base, "`BinRef.val` names the payload");
        debug_assert_eq!(
            (_base_flags_after.raw() & !beam_term::bitstring::ERL_SUB_BITS_FLAG_MASK)
                as *const u8,
            new_base,
            "`ErlSubBits.base_flags` names the payload"
        );
        debug_assert_eq!(
            _base_flags_after.raw() & beam_term::bitstring::ERL_SUB_BITS_FLAG_MASK,
            flags,
            "the flag bits are preserved"
        );
        debug_assert_eq!(boxed_val(src), root_block, "the root keeps its address");
        debug_assert_eq!(
            boxed_val(orig_after),
            bin_ref_block,
            "the pair keeps its own `BinRef`, wherever it now lives"
        );
        debug_assert_eq!(
            (start_after.raw(), end_after.raw()),
            (start_before.raw(), end_before.raw()),
            "a relocation never changes the window"
        );
        debug_assert_eq!(next_after, next_before, "`BinRef.next` is not rewritten");
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
    #[inline]
    pub(super) fn bit_len(&self) -> usize {
        self.cur
    }

    /// How many more bits fit in the payload.
    #[inline]
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
    /// rewritten over the new address ([`republish_over`]): exactly two words move — the
    /// `BinRef.val` word and the `ErlSubBits.base_flags` pointer word (flag bits
    /// preserved). The pair's *root* address, its `start`/`end` window, its `BinRef.next`
    /// chain word, its `orig` back-pointer and the returned `Eterm` (it is `self.src`,
    /// untouched) do not. The two blocks are **not** assumed to be adjacent: a collection
    /// copies the `ErlSubBits` root and its `orig` `BinRef` as independent allocations, so
    /// the words between them belong to no one in particular. `self.base` follows, and
    /// `self.capacity` is re-read from the bytes `beam_alloc` reports, so the invariant
    /// below holds for the widened payload too.
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
    /// more this method establishes: (a) both blocks the republish writes are the
    /// accumulator's own — its `ErlSubBits` root and the `BinRef` that root's `orig` word
    /// names are the two blocks `InPlace` was built for, so the write touches no other
    /// term's words (they need not be adjacent, and [`republish_over`] never assumes they
    /// are), and (b) no **other** on-heap pair names the old payload
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
    #[inline]
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
    #[inline]
    pub(super) fn push_bit(&mut self, bit: u8) {
        debug_assert!(self.cur < self.capacity, "the caller checked the capacity");
        write_payload_bit(self.base, self.start, self.cur, bit);
        self.cur += 1;
    }

    /// Bulk-append whole bytes from `src` (the byte-aligned fast path).
    ///
    /// The caller guarantees `src.len() * 8 <= `[`InPlace::remaining_bits`] (checked by
    /// [`super::BitWriter`], which promotes on a shortfall) and that `src` does **not**
    /// overlap the payload ([`InPlace::overlaps`]).
    #[inline]
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

    /// Store a **fixed-width** big- or little-endian integer field at the current
    /// position in one native store.
    ///
    /// This is the writable-accumulator half of the prepared `bs_create_bin`
    /// **byte-field** write (`BitWriter::push_byte_field`): the field is a whole number
    /// of bytes (`nbytes` in `1 | 2 | 4 | 8`) written to a byte-aligned position, so the
    /// bytes are exactly what the bit-granular `push_bit` loop produces for the same
    /// value masked into the field width (the truncating cast *is* that mask) — but
    /// without a `push_bit` per bit and without the dynamic-length copy
    /// [`InPlace::copy_bytes`] has to make.
    ///
    /// The caller guarantees `self.cur % 8 == 0` and `nbytes * 8 <= remaining_bits()`
    /// (its own capacity check, or the armed per-instruction reservation, which is the
    /// same bound), so every byte written is inside the payload.
    #[inline]
    pub(super) fn write_byte_field(&mut self, value: u64, nbytes: usize, little: bool) {
        debug_assert!(self.cur % 8 == 0, "the byte field is byte-aligned");
        debug_assert!(matches!(nbytes, 1 | 2 | 4 | 8), "a specialised field width");
        debug_assert!(
            nbytes * 8 <= self.remaining_bits(),
            "the caller checked the capacity"
        );
        // SAFETY: the destination byte `cur / 8` starts at `cur`, and the check above
        // keeps `cur + nbytes * 8` inside `capacity`, so the `nbytes` bytes written are
        // inside the payload the machine allocated and keeps alive for this window.
        // `write_unaligned` because a payload byte address carries no alignment
        // guarantee; every byte store is a plain `u8` store.
        unsafe {
            let dst = self.base.add(self.cur / 8);
            match nbytes {
                1 => *dst = value as u8,
                2 => core::ptr::write_unaligned(
                    dst.cast::<u16>(),
                    if little {
                        (value as u16).to_le()
                    } else {
                        (value as u16).to_be()
                    },
                ),
                4 => core::ptr::write_unaligned(
                    dst.cast::<u32>(),
                    if little {
                        (value as u32).to_le()
                    } else {
                        (value as u32).to_be()
                    },
                ),
                _ => core::ptr::write_unaligned(
                    dst.cast::<u64>(),
                    if little { value.to_le() } else { value.to_be() },
                ),
            }
        }
        self.cur += nbytes * 8;
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

// ---------------------------------------------------------------------------
// The bit-granular write rule — this node's ONE definition of it
// ---------------------------------------------------------------------------

/// Write **one bit** into the payload byte holding absolute bit `cur_bit`, MSB-first,
/// applying the residue-clearing rule every bit-granular write in this node shares.
///
/// The rule, in the three cases it has:
///
/// * a write that **enters** a byte (`cur_bit % 8 == 0`) clears that byte first, so the
///   bits this build writes never inherit what an earlier **failed** attempt at the same
///   offset left behind (a failure leaves those bytes invisible, but not necessarily
///   zero);
/// * a write at the accumulator's own window end (`cur_bit == start_bit`) that lands
///   *inside* a byte masks the fresh low bits away, keeping the window's high
///   `cur_bit % 8` bits;
/// * the bit itself is OR-ed in at `7 - cur_bit % 8`.
///
/// The three absolute cases make a re-write at the same position produce the same byte,
/// which is what lets a lowering re-execute a parked instruction.
///
/// # Contract
///
/// `base` must be the first data byte of a live, writable payload whose capacity covers
/// `cur_bit` (`cur_bit < capacity_bits`), and `start_bit <= cur_bit`. Both `InPlace::push_bit`
/// (over its own `base`/`start`/`cur`, with `cur < capacity`) and
/// [`write_bit_field`] (over a capacity a run-time door verified for the whole plan)
/// establish exactly that.
#[inline]
fn write_payload_bit(base: *mut u8, start_bit: usize, cur_bit: usize, bit: u8) {
    let byte = cur_bit / 8;
    let position = cur_bit % 8;
    // SAFETY: the caller guarantees `cur_bit < capacity_bits`, so `byte = cur_bit / 8` is
    // inside the payload the owning machine allocated and keeps alive, and `base` is that
    // payload's first data byte (`writable_append_target`). All three accesses land on
    // that one byte.
    unsafe {
        if position == 0 {
            *base.add(byte) = 0;
        } else if cur_bit == start_bit {
            *base.add(byte) &= 0xFFu8 << (8 - position);
        }
        if bit & 1 != 0 {
            *base.add(byte) |= 1 << (7 - position);
        }
    }
}

/// Write the low `nbits` bits of `value`, **MSB-first**, into the payload starting at
/// absolute bit `cur_bit` — one [`write_payload_bit`] per bit, so a bit-granular field and
/// the family's own [`InPlace::push_bit`] loop cannot disagree about the bytes.
///
/// This is the body behind `create::seam::write_bit_field`, the safe door a native
/// lowering calls; it is the same rule `BitWriter`'s bit loop runs
/// ([`BitWriter::push_uint`](super::BitWriter)) because both end up here.
///
/// # Contract
///
/// * every bit in `[cur_bit, cur_bit + nbits)` is already verified inside the payload's
///   capacity (`cur_bit + nbits <= capacity_bits`) and `start_bit <= cur_bit` — the room a
///   run-time door checked for the whole plan;
/// * `nbits <= 64`;
/// * writes only the payload bytes the field spans: no term access, no `Machine`, no
///   allocation.
// The seam is the bitstring family's **exported** surface for a native lowering, and
// nothing in a plain `--lib` build refers to the bit-field door yet (see `create/mod.rs`'s
// `mod seam`): the reachability root is one level up, so the lint would fire here first.
// The door is deliberately part of the contract, not dead weight.
#[allow(dead_code)]
#[inline]
pub(crate) fn write_bit_field(
    base: *mut u8,
    start_bit: usize,
    cur_bit: usize,
    value: u64,
    nbits: u32,
) {
    debug_assert!(nbits <= 64, "a machine-word field");
    for k in (0..nbits).rev() {
        write_payload_bit(
            base,
            start_bit,
            cur_bit + (nbits - 1 - k) as usize,
            ((value >> k) & 1) as u8,
        );
    }
}
#[cfg(test)]
#[path = "raw_tests.rs"]
mod tests;
