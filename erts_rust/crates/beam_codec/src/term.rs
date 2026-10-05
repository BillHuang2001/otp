//! Term inspection and boxed sub-term materialization over [`beam_alloc::Heap`].
//!
//! This is the **only** module in the crate allowed to touch raw heap words: the
//! crate-wide `#![deny(unsafe_code)]` is lifted for it alone (`#[allow(unsafe_code)]`
//! on the `mod term;` declaration in `lib.rs`), so the crate's entire `unsafe`
//! surface is one auditable file. `beam_term` marks its accessors
//! (`is_tuple`, `tuple_element`, `float_val`, `big_val`, …) as `unsafe fn` because
//! the validity of an `Eterm` is a caller invariant and there is no safe way to
//! read or write a heap word; the functions here wrap those accessors and turn a
//! violated invariant into `BeamError::BadArg` instead of undefined behaviour.
//!
//! Every layout below is pinned line-by-line to the legacy C ground truth
//! `erts/emulator/beam/erl_term.h` and `erts/emulator/beam/erl_map.h` (the C is
//! read, never modified).
//!
//! This module is **public but not part of the cross-crate contract**: the
//! `beam_term` API exposes the layout *predicates* but not the *constructors* for
//! floats, bignums, flatmaps and the external identifiers. Those belong in
//! `beam_term` (the lowest common ancestor for term operations, and the crate
//! that owns the frozen layout); when they land there this module collapses into
//! thin forwarders — see the crate `CONTEXT.md`, "Known Issues".

use core::mem::size_of;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{
    arityval, boxed_val, is_bignum_header, is_boxed, is_external_pid_header,
    is_external_port_header, is_external_ref_header, is_flatmap_header, is_float, is_list,
    is_tuple, list_val, make_arityval, make_boxed, make_header, make_tuple, tuple0, tuple_val,
    Eterm, BIG_SIGN_BIT, CAR_OFFSET, CDR_OFFSET, EXTERNAL_PID_SUBTAG, EXTERNAL_PORT_SUBTAG,
    EXTERNAL_REF_SUBTAG, HEADER_FLONUM, MAP_SUBTAG, NEG_BIG_SUBTAG, POS_BIG_SUBTAG,
};

use crate::error;
use crate::tag::Tag;

/// Number of bytes in one term word (`size_of::<Eterm>()`).
const WORD_BYTES: usize = size_of::<Eterm>();
/// Number of `u32`s that fit in one term word (two on 64-bit).
const U32_PER_WORD: usize = WORD_BYTES / size_of::<u32>();
/// `BIG_ARITY_MAX` (`erl_term.h:465-469`, 64-bit `ARCH_64`): the largest bignum
/// digit count.
const BIG_ARITY_MAX: usize = (1 << 16) - 1;
/// `MAP_SMALL_MAP_LIMIT`: the largest flat-map pair count (ERTS switches to a
/// hashmap above it).
const MAP_SMALL_MAP_LIMIT: usize = 32;
/// `EXTERNAL_THING_HEAD_SIZE` (`erl_term.h:1161`):
/// `offsetof(ExternalThing, data) / sizeof(Uint)` — the `[header, node, next]`
/// head, three words on `ARCH_64`.
const EXTERNAL_THING_HEAD_SIZE: usize = 3;
/// `EXTERNAL_PORT_HEAP_SIZE` (`erl_term.h:1177`) = the head + one 64-bit data word.
const EXTERNAL_PORT_HEAP_SIZE: usize = EXTERNAL_THING_HEAD_SIZE + 1;
/// `EXTERNAL_PID_HEAP_SIZE` (`erl_term.h:1166`) = the head + one 64-bit data word.
const EXTERNAL_PID_HEAP_SIZE: usize = EXTERNAL_THING_HEAD_SIZE + 1;
/// `ERTS_MAX_REF_NUMBERS` (`erl_term.h:780`): the largest external-reference id
/// count [`make_external_ref`] accepts (and therefore the largest
/// [`external_ref_parts`] reports).
pub const MAX_REF_IDS: usize = 5;

// The frozen term layout is the 64-bit `ARCH_64` ABI of the C ground truth:
// `ErtsDigit` is `Uint` = one `Eterm` word (`big.h:29`) and the
// external-identifier data union packs two `Uint32`s per word
// (`erl_term.h:1156`).
const _: () = assert!(WORD_BYTES == 8, "the frozen term layout assumes ARCH_64");

/// Materializes a float term on `heap`.
///
/// Layout pinned to `erl_term.h:479` (`HEADER_FLONUM = make_header(1, FLOAT_SUBTAG)`)
/// and `erl_term.h:506` (`PUT_DOUBLE`): the two words `[HEADER_FLONUM, f64 bits]`,
/// with the root word `make_boxed(hp)` pointing at the first of them.
pub fn make_float(heap: &mut Heap, value: f64) -> Result<Eterm> {
    // SAFETY: `alloc_words(2)` returned two exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(2)? };
    // SAFETY: `hp` owns exactly those two words; both are written before use.
    unsafe {
        *hp = HEADER_FLONUM;
        *hp.add(1) = Eterm::from_raw(value.to_bits() as usize);
    }
    Ok(make_boxed(hp))
}

