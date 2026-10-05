//! Bitstring term block (`ErlHeapBits` / `ErlSubBits`) — the **local** word-layout
//! shim.
//!
//! # Why this lives here (deliberate, temporary deviation)
//!
//! The frozen term layout is owned by `beam_term` (crate-layer `CONTEXT.md` §2,
//! "No duplication across crates"). The bitstring block is the one boxed layout
//! `beam_term` does **not** yet materialize — it exposes every primitive this shim
//! needs (`HEAP_BITS_SUBTAG`, `SUB_BITS_SUBTAG`, `HEADER_SUB_BITS`, `make_header`,
//! `make_boxed`, `boxed_val`, `is_bitstring`, `bitstring_size`) but no writer and no
//! base/offset reader.
//!
//! `beam_codec`'s node may not write the sibling `beam_term`, so the (small) layout
//! shim is implemented here instead of being added to `beam_term::thing`. It is
//! built **only** from those public constants/primitives (no re-defined tag values),
//! so the duplication is confined to the word arithmetic that `beam_term` will own
//! once the binary constructor lands there.
//!
//! TODO(extract to beam_term::thing): move `make_bitstring` / `bitstring_parts` /
//! `bitstring_bytes` / `heap_bits_size_words` into `beam_term::thing` (next to
//! `write_float`/`write_big`) and forward to them from here, exactly as this crate's
//! float/bignum/flatmap/external-identifier helpers already do. Escalated to the
//! parent node (it owns the crate-layer dependency graph).
//!
//! # The layout (ground truth `erts/emulator/beam/erl_bits.h:150-164`, `:59-77`)
//!
//! * `ErlHeapBits` = `[thing_word, n_bits, data…]` where
//!   `thing_word = _make_header(heap_bits_size(n_bits) - 1, HEAP_BITS_SUBTAG)` and
//!   `heap_bits_size(n_bits) = 2 + ceil(ceil(n_bits / 8) / sizeof(Eterm))`
//!   (`heap_bin_size__(NBYTES(n_bits))`). The root is `make_bitstring(hp)` ==
//!   `make_boxed(hp)`. The data words hold the payload bytes **little-endian**
//!   (byte 0 at the lowest address of word 2 — a plain byte copy into `hp + 2`);
//!   the unused trailing bits of the last byte are zero, and the padding bytes of
//!   the trailing data word are zero as well (the C `memcpy` leaves them
//!   uninitialised, which is harmless there but unrepresentable in safe Rust).
//! * `ErlSubBits` = `[HEADER_SUB_BITS, base_flags, start, end, orig]`: `base_flags`
//!   carries the base pointer with its low 2 bits used as flags
//!   (`ERL_SUB_BITS_FLAG_MASK`, `erl_bits.h:95,124-125`), `start`/`end` are **bit**
//!   offsets into that base and the size is `end - start`.
//!
//! Bit order is **MSB-first inside each byte**, exactly as the C copies it, so
//! `<<1:3>>` is the single byte `0b0010_0000` with `n_bits = 3`; no bit shifting is
//! required to materialize or read a bitstring — only a byte copy plus zero-fill.
//!
//! # `unsafe` contract
//!
//! This module is inside the crate's single `#[allow(unsafe_code)]` subtree
//! (`mod term;` in `lib.rs`). Every `unsafe` block writes into words `beam_alloc`
//! just handed out, or reads words a live `Eterm` points at, and carries a
//! `// SAFETY:` rationale. The public functions are *safe*: a wrong kind of term
//! yields [`beam_common::BeamError::BadArg`] and a corrupt block is rejected by its
//! self-describing header, never by undefined behaviour.

use core::mem::size_of;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{
    arityval, boxed_val, is_bitstring, make_boxed, make_header, Eterm, HEADER_SUB_BITS,
    HEAP_BITS_SUBTAG,
};

use crate::error;

/// `ERL_SUB_BITS_FLAG_MASK` (`erl_bits.h:95`): the low two bits of an
/// `ErlSubBits::base_flags` word carry the match-context / volatile flags, so the
/// base pointer must be masked with `!ERL_SUB_BITS_FLAG_MASK` before use
/// (`erl_sub_bits_get_base`, `erl_bits.h:124-125`).
pub const BITSTRING_POINTER_FLAG_MASK: usize = 0x3;

/// Number of bytes in one term word (`size_of::<Eterm>()`, 8 on the frozen
/// `ARCH_64` layout).
const WORD_BYTES: usize = size_of::<Eterm>();

/// `ErlHeapBits` head size in words (`erl_bits.h:150-153`):
/// `thing_word`, `size` — the data follows.
const ERL_HEAP_BITS_HEAD_WORDS: usize = 2;

