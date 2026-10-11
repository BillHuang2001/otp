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
//! allocating a fresh match context, and materialising a result bitstring with an
//! explicit bit count. Every `unsafe` block carries a `// SAFETY:` rationale; every
//! caller hands in a term this module (or `beam_term`) already validated.
//!
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

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::machine::Machine;
use crate::terms::{reserve, Slack};

/// `ERL_ONHEAP_BINARY_LIMIT` (`erl_bits.h:167`): the largest byte count a hardware
/// *heap* binary holds; a larger result becomes the off-heap ref-counted binary
/// [`make_binary`] builds ([`make_writable`] for the `bs_init_writable` accumulator).
const ERL_ONHEAP_BINARY_LIMIT: usize = 64;

/// The number of bytes in one heap word (`sizeof(Eterm)`).
const WORD_BYTES: usize = core::mem::size_of::<Eterm>();

/// The two fixed head words of an `ErlHeapBits` (`thing_word`, `size`).
const HEAP_BITS_HEAD_WORDS: usize = 2;

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
    // SAFETY: an `ErlHeapBits` is `[thing_word, size, data…]`; word 1 is the bit
    // count and word 2 begins the data.
    let bits = unsafe { *p.add(1) }.raw();
    // SAFETY: as above — a live heap-bits block stores its payload at word 2.
    let data = unsafe { p.add(HEAP_BITS_HEAD_WORDS).cast::<u8>() };
    Some((data as *const u8, bits, 0))
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
/// of `bytes` (MSB-first), zeroing the unused trailing bits of the final byte.
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
    let num_words = HEAP_BITS_HEAD_WORDS + num_bytes.div_ceil(WORD_BYTES);
    let hp = reserve(machine, ctx, num_words, RootSet::ALL_LIVE)?;
    // SAFETY: the rooted reserve returned `2 + ceil(num_bytes / word_bytes)`
    // exclusively-owned, writable words. Word 0 takes the heap-bits header, word 1
    // the bit count, and the data region at word 2 has room for `num_bytes` bytes.
    // (`bytes` is a Rust-owned buffer — never a live term's storage — so the reserve
    // cannot invalidate it.)
    unsafe {
        *hp = beam_term::make_header(num_words - 1, beam_term::HEAP_BITS_SUBTAG);
        *hp.add(1) = Eterm::from_raw(nbits);
        let data = hp.add(HEAP_BITS_HEAD_WORDS).cast::<u8>();
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, num_bytes);
        if num_bytes != 0 && nbits % 8 != 0 {
            // The source's trailing bits in the final partial byte are unspecified
            // (it is a `ceil`-rounded copy), so mask them to zero — the canonical
            // "unused low bits are zero" bitstring invariant.
            let keep = nbits % 8;
            let mask = 0xFFu8 << (8 - keep);
            *data.add(num_bytes - 1) &= mask;
        }
    }
    Ok(beam_term::make_boxed(hp))
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
/// The 8-word `[BinRef][ErlSubBits]` pair is reserved **rooted** first, then the
/// off-heap payload is allocated and the pair written (the frozen
/// reserve-then-write ordering; the payload is off-heap and never moves).
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
    let data = machine.alloc_refc_binary(&vec![0u8; capacity])?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::MinimalContext;

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

        assert_eq!(handle.len(), 65);
        assert_eq!(handle.as_bytes(), vec![0u8; 65].as_slice());
    }
}