/// Reads the value of a float term.
///
/// Inverse of [`make_float`]: yields the `f64` stored in the word following the
/// float header (`erl_term.h:479`, `erl_term.h:506`). A term that is not a float
/// yields `BeamError::BadArg`.
pub fn float_value(term: Eterm) -> Result<f64> {
    // `is_float(x)` is `is_boxed(x) && *boxed_val(x) == HEADER_FLONUM`
    // (`erl_term.h:484`); it reads only the header word.
    // SAFETY: `is_float` dereferences the header word of a boxed term, which is the
    // caller invariant for any `Eterm` (a term is always a valid value).
    if !unsafe { is_float(term) } {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_float` established that `term` is boxed with header
    // `HEADER_FLONUM`, i.e. the `[HEADER_FLONUM, bits]` block, so word `1` is a
    // live, initialised word.
    let bits = unsafe { (*boxed_val(term).add(1)).raw() as u64 };
    Ok(f64::from_bits(bits))
}

/// Materializes a bignum term on `heap`.
///
/// `digits` are `ErtsDigit`s (`big.h:29`: `typedef Uint ErtsDigit`, i.e. exactly one
/// machine word — `u64` on `ARCH_64`), **least significant first**; `negative` sets
/// the header's `_BIG_SIGN_BIT`. Layout pinned to `erl_term.h:452-471` and
/// `big.h:68-73,96-97`: one digit per word and `BIG_NEED_SIZE(n) = n + 1` words, i.e.
/// `[make_pos_bignum_header(n-1) | make_neg_bignum_header(n-1), d0 … d(n-1)]`, with
/// the root word `make_boxed(hp)`.
///
/// An empty `digits` is an illegal zero-width bignum (`external.c:4474-4486` strips
/// the leading zeroes precisely to avoid building one) and yields
/// `BeamError::BadArg`; more than `BIG_ARITY_MAX` (`(1 << 16) - 1`) digits yields
/// `BeamError::SystemLimit`.
pub fn make_big(heap: &mut Heap, negative: bool, digits: &[u64]) -> Result<Eterm> {
    let n = digits.len();
    if n == 0 {
        return Err(error::bad_arg());
    }
    if n > BIG_ARITY_MAX {
        return Err(error::system_limit());
    }
    // `BIG_NEED_SIZE(n) = n + 1` (`big.h:97`): the header word plus one word per
    // `ErtsDigit`.
    let words = 1 + n;
    // SAFETY: `alloc_words(words)` returned `words` exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(words)? };
    let subtag = if negative {
        NEG_BIG_SUBTAG
    } else {
        POS_BIG_SUBTAG
    };
    // SAFETY: `hp` owns `words = 1 + n` words; word `0` receives the bignum header
    // (arity `n - 1`) and words `1..=n` receive the digits in least-significant-first
    // order (`BIG_V(xp) = (ErtsDigit *)((xp) + 1)`, `big.h:68`).
    unsafe {
        *hp = make_header(n - 1, subtag);
        for (i, &digit) in digits.iter().enumerate() {
            *hp.add(1 + i) = Eterm::from_raw(digit as usize);
        }
    }
    Ok(make_boxed(hp))
}

/// Splits a bignum term into `(negative, digits)`.
///
/// `digits` are `ErtsDigit`s (`u64`), least significant first, borrowed from the
/// term's storage (the caller must keep the owning heap alive) — the exact inverse of
/// [`make_big`] (`big.h:59-73`: `BIG_V(xp) = (ErtsDigit *)((xp) + 1)`,
/// `BIG_ARITY(xp) = bignum_header_arity(*xp)`; the sign lives in the header's
/// `_BIG_SIGN_BIT`). A term that is not a bignum yields `BeamError::BadArg`.
pub fn big_parts<'a>(term: Eterm) -> Result<(bool, &'a [u64])> {
    if !is_boxed(term) {
        return Err(error::bad_arg());
    }
    let p = boxed_val(term);
    // SAFETY: `is_boxed(term)` means `p` is a live header-word pointer (the caller
    // invariant for any `Eterm`); the predicate reads that one word only.
    let hdr = unsafe { *p };
    if !is_bignum_header(hdr) {
        return Err(error::bad_arg());
    }
    // The header arity is `n - 1` (`make_bignum_header(n-1)`, `erl_term.h:452-453`).
    let n = arityval(hdr) + 1;
    // SAFETY: the header encodes the digit count `n` and `make_big` stored the `n`
    // one-word `ErtsDigit`s starting at word one of the block, so the slice is live and
    // in bounds; the caller keeps the owning heap alive for `'a`.
    let digits = unsafe { core::slice::from_raw_parts(p.add(1).cast::<u64>(), n) };
    Ok(((hdr.raw() & BIG_SIGN_BIT) != 0, digits))
}

/// Materializes a small flat map on `heap`.
///
/// Layout pinned to `erl_map.h:181-199` (`MAP_HEADER_FLATMAP = 0x12C`, i.e.
/// `_make_header(4, MAP_SUBTAG)`) and `erl_map.c:673-700`: the word block
/// `[arityval(n), k0 … k(n-1), MAP_HEADER_FLATMAP, n, keys, v0 … v(n-1)]`, where
/// `keys = make_tuple(hp)` is emitted **before** the head, the flatmap head sits
/// at word `1 + n`, the `size` field is the raw integer `n`, and the root is
/// `make_boxed(hp + 1 + n)`. `n == 0` is the 3-word block
/// `[MAP_HEADER_FLATMAP, 0, tuple0()]`.
///
/// More than `MAP_SMALL_MAP_LIMIT` (32) pairs needs the HAMT builder and yields
/// [`crate::error::unsupported`].
pub fn make_flatmap(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let n = pairs.len();
    if n > MAP_SMALL_MAP_LIMIT {
        return Err(error::unsupported(Tag::Map));
    }
    // NOTE: duplicate keys are **not** detected in Milestone 1 — that needs term
    // comparison (owned by `beam_term`/`beam_ets`), so the wire order is preserved
    // as-is (see the crate `CONTEXT.md`, "Known Issues").
    if n == 0 {
        // SAFETY: `alloc_words(3)` returned three exclusively-owned, writable words.
        let hp = unsafe { heap.alloc_words(3)? };
        // SAFETY: `hp` owns exactly those three words; all are written before use.
        unsafe {
            *hp = make_header(4, MAP_SUBTAG);
            *hp.add(1) = Eterm::from_raw(0);
            *hp.add(2) = tuple0();
        }
        return Ok(make_boxed(hp));
    }
    // SAFETY: `alloc_words(4 + 2n)` returned `4 + 2n` exclusively-owned, writable
    // words.
    let hp = unsafe { heap.alloc_words(4 + 2 * n)? };
    // SAFETY: `hp` owns `4 + 2n` words. The keys tuple occupies `hp[0..=n]`, the
    // flatmap head prefix `[head, size, keys]` occupies `hp[1+n..=3+n]`, and the
    // values occupy `hp[4+n..4+2n]` — every write below is in bounds.
    let root = unsafe {
        *hp = make_arityval(n);
        for i in 0..n {
            *hp.add(1 + i) = pairs[i].0;
        }
        *hp.add(1 + n) = make_header(4, MAP_SUBTAG);
        *hp.add(2 + n) = Eterm::from_raw(n);
        *hp.add(3 + n) = make_tuple(hp);
        for j in 0..n {
            *hp.add(4 + n + j) = pairs[j].1;
        }
        hp.add(1 + n)
    };
    Ok(make_boxed(root))
}

/// Splits a flat map into its `(keys, values)`.
///
/// Both slices are borrowed from the term's storage (the caller must keep the
/// owning heap alive) and have the same length, in wire order — the exact inverse
/// of [`make_flatmap`] (`erl_map.h:181-199`). A term that is not a *flat* map (or
/// is a hashmap) yields `BeamError::BadArg`.
pub fn flatmap_parts<'a>(term: Eterm) -> Result<(&'a [Eterm], &'a [Eterm])> {
    if !is_boxed(term) {
        return Err(error::bad_arg());
    }
    let p = boxed_val(term);
    // SAFETY: `p` is a live header-word pointer (caller invariant for a boxed term);
    // the predicate reads that one word only.
    let hdr = unsafe { *p };
    // The flatmap head is `MAP_HEADER_FLATMAP`; a hashmap head (or any other header)
    // is rejected.
    if !is_flatmap_header(hdr) {
        return Err(error::bad_arg());
    }
    // The root points at the flatmap head (`flatmap_t`): `[head, size, keys]`,
    // followed by the values (`erl_map.h:57-61`, `flatmap_get_*`). Word `1` is the
    // raw pair count `n`.
    // SAFETY: the head prefix `[head, size, keys]` is live and initialised by
    // `make_flatmap`, so word `1` is a valid readable word.
    let n = unsafe { (*p.add(1)).raw() };
    // A well-formed flatmap never exceeds `MAP_SMALL_MAP_LIMIT` pairs; a larger `n`
    // means a corrupt/mismatched block, so reject it before building any slice.
    if n > MAP_SMALL_MAP_LIMIT {
        return Err(error::bad_arg());
    }
    if n == 0 {
        return Ok((&[], &[]));
    }
    // SAFETY: a well-formed flatmap block owns `4 + 2n` words and the caller keeps
    // the owning heap alive for `'a`. Relative to the head pointer `p`, the
    // `flatmap_t` prefix is `[head, size, keys]`, so word `2` is the keys tuple term
    // and words `3..3+n` are the values — both slices are live and in bounds.
    let (keys, values) = unsafe {
        let values = core::slice::from_raw_parts(p.add(3).cast::<Eterm>(), n);
        let keys_tup = *p.add(2);
        let keys = core::slice::from_raw_parts(boxed_val(keys_tup).add(1), n);
        (keys, values)
    };
    Ok((keys, values))
}