/// Word index of `ErlSubBits::base_flags`.
const SUB_BITS_BASE_WORD: usize = 1;
/// Word index of `ErlSubBits::start`.
const SUB_BITS_START_WORD: usize = 2;
/// Word index of `ErlSubBits::end`.
const SUB_BITS_END_WORD: usize = 3;

/// `ceil(n / d)`, written with divisions only so it can never overflow (the
/// `(n + d - 1) / d` form can wrap for a corrupt size word).
#[inline]
const fn ceil_div(n: usize, d: usize) -> usize {
    n / d + ((n % d != 0) as usize)
}

/// `NBYTES(n_bits)` (`erl_bits.h:25`): the number of whole bytes a bit count
/// occupies, i.e. `ceil(n_bits / 8)`. Overflow-free for every bit count.
#[inline]
pub const fn bit_count_to_bytes(n_bits: usize) -> usize {
    ceil_div(n_bits, 8)
}

/// `heap_bits_size(num_bits)` (`erl_bits.h:157-164`): the word count of the
/// `ErlHeapBits` block holding `n_bits` bits —
/// `2 + ceil(ceil(n_bits / 8) / sizeof(Eterm))` (`heap_bin_size__(NBYTES(n))`).
///
/// Overflow-free for every bit count, so a corrupt `size` word is rejected by the
/// header-arity check in [`bitstring_parts`] rather than wrapping.
#[inline]
pub const fn heap_bits_size_words(n_bits: usize) -> usize {
    ERL_HEAP_BITS_HEAD_WORDS + ceil_div(bit_count_to_bytes(n_bits), WORD_BYTES)
}

/// Materializes a **binary** (`n_bits = bytes.len() * 8`) on `heap`.
///
/// # Divergence from ERTS (documented)
///
/// ERTS stores a binary larger than `ERL_ONHEAP_BINARY_LIMIT` (64 bytes) as an
/// off-heap ref-counted `Binary` behind a `BinRef`+`ErlSubBits` pair. This crate
/// has no refc binary yet (owned by `beam_alloc`/`beam_gc`), so **every** binary is
/// materialized as an on-heap `ErlHeapBits` block, whatever its size. The
/// selection is an open *interior* policy — no externally pinned layout/ABI depends
/// on it — and the observable term (`is_binary/1`, `byte_size/1`, contents) is
/// identical. See the crate `CONTEXT.md`, "Known Issues".
pub fn make_binary(heap: &mut Heap, bytes: &[u8]) -> Result<Eterm> {
    // A byte count of `usize::MAX / 8` could not be held in memory anyway; report it
    // rather than wrapping.
    match bytes.len().checked_mul(8) {
        Some(n_bits) => make_bitstring(heap, bytes, n_bits),
        None => Err(error::system_limit()),
    }
}

/// Materializes a bitstring of `n_bits` bits on `heap`.
///
/// `bytes` must be exactly `ceil(n_bits / 8)` bytes long — the payload of the
/// bitstring, most significant bit first inside each byte (so `<<1:3>>` is
/// `&[0b0010_0000]` with `n_bits = 3`). The unused trailing bits of the last byte
/// are forced to zero, so the term carries no garbage from the caller's buffer (the
/// C decoder copies the trailing byte verbatim; equality/`term_to_binary` ignore
/// those bits there, and zeroing them here makes our own round-trip canonical).
///
/// A `bytes` length that is not `ceil(n_bits / 8)` (in particular
/// `n_bits > bytes.len() * 8`) yields `BeamError::BadArg`; so does a bit count whose
/// block size would overflow.
pub fn make_bitstring(heap: &mut Heap, bytes: &[u8], n_bits: usize) -> Result<Eterm> {
    let n_bytes = bit_count_to_bytes(n_bits);
    if bytes.len() != n_bytes {
        return Err(error::bad_arg());
    }
    let words = heap_bits_size_words(n_bits);
    // SAFETY: `alloc_words` returned `words` exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(words)? };
    // SAFETY: `hp` owns `words = 2 + ceil(n_bytes / WORD_BYTES)` words: word 0 is the
    // header, word 1 the bit size and words `2..` the data — exactly the `ErlHeapBits`
    // layout (`erl_bits.h:150-164`), so the header arity is `words - 1` and the root is
    // `make_bitstring(hp) == make_boxed(hp)`. The data words are zeroed first (so the
    // padding bytes of the trailing data word are initialised), then the `n_bytes`
    // payload bytes are copied in little-endian (byte 0 at the lowest address of word
    // 2), and finally the unused trailing bits of the last byte are cleared.
    unsafe {
        *hp = make_header(words - 1, HEAP_BITS_SUBTAG);
        *hp.add(1) = Eterm::from_raw(n_bits);
        let data = hp.add(ERL_HEAP_BITS_HEAD_WORDS).cast::<u8>();
        let data_bytes = words * WORD_BYTES - ERL_HEAP_BITS_HEAD_WORDS * WORD_BYTES;
        core::ptr::write_bytes(data, 0, data_bytes);
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        let tail = n_bits % 8;
        if tail != 0 {
            *data.add(bytes.len() - 1) &= 0xFFu8 << (8 - tail);
        }
    }
    Ok(make_boxed(hp))
}

