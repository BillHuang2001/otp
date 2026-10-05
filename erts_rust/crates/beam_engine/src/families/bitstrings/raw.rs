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
//!   HEAP_BITS_SUBTAG)`, i.e. `HEAP_BITS_SUBTAG` after masking — the M1 engine only
//!   ever *builds* heap bitstrings (off-heap ref-counted binaries are a documented
//!   gap), so any other bitstring header is rejected here rather than guessed at.
//! * `ErlSubBits` is the match-context form: word 2 is the current **read
//!   position** in bits and word 3 is the subject size in bits, so `end - start`
//!   is exactly `bitstring_size/1` for a context (`erl_bits.h:52-56`). The engine
//!   keeps `base_flags` at 0 (the C `ERL_SUB_BITS_FLAG_MUTABLE` bit only gates the
//!   native match-context reuse path, which the interpreter reproduces by re-reading
//!   the term each opcode instead).
//!
//! `orig` is the subject term (an on-heap byte binary), kept so the context stays
//! self-contained and its byte slice can be re-derived at every opcode.
#![allow(unsafe_code)]

use beam_common::Result;
use beam_term::{boxed_val, Eterm, ERL_SUB_BITS_SIZE, HEADER_BIN_REF, HEADER_SUB_BITS};

use crate::context::ExecutionContext;
use crate::error;

/// `ERL_ONHEAP_BINARY_LIMIT` (`erl_bits.h:167`): the largest byte count a hardware
/// *heap* binary holds; a larger result needs the off-heap ref-counted binary this
/// milestone does not build.
const ERL_ONHEAP_BINARY_LIMIT: usize = 64;

/// The number of bytes in one heap word (`sizeof(Eterm)`).
const WORD_BYTES: usize = core::mem::size_of::<Eterm>();

/// The two fixed head words of an `ErlHeapBits` (`thing_word`, `size`).
const HEAP_BITS_HEAD_WORDS: usize = 2;

/// The data pointer and **bit** size of an on-heap bitstring term.
///
/// Returns `None` for anything that is not a heap bitstring: a non-boxed term, a
/// sub-bitstring/match context ([`HEADER_SUB_BITS`]) or an off-heap binary
/// reference ([`HEADER_BIN_REF`]). Callers turn `None` into their own failure
/// (a `bs_start_match3` match failure, or a typed `unsupported`).
pub(super) fn heap_bits(t: Eterm) -> Option<(*const u8, usize)> {
    // SAFETY: `is_bitstring` checks `is_boxed` before it dereferences the header.
    if !unsafe { beam_term::is_bitstring(t) } {
        return None;
    }
    let p = boxed_val(t);
    // SAFETY: `is_bitstring` established that `t` is a live bitstring block, so
    // word 0 is its header.
    let hdr = unsafe { *p };
    if hdr == HEADER_SUB_BITS || hdr == HEADER_BIN_REF {
        return None;
    }
    // SAFETY: an `ErlHeapBits` is `[thing_word, size, data…]`; word 1 is the bit
    // count and word 2 begins the data.
    let bits = unsafe { *p.add(1) }.raw();
    // SAFETY: as above — a live heap-bits block stores its payload at word 2.
    let data = unsafe { p.add(HEAP_BITS_HEAD_WORDS).cast::<u8>() };
    Some((data as *const u8, bits))
}

/// The byte payload of an on-heap bitstring term: `ceil(bits / 8)` bytes.
///
/// A non-heap-bitstring subject (a match context, an off-heap reference, a
/// non-bitstring) is the documented M1 gap and yields
/// [`crate::error::unsupported`]. The returned slice borrows the subject's
/// storage, so the caller must keep the owning heap alive and unmoved for as long
/// as it is used (the [`ExecutionContext`] ownership contract).
pub(super) fn subject_bytes<'a>(t: Eterm) -> Result<&'a [u8]> {
    let (ptr, bits) = heap_bits(t)
        .ok_or_else(|| error::unsupported("bitstring: subject is not an on-heap binary"))?;
    let num_bytes = bits.div_ceil(8);
    if num_bytes == 0 {
        return Ok(&[]);
    }
    // SAFETY: a live heap-bits block owns `2 + ceil(num_bytes / word_bytes)` words,
    // so the `num_bytes` data bytes starting at word 2 are in bounds and readable;
    // the caller keeps the owning heap alive and unmoved for `'a`.
    Ok(unsafe { core::slice::from_raw_parts(ptr, num_bytes) })
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

/// Build a fresh match-context term over `orig` positioned at bit `start` with
/// subject size `end` bits.
///
/// The five words are `ERL_SUB_BITS_SIZE` (`erl_bits.h:128`), matching the C
/// `HEADER_SUB_BITS` block layout exactly; `base_flags` stays 0 (see the module
/// docs). The pointer is valid until the next `ensure_heap` (GC).
pub(super) fn make_ctx(
    ctx: &mut dyn ExecutionContext,
    orig: Eterm,
    start: usize,
    end: usize,
) -> Result<Eterm> {
    let hp = ctx.alloc_words(ERL_SUB_BITS_SIZE)?;
    // SAFETY: `alloc_words` returned exactly `ERL_SUB_BITS_SIZE` (5)
    // exclusively-owned, writable words; every write below is in bounds and the
    // header is written before the block is turned into a term.
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
/// `BIG_NEED_SIZE(n) = n + 1`). The block is allocated through the
/// [`ExecutionContext`] (`alloc_words`), so the term stays valid until the next
/// `ensure_heap`/GC.
pub(super) fn make_big(
    ctx: &mut dyn ExecutionContext,
    negative: bool,
    digits: &[u64],
) -> Result<Eterm> {
    if digits.is_empty() {
        // An internal invariant (the caller normalises zero to a small); a
        // zero-width bignum would be a wrong result, so surface it.
        return Err(error::unsupported("bitstring: zero-width bignum"));
    }
    let hp = ctx.alloc_words(beam_term::big_size_words(digits.len()))?;
    // SAFETY: `alloc_words` returned exactly `big_size_words(n) = n + 1`
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

/// Build an on-heap bitstring of exactly `nbits` bits from the leading bits of
/// `bytes` (MSB-first), zeroing the unused trailing bits of the final byte.
///
/// `bytes` must hold at least `ceil(nbits / 8)` bytes; a longer slice is fine (the
/// surplus is ignored). More than [`ERL_ONHEAP_BINARY_LIMIT`] bytes needs the
/// off-heap representation and yields [`crate::error::unsupported`].
pub(super) fn make_bits(
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
    let hp = ctx.alloc_words(num_words)?;
    // SAFETY: `alloc_words` returned `2 + ceil(num_bytes / word_bytes)`
    // exclusively-owned, writable words. Word 0 takes the heap-bits header, word 1
    // the bit count, and the data region at word 2 has room for `num_bytes` bytes.
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