/// Materializes an external pid on `heap`.
///
/// Layout pinned to `erl_term.h:1137-1172` (`ExternalThing`): the three-word head
/// `[header, node, next]` (`EXTERNAL_THING_HEAD_SIZE`) followed by the 64-bit
/// `data.pid` union, so `EXTERNAL_PID_HEAP_SIZE = 4` words and the header is
/// `make_external_pid_header() = make_header(EXTERNAL_PID_HEAP_SIZE - 1, …)`.
/// `number` and `serial` are the two `Uint32`s of `data.pid` packed into the single
/// data word (`num` first, then `ser`), exactly as C lays the struct out.
///
/// **M1 substitution:** in C, `node` is a pointer to a registered node (which carries
/// the sysname *and* the creation) and `next` is the off-heap link
/// (`etp->node = erts_find_or_insert_node(sysname, cre, …)`). This crate has no node
/// registry yet, so word `1` holds the sysname atom term and word `2` — the unused
/// off-heap-link slot — holds `creation`. The word positions and arity are exactly
/// C's; only the `node`/`next` pointer semantics are deferred until a node-identity
/// type exists (see the crate `CONTEXT.md`, "Known Issues").
pub fn make_external_pid(
    heap: &mut Heap,
    node: Eterm,
    number: u32,
    serial: u32,
    creation: u32,
) -> Result<Eterm> {
    // SAFETY: `alloc_words(EXTERNAL_PID_HEAP_SIZE)` returned four exclusively-owned,
    // writable words.
    let hp = unsafe { heap.alloc_words(EXTERNAL_PID_HEAP_SIZE)? };
    // SAFETY: `hp` owns exactly those four words; all are written before use. Word `3`
    // is the `data.pid = { Uint32 num; Uint32 ser; }` union, so it carries two
    // native-endian `u32`s and the full 32-bit range of both fields survives.
    unsafe {
        *hp = make_header(EXTERNAL_PID_HEAP_SIZE - 1, EXTERNAL_PID_SUBTAG);
        *hp.add(1) = node;
        *hp.add(2) = Eterm::from_raw(creation as usize);
        let data = hp.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
        *data = number;
        *data.add(1) = serial;
    }
    Ok(make_boxed(hp))
}