/// Splits a bitstring term into `(base, offset_bits, size_bits)`.
///
/// Both bitstring representations are handled:
///
/// * `ErlHeapBits` — `base = boxed_val(t) + 2`, `offset_bits = 0`,
///   `size_bits` = the `size` word;
/// * `ErlSubBits` — `base = base_flags & !ERL_SUB_BITS_FLAG_MASK`,
///   `offset_bits = start`, `size_bits = end - start`.
///
/// `base` points at the **byte** containing bit 0 of the underlying storage, so bit
/// `i` of the bitstring is bit `(offset_bits + i) % 8` (MSB-first) of
/// `base[(offset_bits + i) / 8]`. A term that is not a bitstring, an `ErlHeapBits`
/// whose header arity disagrees with its `size` word, or an `ErlSubBits` with
/// `end < start` yields `BeamError::BadArg` — never a panic, never UB.
pub fn bitstring_parts(term: Eterm) -> Result<(*const u8, usize, usize)> {
    // SAFETY: `is_bitstring` reads only the header word of a boxed term (the caller
    // invariant for any `Eterm`) and never dereferences a non-boxed word.
    if !unsafe { is_bitstring(term) } {
        return Err(error::bad_arg());
    }
    let p = boxed_val(term);
    // SAFETY: `is_bitstring` established that `term` is boxed with a live, initialised
    // header word, so reading that word is sound.
    let header = unsafe { *p };
    if header == HEADER_SUB_BITS {
        // `ErlSubBits`: the header value fixes the block at `ERL_SUB_BITS_SIZE` words,
        // so words 1..4 are live and initialised by the writer.
        // SAFETY: as above — the header identifies a full `ErlSubBits` block.
        let (base_flags, start, end) = unsafe {
            (
                (*p.add(SUB_BITS_BASE_WORD)).raw(),
                (*p.add(SUB_BITS_START_WORD)).raw(),
                (*p.add(SUB_BITS_END_WORD)).raw(),
            )
        };
        // A negative size is impossible in a well-formed term; reject rather than
        // wrapping the subtraction.
        let Some(size_bits) = end.checked_sub(start) else {
            return Err(error::bad_arg());
        };
        let base = base_flags & !BITSTRING_POINTER_FLAG_MASK;
        Ok((base as *const u8, start, size_bits))
    } else {
        // `ErlHeapBits`: `[thing_word, size, data…]`. The header arity is
        // `heap_bits_size(size) - 1`; a mismatch means a corrupt/mismatched block.
        // SAFETY: the header is a live term word.
        let size_bits = unsafe { (*p.add(1)).raw() };
        if heap_bits_size_words(size_bits) != arityval(header) + 1 {
            return Err(error::bad_arg());
        }
        // SAFETY: the arity check fixed the block at `2 + ceil(n_bytes/WORD_BYTES)`
        // words, so word 2 (the first data byte) is inside it and stays live for as long
        // as the caller keeps the owning heap alive.
        let base = unsafe { p.add(ERL_HEAP_BITS_HEAD_WORDS).cast::<u8>() };
        Ok((base, 0, size_bits))
    }
}

/// The bit size of a bitstring term.
///
/// A term that is not a bitstring (or whose block is corrupt) yields
/// `BeamError::BadArg`.
pub fn bitstring_size_bits(term: Eterm) -> Result<usize> {
    Ok(bitstring_parts(term)?.2)
}