/// Splits an external pid term into `(node, number, serial, creation)`.
///
/// The exact inverse of [`make_external_pid`] (`erl_term.h:1137-1172`): the data word
/// is `data.pid = { num, ser }` and `creation` comes from the `next` slot (the M1
/// substitution described on [`make_external_pid`]). A term that is not an external
/// pid, or whose header arity is not `EXTERNAL_PID_HEAP_SIZE - 1`, yields
/// `BeamError::BadArg`.
pub fn external_pid_parts(term: Eterm) -> Result<(Eterm, u32, u32, u32)> {
    if !is_boxed(term) {
        return Err(error::bad_arg());
    }
    let p = boxed_val(term);
    // SAFETY: `p` is a live header-word pointer (caller invariant for a boxed term);
    // the predicate reads that one word only.
    let hdr = unsafe { *p };
    if !is_external_pid_header(hdr) || arityval(hdr) + 1 != EXTERNAL_PID_HEAP_SIZE {
        return Err(error::bad_arg());
    }
    // SAFETY: the header arity confirms the 4-word `[header, node, creation, data]`
    // block (`make_external_pid`), so words `1..=3` are live and initialised. The data
    // word holds two native-endian `u32`s.
    let (node, number, serial, creation) = unsafe {
        let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
        (*p.add(1), *data, *data.add(1), (*p.add(2)).raw() as u32)
    };
    Ok((node, number, serial, creation))
}

/// Materializes an external port on `heap`.
///
/// Layout pinned to `erl_term.h:1137-1183` (`ExternalThing`): the three-word head
/// `[header, node, next]` followed by the `data.port` union, whose `id` is a `Uint64`
/// on `ARCH_64` (`erl_term.h:1148-1155`) — so `EXTERNAL_PORT_HEAP_SIZE = 4` words and
/// the header is
/// `make_external_port_header() = make_header(EXTERNAL_PORT_HEAP_SIZE - 1, …)`.
/// The `node`/`next` slots carry the sysname atom and `creation` (the M1 substitution
/// described on [`make_external_pid`]); `number` is the 64-bit `V4_PORT_EXT` id.
pub fn make_external_port(
    heap: &mut Heap,
    node: Eterm,
    number: u64,
    creation: u32,
) -> Result<Eterm> {
    // SAFETY: `alloc_words(EXTERNAL_PORT_HEAP_SIZE)` returned four exclusively-owned,
    // writable words.
    let hp = unsafe { heap.alloc_words(EXTERNAL_PORT_HEAP_SIZE)? };
    // SAFETY: `hp` owns exactly those four words; all are written before use. Word `3`
    // is `data.port.id` (`Uint64` on `ARCH_64`), so the full 64-bit id survives.
    unsafe {
        *hp = make_header(EXTERNAL_PORT_HEAP_SIZE - 1, EXTERNAL_PORT_SUBTAG);
        *hp.add(1) = node;
        *hp.add(2) = Eterm::from_raw(creation as usize);
        *hp.add(EXTERNAL_THING_HEAD_SIZE) = Eterm::from_raw(number as usize);
    }
    Ok(make_boxed(hp))
}

/// Splits an external port term into `(node, number, creation)`.
///
/// The exact inverse of [`make_external_port`] (`erl_term.h:1137-1183`): the data word
/// is the 64-bit `data.port.id` and `creation` comes from the `next` slot (the M1
/// substitution described on [`make_external_pid`]). A term that is not an external
/// port, or whose header arity is not `EXTERNAL_PORT_HEAP_SIZE - 1`, yields
/// `BeamError::BadArg`.
pub fn external_port_parts(term: Eterm) -> Result<(Eterm, u64, u32)> {
    if !is_boxed(term) {
        return Err(error::bad_arg());
    }
    let p = boxed_val(term);
    // SAFETY: `p` is a live header-word pointer (caller invariant for a boxed term);
    // the predicate reads that one word only.
    let hdr = unsafe { *p };
    if !is_external_port_header(hdr) || arityval(hdr) + 1 != EXTERNAL_PORT_HEAP_SIZE {
        return Err(error::bad_arg());
    }
    // SAFETY: the header arity confirms the 4-word `[header, node, creation, id]` block
    // (`make_external_port`), so words `1..=3` are live and initialised.
    let (node, number, creation) = unsafe {
        (
            *p.add(1),
            (*p.add(EXTERNAL_THING_HEAD_SIZE)).raw() as u64,
            (*p.add(2)).raw() as u32,
        )
    };
    Ok((node, number, creation))
}

/// Materializes an external reference on `heap`.
///
/// Layout pinned to `erl_term.h:1137-1188` (`ExternalThing`,
/// `make_external_ref_header`) and `bif.c:4810-4848`: the three-word head
/// `[header, node, next]`, then the `data.ui32` array holding the id **count** first
/// (`etp->data.ui32[0] = n`, `bif.c:4840`) followed by the `n` ids. With
/// `DW = n / 2 + 1` (`bif.c:4820`) the data section is `DW` words and
/// `make_external_ref_header(DW) = make_header(DW + EXTERNAL_THING_HEAD_SIZE - 1, …)`,
/// i.e. arity `n / 2 + 3` on `ARCH_64` — exactly the C word shape.
/// `node`/`next` carry the sysname atom and `creation` (the M1 substitution described
/// on [`make_external_pid`]); `ids` are the 32-bit `NEWER_REFERENCE_EXT` identifier
/// words (three on a 64-bit node). An empty or over-long `ids` slice (more than
/// `MAX_REF_IDS`) yields `BeamError::SystemLimit`.
pub fn make_external_ref(
    heap: &mut Heap,
    node: Eterm,
    ids: &[u32],
    creation: u32,
) -> Result<Eterm> {
    let n = ids.len();
    if n == 0 || n > MAX_REF_IDS {
        return Err(error::system_limit());
    }
    // `DW = n / sizeof(ErtsDigit) + 1` ref "words" (`bif.c:4818-4835`), i.e. one
    // word per two `u32`s plus the word that holds the id count.
    let data_words = n / U32_PER_WORD + 1;
    let words = EXTERNAL_THING_HEAD_SIZE + data_words;
    // SAFETY: `alloc_words(words)` returned `words` exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(words)? };
    // SAFETY: `hp` owns `words = 3 + data_words` words: word `0` is the header (arity
    // `words - 1`), word `1` is the node, word `2` is the creation, and the `data.ui32`
    // array starts at word `EXTERNAL_THING_HEAD_SIZE` with `data_words * 2` slots. The
    // zeroing initialises the whole data section (including the trailing padding slot
    // of an even id count); the id count and the ids are then written into its front.
    unsafe {
        *hp = make_header(words - 1, EXTERNAL_REF_SUBTAG);
        *hp.add(1) = node;
        *hp.add(2) = Eterm::from_raw(creation as usize);
        let data = hp.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
        core::ptr::write_bytes(data, 0, data_words * U32_PER_WORD);
        *data = n as u32;
        core::ptr::copy_nonoverlapping(ids.as_ptr(), data.add(1), n);
    }
    Ok(make_boxed(hp))
}

/// Splits an external reference term into `(node, ids, creation)`.
///
/// `ids` is borrowed from the term's storage (the caller must keep the owning heap
/// alive) — the exact inverse of [`make_external_ref`] (`erl_term.h:1137-1188`): the
/// data section starts at word `EXTERNAL_THING_HEAD_SIZE` with the id count as the
/// first `u32` (`external_ref_no_numbers(x) = external_ref_data(x)[0]`, `ARCH_64`) and
/// the ids after it, while `creation` comes from the `next` slot (the M1 substitution
/// described on [`make_external_pid`]). A term that is not an external reference, or
/// whose id count does not fit its data section, yields `BeamError::BadArg`.
pub fn external_ref_parts<'a>(term: Eterm) -> Result<(Eterm, &'a [u32], u32)> {
    if !is_boxed(term) {
        return Err(error::bad_arg());
    }
    let p = boxed_val(term);
    // SAFETY: `p` is a live header-word pointer (caller invariant for a boxed term);
    // the predicate reads that one word only.
    let hdr = unsafe { *p };
    if !is_external_ref_header(hdr) {
        return Err(error::bad_arg());
    }
    // `external_thing_data_words`: `arityval(header) + 1 - EXTERNAL_THING_HEAD_SIZE`.
    let Some(data_words) = arityval(hdr).checked_sub(EXTERNAL_THING_HEAD_SIZE - 1) else {
        return Err(error::bad_arg());
    };
    // SAFETY: the header arity fixes the block size at `EXTERNAL_THING_HEAD_SIZE +
    // data_words` words, so the data section is live and initialised; word `1` is the
    // node and word `2` the creation.
    let (node, ids, creation) = unsafe {
        let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
        let n = *data as usize;
        // The count is self-describing, so it must fit the data section it was read
        // from before any slice is built.
        if n == 0 || n > MAX_REF_IDS || n + 1 > data_words * U32_PER_WORD {
            return Err(error::bad_arg());
        }
        (
            *p.add(1),
            core::slice::from_raw_parts(data.add(1), n),
            (*p.add(2)).raw() as u32,
        )
    };
    Ok((node, ids, creation))
}