/// A safe, borrowed byte view of a bitstring term: `(bytes, offset_bits,
/// size_bits)`.
///
/// `bytes` spans `ceil((offset_bits + size_bits) / 8)` bytes starting at the
/// bitstring's base, so the `size_bits` significant bits are addressable as
/// `bytes[(offset_bits + i) / 8]` bit `(offset_bits + i) % 8` (MSB-first). The
/// borrowed slice aliases the term's storage: the caller must keep the owning heap
/// (or, for a sub-bitstring, the underlying binary) alive and unmoved for `'a`.
///
/// This is the safe counterpart of [`bitstring_parts`] used by the encoder and
/// decoder — it keeps the raw-pointer arithmetic inside this module, so
/// `encode.rs`/`decode.rs` stay safe Rust.
pub fn bitstring_bytes<'a>(term: Eterm) -> Result<(&'a [u8], usize, usize)> {
    let (base, offset_bits, size_bits) = bitstring_parts(term)?;
    let Some(end_bit) = offset_bits.checked_add(size_bits) else {
        return Err(error::bad_arg());
    };
    let n_bytes = bit_count_to_bytes(end_bit);
    // SAFETY: `bitstring_parts` established that `base` points at the first byte of the
    // storage backing a live bitstring of `offset_bits + size_bits` bits, so the
    // `ceil((offset_bits + size_bits) / 8)` bytes from there are inside that object and
    // stay live for `'a` as long as the caller keeps the owner alive (the same invariant
    // `beam_term`'s `*_parts` accessors document).
    let bytes = unsafe { core::slice::from_raw_parts(base, n_bytes) };
    Ok((bytes, offset_bits, size_bits))
}

#[cfg(test)]
mod tests {
    //! Raw-word tests of the bitstring shim: the block layout is asserted word by
    //! word against `erl_bits.h`, both representations are read back, and every
    //! error path is checked to be `BadArg` with no panic.

    use beam_common::BeamError;
    use beam_term::prelude::{make_small, NIL};

    use super::*;

    /// A heap large enough for every block built here.
    fn new_heap() -> Heap {
        Heap::new(64).expect("the carrier allocator never fails for a tiny heap")
    }

    /// Asserts a shim call failed with `BeamError::BadArg`.
    fn assert_bad_arg<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    #[test]
    fn bit_count_and_block_size_match_erl_bits_h() {
        assert_eq!(bit_count_to_bytes(0), 0);
        assert_eq!(bit_count_to_bytes(1), 1);
        assert_eq!(bit_count_to_bytes(8), 1);
        assert_eq!(bit_count_to_bytes(9), 2);
        assert_eq!(bit_count_to_bytes(64), 8);
        assert_eq!(bit_count_to_bytes(65), 9);

        // `heap_bits_size(n_bits) = 2 + ceil(ceil(n_bits/8)/8)`.
        assert_eq!(heap_bits_size_words(0), 2);
        assert_eq!(heap_bits_size_words(1), 3);
        assert_eq!(heap_bits_size_words(64), 3);
        assert_eq!(heap_bits_size_words(65), 4);
        assert_eq!(heap_bits_size_words(512), 10);
        // An absurd size word stays finite instead of wrapping or panicking.
        assert!(heap_bits_size_words(usize::MAX) > heap_bits_size_words(512));
    }