/// The arity of a tuple term.
///
/// Reads the tuple's arity word — `arityval(hdr)` / `thing_arityval(hdr)`
/// (`erl_term.h`, the `TAG_PRIMARY_HEADER` arity in the header's high bits). A
/// term that is not a tuple yields `BeamError::BadArg`.
pub fn tuple_arity(term: Eterm) -> Result<usize> {
    // SAFETY: `is_tuple` reads only the header word of a boxed term (the caller
    // invariant for any `Eterm`).
    if !unsafe { is_tuple(term) } {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_tuple` established that `term` is a live tuple whose header is an
    // arityval, so the arity field is meaningful.
    Ok(arityval(unsafe { *boxed_val(term) }))
}

/// The element at `index` of a tuple term, zero-based.
///
/// Index `i` is the heap word `1 + i` after the tuple's arity header
/// (`erl_term.h`, the tuple `thing` layout). An out-of-range index or a term that
/// is not a tuple yields `BeamError::BadArg`.
pub fn tuple_element(term: Eterm, index: usize) -> Result<Eterm> {
    let arity = tuple_arity(term)?;
    if index >= arity {
        return Err(error::bad_arg());
    }
    // SAFETY: `tuple_arity` established that `term` is a live tuple of `arity`
    // elements, so word `1 + index` is a live, initialised element word.
    Ok(unsafe { *tuple_val(term).add(1 + index) })
}

/// The head (`CAR`) of a cons cell.
///
/// The head is the word at `CAR_OFFSET` (0) of the header-less 2-word cons cell
/// tagged `TAG_PRIMARY_LIST` (`erl_term.h`; `NIL` is an immediate, not a cons
/// cell, and yields `BeamError::BadArg`).
pub fn cons_head(term: Eterm) -> Result<Eterm> {
    if !is_list(term) {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_list` established that `term` is a cons cell (a header-less
    // 2-word `[CAR, CDR]` block), so the CAR word is live and initialised.
    Ok(unsafe { *list_val(term).add(CAR_OFFSET) })
}

/// The tail (`CDR`) of a cons cell.
///
/// The tail is the word at `CDR_OFFSET` (1) of the header-less 2-word cons cell
/// tagged `TAG_PRIMARY_LIST` (`erl_term.h`; `NIL` is an immediate, not a cons
/// cell, and yields `BeamError::BadArg`).
pub fn cons_tail(term: Eterm) -> Result<Eterm> {
    if !is_list(term) {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_list` established that `term` is a cons cell (a header-less
    // 2-word `[CAR, CDR]` block), so the CDR word is live and initialised.
    Ok(unsafe { *list_val(term).add(CDR_OFFSET) })
}

#[cfg(test)]
mod tests {
    //! Raw-word unit tests: every block layout is asserted word-by-word against the
    //! legacy C ground truth, and every accessor is checked for its error behaviour
    //! on the wrong term type / out-of-range index.
    //!
    //! `unsafe` below only dereferences pointers into a `Heap` this test has just
    //! allocated and written itself (and, for the parts accessors, reads back the
    //! same words); each block has its own `// SAFETY:` note.

    use beam_alloc::Heap;
    use beam_common::{BeamError, Result};
    use beam_term::prelude::{is_map_header, make_small, NIL};

    use super::*;
    use crate::error::is_unsupported;

    /// A heap large enough for every block built in this module.
    fn new_heap() -> Heap {
        Heap::new(256).expect("the carrier allocator never fails for a tiny heap")
    }

    /// Asserts a codec call failed with `BeamError::BadArg` (`BeamError` is not
    /// `PartialEq`, so `matches!` is used).
    fn assert_bad_arg<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    /// Asserts a codec call failed with `BeamError::SystemLimit`.
    fn assert_system_limit<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::SystemLimit) => {}
            other => panic!("expected SystemLimit, got {other:?}"),
        }
    }

    #[test]
    fn float_block_and_bit_patterns() {
        for value in [
            3.14_f64,
            0.0,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::MIN,
            f64::MAX,
        ] {
            let mut heap = new_heap();
            let term = make_float(&mut heap, value).expect("2 words fit");
            assert_eq!(heap.used(), 2);

            // SAFETY: `term` is a float this test just built on `heap`, so the block
            // `[HEADER_FLONUM, bits]` is live and word `1` is initialised.
            unsafe {
                let p = boxed_val(term);
                assert_eq!(*p, HEADER_FLONUM);
                assert_eq!(*p.add(1), Eterm::from_raw(value.to_bits() as usize));
            }
            // Compare by bit pattern so NaN round-trips too.
            assert_eq!(float_value(term).unwrap().to_bits(), value.to_bits());
        }
    }

    #[test]
    fn float_value_rejects_non_floats() {
        let mut heap = new_heap();
        assert_bad_arg(float_value(make_small(7)));
        assert_bad_arg(float_value(NIL));
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        assert_bad_arg(float_value(tuple));
    }

    #[test]
    fn bignum_block_layout_and_parts() {
        let mut heap = new_heap();
        let digits = [0x0A_u64, 0x0B, 0x0C];
        let term = make_big(&mut heap, false, &digits).expect("fits");
        // `BIG_NEED_SIZE(3) = 3 + 1`: the header word plus one `ErtsDigit` per word.
        assert_eq!(heap.used(), 4);

        // SAFETY: `term` is a bignum this test just built; word `0` is its header and
        // the digit words follow it (`BIG_V(xp) = (ErtsDigit *)((xp) + 1)`).
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, make_header(2, POS_BIG_SUBTAG));
            assert!(is_bignum_header(*p));
            assert_eq!(arityval(*p), digits.len() - 1);
            assert_eq!((*p).raw() & BIG_SIGN_BIT, 0);
            let stored = core::slice::from_raw_parts(p.add(1).cast::<u64>(), digits.len());
            assert_eq!(stored, &digits[..]);
        }

        let (negative, got) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(got, &digits[..]);
    }

    #[test]
    fn bignum_sign_bit_and_digit_words() {
        let mut heap = new_heap();
        let term = make_big(&mut heap, true, &[u64::MAX]).expect("fits");
        assert_eq!(heap.used(), 2, "header + one digit word");

        // SAFETY: `term` is a bignum this test just built; read its header and the
        // single digit word.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, make_header(0, NEG_BIG_SUBTAG));
            assert_ne!((*p).raw() & BIG_SIGN_BIT, 0);
            assert_eq!((*p.add(1)).raw() as u64, u64::MAX);
        }

        let (negative, got) = big_parts(term).unwrap();
        assert!(negative);
        assert_eq!(got, &[u64::MAX]);
    }

    #[test]
    fn bignum_rejects_empty_and_over_long_digits() {
        let mut heap = new_heap();
        assert_bad_arg(make_big(&mut heap, false, &[]));
        assert_eq!(heap.used(), 0, "the rejected bignum allocated nothing");

        let too_many = vec![0u64; BIG_ARITY_MAX + 1];
        assert_system_limit(make_big(&mut heap, false, &too_many));
        assert_eq!(heap.used(), 0);

        // Exactly `BIG_ARITY_MAX` digits is still accepted.
        let mut big_heap = Heap::new(BIG_ARITY_MAX + 1).expect("a 64 KiB word heap");
        let at_max = vec![1u64; BIG_ARITY_MAX];
        let term = make_big(&mut big_heap, false, &at_max).expect("at the digit bound");
        assert_eq!(big_parts(term).unwrap().1, &at_max[..]);
    }

    #[test]
    fn big_parts_rejects_non_bignums() {
        let mut heap = new_heap();
        let float = make_float(&mut heap, 1.0).unwrap();
        assert_bad_arg(big_parts(make_small(1)));
        assert_bad_arg(big_parts(float));
    }

    #[test]
    fn flatmap_empty_is_exactly_three_words() {
        let mut heap = new_heap();
        let base = heap.hp();
        let map = make_flatmap(&mut heap, &[]).expect("fits");
        assert_eq!(heap.used(), 3, "the empty flatmap is 3 words");

        // SAFETY: `base` is the first word of the 3-word block just written.
        unsafe {
            assert_eq!(*base, make_header(4, MAP_SUBTAG));
            assert!(is_map_header(*base));
            assert!(is_flatmap_header(*base));
            assert_eq!((*base.add(1)).raw(), 0);
            assert_eq!(*base.add(2), tuple0());
        }
        assert_eq!(boxed_val(map), base, "root = make_boxed(hp)");
        let (keys, values) = flatmap_parts(map).unwrap();
        assert!(keys.is_empty());
        assert!(values.is_empty());
    }

    #[test]
    fn flatmap_two_pairs_word_by_word() {
        let mut heap = new_heap();
        let base = heap.hp();
        let k0 = make_small(10);
        let v0 = make_small(100);
        let k1 = make_small(20);
        let v1 = make_small(200);
        let map = make_flatmap(&mut heap, &[(k0, v0), (k1, v1)]).expect("fits");
        assert_eq!(heap.used(), 4 + 2 * 2);

        // SAFETY: `base` is the first word of the `4 + 2n` block just written; every
        // index below is within that block.
        unsafe {
            assert_eq!(*base, make_arityval(2)); // word 0: keys-tuple arity
            assert_eq!(*base.add(1), k0); // word 1: k0
            assert_eq!(*base.add(2), k1); // word 2: k1
            assert_eq!(*base.add(3), make_header(4, MAP_SUBTAG)); // word 1+n: head
            assert!(is_flatmap_header(*base.add(3)));
            assert_eq!((*base.add(4)).raw(), 2); // word 2+n: RAW size == n
            assert_eq!(*base.add(5), make_tuple(base)); // word 3+n: keys tuple
            assert_eq!(*base.add(6), v0); // word 4+n: v0
            assert_eq!(*base.add(7), v1); // word 7: v1

            // The keys tuple term (word `3+n`) points back at the block base, and the
            // flatmap root is the head at `hp + 1 + n` (`flatmap_parts` reads the
            // keys/values relative to that head: keys at head+2, values at head+3).
            let keys_tup = *base.add(5); // word 3+n
            assert_eq!(boxed_val(keys_tup), base);
            let head = boxed_val(map);
            assert_eq!(head, base.add(3)); // hp + 1 + n
            assert_eq!(*head.add(2), keys_tup); // head+2 == keys tuple
            assert_eq!(*head.add(3), v0); // head+3 == first value
        }

        let (keys, values) = flatmap_parts(map).unwrap();
        assert_eq!(keys, &[k0, k1]);
        assert_eq!(values, &[v0, v1]);
    }

    #[test]
    fn flatmap_rejects_oversized_and_non_maps() {
        let mut heap = new_heap();
        let pairs: Vec<(Eterm, Eterm)> = (0..MAP_SMALL_MAP_LIMIT + 1)
            .map(|i| (make_small(i as i64), make_small(i as i64)))
            .collect();
        let err = make_flatmap(&mut heap, &pairs).unwrap_err();
        assert!(is_unsupported(&err), "oversized maps are unsupported in M1");
        assert_eq!(heap.used(), 0, "the rejected map allocated nothing");

        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        assert_bad_arg(flatmap_parts(tuple));
        assert_bad_arg(flatmap_parts(make_small(1)));
    }

    #[test]
    fn external_pid_block_layout_and_parts() {
        let mut heap = new_heap();
        let node = make_small(7);
        let pid =
            make_external_pid(&mut heap, node, 0xAAAA_BBBB, 0xCCCC_DDDD, 0x1234_5678).expect("fits");
        assert_eq!(heap.used(), EXTERNAL_PID_HEAP_SIZE);

        // SAFETY: `pid` is the 4-word block this test just wrote; word `3` is the
        // `data.pid = { Uint32 num; Uint32 ser; }` union.
        unsafe {
            let p = boxed_val(pid);
            assert_eq!(*p, make_header(EXTERNAL_PID_HEAP_SIZE - 1, EXTERNAL_PID_SUBTAG));
            assert!(is_external_pid_header(*p));
            assert_eq!(*p.add(1), node);
            assert_eq!(
                (*p.add(2)).raw() as u32,
                0x1234_5678,
                "creation lives in the off-heap-link slot"
            );
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, 0xAAAA_BBBB);
            assert_eq!(*data.add(1), 0xCCCC_DDDD);
        }

        let (n, num, ser, cre) = external_pid_parts(pid).unwrap();
        assert_eq!(
            (n, num, ser, cre),
            (node, 0xAAAA_BBBB, 0xCCCC_DDDD, 0x1234_5678)
        );
        assert_bad_arg(external_pid_parts(make_small(1)));
        // A block of the right arity but the wrong subtag is rejected.
        assert_bad_arg(external_pid_parts(
            make_external_port(&mut heap, node, 1, 2).unwrap(),
        ));
    }

    #[test]
    fn external_port_block_layout_and_parts() {
        let mut heap = new_heap();
        let node = make_small(9);
        let port = make_external_port(&mut heap, node, 0x1122_3344_5566_7788, 0x99AA_BBCC)
            .expect("fits");
        assert_eq!(heap.used(), EXTERNAL_PORT_HEAP_SIZE);

        // SAFETY: `port` is the 4-word block this test just wrote; word `3` is the
        // 64-bit `data.port.id` (`ARCH_64`).
        unsafe {
            let p = boxed_val(port);
            assert_eq!(*p, make_header(EXTERNAL_PORT_HEAP_SIZE - 1, EXTERNAL_PORT_SUBTAG));
            assert!(is_external_port_header(*p));
            assert_eq!(*p.add(1), node);
            assert_eq!((*p.add(2)).raw() as u32, 0x99AA_BBCC);
            assert_eq!((*p.add(3)).raw() as u64, 0x1122_3344_5566_7788);
        }

        let (n, num, cre) = external_port_parts(port).unwrap();
        assert_eq!((n, num, cre), (node, 0x1122_3344_5566_7788, 0x99AA_BBCC));
        assert_bad_arg(external_port_parts(make_small(1)));
        // A pid of the same size but a different subtag is not a port.
        let pid = make_external_pid(&mut heap, node, 1, 2, 3).unwrap();
        assert_bad_arg(external_port_parts(pid));
    }

    #[test]
    fn external_ref_block_layout_and_parts() {
        let mut heap = new_heap();
        let node = make_small(11);
        let ids = [0x1111_1111u32, 0x2222_2222, 0x3333_3333];
        let r = make_external_ref(&mut heap, node, &ids, 0xABCD_1234).expect("fits");
        // `EXTERNAL_THING_HEAD_SIZE` + `DW` = 3 + (3 / 2 + 1) = 5 words.
        assert_eq!(heap.used(), EXTERNAL_THING_HEAD_SIZE + 3 / U32_PER_WORD + 1);

        // SAFETY: `r` is the 5-word block this test just wrote. The data section is the
        // `data.ui32` array at word 3: the id count first (`bif.c:4840`), then the ids.
        unsafe {
            let p = boxed_val(r);
            assert_eq!(
                *p,
                make_header(3 / U32_PER_WORD + 1 + EXTERNAL_THING_HEAD_SIZE - 1, EXTERNAL_REF_SUBTAG)
            );
            assert!(is_external_ref_header(*p));
            assert_eq!(arityval(*p) + 1, heap.used());
            assert_eq!(*p.add(1), node);
            assert_eq!((*p.add(2)).raw() as u32, 0xABCD_1234);
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, ids.len() as u32, "the id count heads the data section");
            assert_eq!(*data.add(1), ids[0]);
            assert_eq!(*data.add(2), ids[1]);
            assert_eq!(*data.add(3), ids[2]);
        }

        let (n, got, cre) = external_ref_parts(r).unwrap();
        assert_eq!(n, node);
        assert_eq!(got, &ids[..]);
        assert_eq!(cre, 0xABCD_1234);
        assert_bad_arg(external_ref_parts(make_small(1)));
    }

    #[test]
    fn external_ref_single_id_and_bounds() {
        let mut heap = new_heap();
        let r = make_external_ref(&mut heap, NIL, &[0xDEAD_BEEF], 0x01).expect("fits");
        // `DW = 1 / 2 + 1 = 1`; the head (3 words) + the 1-word data section.
        assert_eq!(heap.used(), EXTERNAL_THING_HEAD_SIZE + 1);
        // SAFETY: `r` is the 4-word block this test just wrote.
        unsafe {
            let p = boxed_val(r);
            assert_eq!(
                *p,
                make_header(1 / U32_PER_WORD + 1 + EXTERNAL_THING_HEAD_SIZE - 1, EXTERNAL_REF_SUBTAG)
            );
            assert_eq!(*p.add(1), NIL);
            assert_eq!((*p.add(2)).raw() as u32, 0x01);
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, 1);
            assert_eq!(*data.add(1), 0xDEAD_BEEF);
        }
        let (n, ids, cre) = external_ref_parts(r).unwrap();
        assert_eq!(n, NIL);
        assert_eq!(ids, &[0xDEAD_BEEF]);
        assert_eq!(cre, 0x01);

        assert_system_limit(make_external_ref(&mut heap, NIL, &[], 0));
        assert_system_limit(make_external_ref(&mut heap, NIL, &[1, 2, 3, 4, 5, 6], 0));
        // `MAX_REF_IDS` itself is accepted.
        let max = make_external_ref(&mut heap, NIL, &[1, 2, 3, 4, 5], 0).expect("fits");
        assert_eq!(external_ref_parts(max).unwrap().1, &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn external_ref_parts_rejects_a_corrupt_id_count() {
        let mut heap = new_heap();
        let r = make_external_ref(&mut heap, NIL, &[0xAA, 0xBB], 0).expect("fits");
        // SAFETY: `r` is a block this test owns; the data section starts at word 3 and
        // is overwritten with an id count the block cannot hold. The term is dropped
        // afterwards, so no accessor ever sees the corrupt state.
        unsafe {
            let data = boxed_val(r).add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            *data = (MAX_REF_IDS + 1) as u32;
        }
        assert_bad_arg(external_ref_parts(r));

        // SAFETY: as above — a zero id count is equally impossible.
        unsafe {
            let data = boxed_val(r).add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            *data = 0;
        }
        assert_bad_arg(external_ref_parts(r));
    }

    #[test]
    fn tuple_accessors_round_trip_and_bounds() {
        let mut heap = new_heap();
        let a = make_small(1);
        let b = make_small(2);
        let c = make_small(3);
        let t = heap.tuple(&[a, b, c]).unwrap();

        assert_eq!(tuple_arity(t).unwrap(), 3);
        assert_eq!(tuple_element(t, 0).unwrap(), a);
        assert_eq!(tuple_element(t, 1).unwrap(), b);
        assert_eq!(tuple_element(t, 2).unwrap(), c);
        assert_bad_arg(tuple_element(t, 3));
        assert_bad_arg(tuple_element(t, usize::MAX));

        // Not a tuple: a small int, `NIL` and a boxed non-tuple.
        assert_bad_arg(tuple_arity(a));
        assert_bad_arg(tuple_arity(NIL));
        assert_bad_arg(tuple_element(NIL, 0));
        let float = make_float(&mut heap, 1.0).unwrap();
        assert_bad_arg(tuple_arity(float));

        // The zero-arity tuple is still a tuple.
        assert_eq!(tuple_arity(tuple0()).unwrap(), 0);
        assert_bad_arg(tuple_element(tuple0(), 0));
    }

    #[test]
    fn cons_accessors_round_trip_and_reject_nil() {
        let mut heap = new_heap();
        let proper = heap.cons(make_small(1), NIL).unwrap();
        assert_eq!(cons_head(proper).unwrap(), make_small(1));
        assert_eq!(cons_tail(proper).unwrap(), NIL);

        // An improper list: the tail is a non-nil term.
        let improper = heap.cons(make_small(1), make_small(2)).unwrap();
        assert_eq!(cons_head(improper).unwrap(), make_small(1));
        assert_eq!(cons_tail(improper).unwrap(), make_small(2));

        // `NIL` is an immediate, not a cons cell, and neither is a tuple.
        assert_bad_arg(cons_head(NIL));
        assert_bad_arg(cons_tail(NIL));
        assert_bad_arg(cons_head(make_small(3)));
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        assert_bad_arg(cons_tail(tuple));
    }
}