    #[test]
    fn empty_binary_is_a_two_word_block() {
        let mut heap = new_heap();
        let term = make_binary(&mut heap, &[]).expect("the empty binary fits");
        assert_eq!(heap.used(), 2, "[thing_word, size] and no data word");

        // SAFETY: `term` is the 2-word block this test just wrote.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, make_header(1, HEAP_BITS_SUBTAG));
            assert_eq!((*p.add(1)).raw(), 0);
        }
        assert!(unsafe { is_bitstring(term) });
        assert_eq!(bitstring_size_bits(term).unwrap(), 0);
        let (bytes, off, size) = bitstring_bytes(term).unwrap();
        assert_eq!((bytes, off, size), (&[][..], 0, 0));
        assert_eq!(bitstring_parts(term).unwrap().1, 0);
    }

    #[test]
    fn binary_block_layout_is_byte_exact() {
        let mut heap = new_heap();
        let payload = [0xDEu8, 0xAD, 0xBE, 0xEF];
        let base = heap.hp();
        let term = make_binary(&mut heap, &payload).expect("fits");
        // 2 head words + ceil(4/8) = 1 data word.
        assert_eq!(heap.used(), 3);

        // SAFETY: `base` is the first word of the 3-word block just written; the payload
        // was copied little-endian into the data word at index 2.
        unsafe {
            assert_eq!(*base, make_header(2, HEAP_BITS_SUBTAG));
            assert_eq!((*base.add(1)).raw(), 32);
            let mut stored = [0u8; 4];
            core::ptr::copy_nonoverlapping(base.add(2).cast::<u8>(), stored.as_mut_ptr(), 4);
            assert_eq!(stored, payload);
        }
        assert_eq!(boxed_val(term), base, "root = make_boxed(hp)");

        let (bytes, off, size) = bitstring_bytes(term).unwrap();
        assert_eq!((bytes, off, size), (&payload[..], 0, 32));
        assert_eq!(bitstring_size_bits(term).unwrap(), 32);
    }

    #[test]
    fn bitstring_zeroes_the_unused_trailing_bits() {
        let mut heap = new_heap();
        // `<<1:3>>`: the significant bits are `001`, the rest of the byte is garbage.
        let term = make_bitstring(&mut heap, &[0b0011_1111], 3).expect("fits");
        assert_eq!(heap.used(), 3);
        let (bytes, off, size) = bitstring_bytes(term).unwrap();
        assert_eq!((bytes, off, size), (&[0b0010_0000][..], 0, 3));
    }

    #[test]
    fn make_bitstring_rejects_a_wrong_byte_count() {
        let mut heap = new_heap();
        // `n_bits > bytes.len() * 8` — the classic lying bit count.
        assert_bad_arg(make_bitstring(&mut heap, &[0], 9));
        assert_bad_arg(make_bitstring(&mut heap, &[], 1));
        // Too *many* bytes for the bit count is equally a contract violation.
        assert_bad_arg(make_bitstring(&mut heap, &[0, 0], 3));
        assert_eq!(heap.used(), 0, "a rejected bitstring allocated nothing");

        // The exact byte count is accepted at every byte boundary.
        for (n_bits, n_bytes) in [(0usize, 0usize), (1, 1), (8, 1), (9, 2), (16, 2)] {
            let bytes = vec![0u8; n_bytes];
            let mut heap = new_heap();
            let term = make_bitstring(&mut heap, &bytes, n_bits).expect("exact");
            assert_eq!(bitstring_size_bits(term).unwrap(), n_bits);
        }
    }

    #[test]
    fn sub_bitstring_is_read_through_its_base_and_offsets() {
        /// The underlying storage of `[0b1010_1010, 0b1100_0000]`, 8-byte aligned so
        /// that the low two `base_flags` bits are free for the flag mask.
        #[repr(align(8))]
        struct Storage([u8; 2]);
        let storage = Storage([0b1010_1010, 0b1100_0000]);
        let payload: &[u8] = &storage.0;
        let base = payload.as_ptr() as usize;
        // `ErlSubBits`: start = 3, end = 12 ⇒ 9 bits from bit 3, i.e. `010101011`.
        let mut words = [
            HEADER_SUB_BITS,
            Eterm::from_raw(base),
            Eterm::from_raw(3),
            Eterm::from_raw(12),
            make_small(0),
        ];
        let sub = make_boxed(words.as_mut_ptr());

        assert!(unsafe { is_bitstring(sub) });
        assert_eq!(bitstring_size_bits(sub).unwrap(), 9);
        let (bytes, off, size) = bitstring_bytes(sub).unwrap();
        assert_eq!((bytes, off, size), (payload, 3, 9));

        // The flags in the low bits of `base_flags` are masked off the pointer.
        words[1] = Eterm::from_raw(base | BITSTRING_POINTER_FLAG_MASK);
        let sub = make_boxed(words.as_mut_ptr());
        let (offset, size) = {
            let (_, off, size) = bitstring_parts(sub).unwrap();
            (off, size)
        };
        assert_eq!((offset, size), (3, 9));
        assert_eq!(bitstring_bytes(sub).unwrap().0, payload);
    }

    #[test]
    fn corrupt_and_wrong_terms_are_bad_arg() {
        let mut heap = new_heap();
        // Not a bitstring at all.
        assert_bad_arg(bitstring_parts(NIL));
        assert_bad_arg(bitstring_parts(make_small(7)));
        assert_bad_arg(bitstring_size_bits(NIL));
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        assert_bad_arg(bitstring_parts(tuple));

        // A heap-bits header whose arity disagrees with its size word: 2 data words are
        // needed for 100 bits, but only 1 is claimed (arity 2 ⇒ 3 words).
        let mut words = [
            make_header(2, HEAP_BITS_SUBTAG),
            Eterm::from_raw(100),
            make_small(0),
        ];
        let bad = make_boxed(words.as_mut_ptr());
        assert!(unsafe { is_bitstring(bad) });
        assert_bad_arg(bitstring_parts(bad));

        // A sub-bits term with `end < start` underflows the size word.
        let storage = [0u8; 4];
        let mut sub_words = [
            HEADER_SUB_BITS,
            Eterm::from_raw(storage.as_ptr() as usize),
            Eterm::from_raw(9),
            Eterm::from_raw(4),
            make_small(0),
        ];
        let bad = make_boxed(sub_words.as_mut_ptr());
        assert_bad_arg(bitstring_parts(bad));
    }
}
