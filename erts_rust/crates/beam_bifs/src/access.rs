//! Safe wrappers over `beam_term`'s raw (unsafe) term accessors.
//!
//! This is the crate's **only** `#[allow(unsafe_code)]` module (the
//! `beam_engine::predicates` / `beam_codec::term` convention). `beam_term`'s
//! boxed-term predicates and structure accessors are `unsafe fn` because each
//! dereferences the header word of a boxed term — sound only when the term is a
//! *live* BEAM term. Every BIF module classifies and destructures its arguments
//! through these wrappers and stays safe Rust; no other module in this crate uses
//! `unsafe`.
//!
//! # Safety invariant
//!
//! Every wrapped term is a **live BEAM term**: a BIF argument taken from the
//! machine's registers/stack, or a term built on a live heap — never unvalidated
//! raw memory. The *immediate* fast paths (small ints, atoms, `NIL`, internal
//! pids/ports) are delegated to the safe `beam_term` predicates
//! ([`beam_term::is_small`], [`beam_term::is_atom`], …); the boxed wrappers only
//! dereference a header after `beam_term` has confirmed the word is boxed.
#![allow(unsafe_code)]

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_term::prelude::{boxed_val, make_boxed, make_header, make_small, signed_value, try_make_small};
use beam_term::{
    Eterm, BITSTRING_TAG_MASK, ERL_HEAP_BITS_SIZE, HEADER_ARITY_OFFS, HEADER_FLONUM,
    HEAP_BITS_SUBTAG, REF_SUBTAG,
};

// ---------------------------------------------------------------------------
// Classification — safe pass-throughs of the `beam_term` immediate predicates
// ---------------------------------------------------------------------------

/// `is_atom(x)`.
#[inline]
pub(crate) fn is_atom(x: Eterm) -> bool {
    beam_term::is_atom(x)
}

/// `is_small(x)` — a 60-bit small integer.
#[inline]
pub(crate) fn is_small(x: Eterm) -> bool {
    beam_term::is_small(x)
}

/// `is_nil(x)` — the empty list `[]`.
#[inline]
pub(crate) fn is_nil(x: Eterm) -> bool {
    beam_term::is_nil(x)
}

/// `is_list(x)` — a cons cell (`false` for `NIL`).
#[inline]
pub(crate) fn is_list(x: Eterm) -> bool {
    beam_term::is_list(x)
}

/// `is_internal_pid(x)`.
#[inline]
pub(crate) fn is_internal_pid(x: Eterm) -> bool {
    beam_term::is_internal_pid(x)
}

/// `is_internal_port(x)`.
#[inline]
pub(crate) fn is_internal_port(x: Eterm) -> bool {
    beam_term::is_internal_port(x)
}

// ---------------------------------------------------------------------------
// Classification — boxed-term predicates (each dereferences a header word)
// ---------------------------------------------------------------------------

/// `is_tuple(x)`.
#[inline]
pub(crate) fn is_tuple(x: Eterm) -> bool {
    // SAFETY: `x` is a live machine word; `is_tuple` checks `is_boxed` before
    // dereferencing the header.
    unsafe { beam_term::is_tuple(x) }
}

/// `is_float(x)` — a boxed float (`HEADER_FLONUM`).
#[inline]
pub(crate) fn is_float(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_float(x) }
}

/// `is_map(x)`.
#[inline]
pub(crate) fn is_map(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_map(x) }
}

/// `is_big(x)` — a bignum.
// A documented part of the `access.rs` surface reserved for dependents: M1 builds no
// bignums, so no BIF dispatches on it yet.
#[allow(dead_code)]
#[inline]
pub(crate) fn is_big(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_big(x) }
}

/// `is_bitstring(x)`.
#[inline]
pub(crate) fn is_bitstring(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_bitstring(x) }
}

/// `is_binary(x)` — a bitstring whose size is a whole number of bytes.
#[inline]
pub(crate) fn is_binary(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_binary(x) }
}

/// `is_function(x)` — a fun/lambda (`is_any_fun`).
#[inline]
pub(crate) fn is_function(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_any_fun(x) }
}

/// `FUN_HEADER_ARITY_OFFS` (`erl_term.h:413`): `_HEADER_ARITY_OFFS + 2`.
const FUN_HEADER_ARITY_OFFS: u32 = HEADER_ARITY_OFFS + 2;

/// The width of the fun header's `arity`/`environment` fields — `erl_fun.h`'s
/// "the arity and number of free variables are both limited to 255"
/// (`erl_term.h:403`).
const FUN_FIELD_MAX: usize = 0xFF;

/// The `arity` field of a fun term's header, or `None` when `x` is not a fun.
///
/// `MAKE_FUN_HEADER(Arity, NumFree, External)` (`erl_term.h:417-424`) packs the
/// arity at bit `FUN_HEADER_ARITY_OFFS` (8) and the environment size at bit 16,
/// eight bits each; `TAG_PRIMARY_HEADER` is zero, so the header word is the pure
/// bit field. `erlang:is_function/2` (`crate::type_test`) is the only caller.
#[inline]
pub(crate) fn fun_arity(x: Eterm) -> Option<u32> {
    if !is_function(x) {
        return None;
    }
    // SAFETY: [`is_function`] (`is_any_fun`) established that `x` is boxed with a
    // `FUN_SUBTAG` header, so its header word is a live, initialised term word.
    let header = unsafe { (*boxed_val(x)).raw() };
    Some(((header >> FUN_HEADER_ARITY_OFFS) & FUN_FIELD_MAX) as u32)
}

/// `is_record(x)` — a native record (`RECORD_SUBTAG`).
///
/// This is the C `is_record` macro (`erl_term.h:437`), **not** a tuple test: a
/// tuple carries the `ARITYVAL` subtag, so `is_record({a,b})` is `false` — exactly
/// what `erlang:is_record/1` documents.
#[inline]
pub(crate) fn is_record(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_record(x) }
}

/// `is_internal_ref(x)` — an on-heap reference.
#[inline]
pub(crate) fn is_internal_ref(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_internal_ref(x) }
}

/// `is_external_pid(x)`.
#[inline]
pub(crate) fn is_external_pid(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_external_pid(x) }
}

/// `is_external_port(x)`.
#[inline]
pub(crate) fn is_external_port(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_external_port(x) }
}

/// `is_external_ref(x)`.
#[inline]
pub(crate) fn is_external_ref(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_external_ref(x) }
}

/// `is_integer(x)` — a small integer or a bignum.
#[inline]
pub(crate) fn is_integer(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`]; the immediate fast path never dereferences.
    unsafe { beam_term::is_integer(x) }
}

/// `is_number(x)` — an integer or a float.
#[inline]
pub(crate) fn is_number(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`]; the immediate fast path never dereferences.
    unsafe { beam_term::is_number(x) }
}

/// `is_pid(x)` — an internal or external pid.
#[inline]
pub(crate) fn is_pid(x: Eterm) -> bool {
    is_internal_pid(x) || is_external_pid(x)
}

/// `is_port(x)` — an internal or external port.
#[inline]
pub(crate) fn is_port(x: Eterm) -> bool {
    is_internal_port(x) || is_external_port(x)
}

/// `is_reference(x)` — an internal or external reference.
#[inline]
pub(crate) fn is_reference(x: Eterm) -> bool {
    is_internal_ref(x) || is_external_ref(x)
}

// ---------------------------------------------------------------------------
// Structure (each dereferences the boxed word)
// ---------------------------------------------------------------------------

/// `tuple_size(x)` — the arity of a tuple.
#[inline]
pub(crate) fn tuple_size(x: Eterm) -> usize {
    // SAFETY: `x` is a live tuple whose header word is valid (caller invariant;
    // the BIF modules call this only after [`is_tuple`] returned true).
    unsafe { beam_term::tuple_size(x) }
}

/// `tuple_element(x, i)` — element `i` (0-based, unchecked) of a tuple.
#[inline]
pub(crate) fn tuple_element(x: Eterm, i: usize) -> Eterm {
    // SAFETY: as [`tuple_size`]; `i` must be `< tuple_size(x)` (caller invariant).
    unsafe { beam_term::tuple_element(x, i) }
}

/// `CAR(x)` — the head word of a cons cell.
#[inline]
pub(crate) fn car(x: Eterm) -> Eterm {
    // SAFETY: `x` is a live non-`NIL` list term (caller invariant; the BIF modules
    // call this only after [`is_list`] returned true).
    unsafe { beam_term::car(x) }
}

/// `CDR(x)` — the tail word of a cons cell.
#[inline]
pub(crate) fn cdr(x: Eterm) -> Eterm {
    // SAFETY: as [`car`].
    unsafe { beam_term::cdr(x) }
}

// ---------------------------------------------------------------------------
// Floats — the frozen `[HEADER_FLONUM, <bits>]` thing (see `beam_term::size`)
// ---------------------------------------------------------------------------

/// Builds a boxed 64-bit float on `heap`: `[HEADER_FLONUM, <bits as a raw word>]`.
///
/// # Errors
///
/// Propagates [`BeamError::Alloc`] when `heap` cannot fit the two words.
pub(crate) fn make_float(heap: &mut Heap, v: f64) -> Result<Eterm> {
    // SAFETY: `alloc_words` hands out two exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(2)? };
    // SAFETY: `p` addresses the two words just handed out; both are written before
    // the pointer escapes this block.
    unsafe {
        *p = HEADER_FLONUM;
        *p.add(1) = Eterm::from_raw(v.to_bits() as usize);
    }
    Ok(make_boxed(p as *const Eterm))
}

/// The IEEE-754 bit pattern of a boxed float.
#[inline]
pub(crate) fn float_bits(x: Eterm) -> u64 {
    // SAFETY: `x` is a live boxed float (`is_float`); its payload word at offset 1
    // holds the IEEE-754 bits (the `beam_term::size` layout).
    unsafe { (*boxed_val(x).add(1)).raw() as u64 }
}

/// The value of a boxed float.
#[inline]
pub(crate) fn float_value(x: Eterm) -> f64 {
    f64::from_bits(float_bits(x))
}

// ---------------------------------------------------------------------------
// The M1 reference thing (used for the ETS table handle)
// ---------------------------------------------------------------------------

/// The number of payload words of the ref thing: `node`, `serial` and `id`.
const REF_PAYLOAD_WORDS: usize = 3;

/// Builds the frozen 4-word ref thing `[make_header(3, REF_SUBTAG), 0, 0, id]`.
///
/// `beam_term` has no `make_ref` in Milestone-1, so this mirrors
/// `beam_process::MonitorRef::to_eterm`; it is the M1 table handle
/// [`crate::ets::TableRegistry`] keys on (see the crate `CONTEXT.md`, Design
/// Decisions / Known Issues).
///
/// # Errors
///
/// [`BeamError::SystemLimit`] when `id` does not fit a 60-bit small integer;
/// otherwise propagates [`BeamError::Alloc`].
pub(crate) fn make_ref_thing(heap: &mut Heap, id: u64) -> Result<Eterm> {
    let raw = i64::try_from(id).map_err(|_| BeamError::SystemLimit)?;
    let id_term = try_make_small(raw).map_err(|_| BeamError::SystemLimit)?;
    // SAFETY: `alloc_words` hands out four exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(REF_PAYLOAD_WORDS + 1)? };
    // SAFETY: `p` addresses the four words just handed out; every one of them is
    // written before the pointer escapes this block.
    unsafe {
        *p = make_header(REF_PAYLOAD_WORDS, REF_SUBTAG);
        *p.add(1) = make_small(0); // node: the local node
        *p.add(2) = make_small(0); // serial: one id space in M1
        *p.add(3) = id_term; // id
    }
    Ok(make_boxed(p as *const Eterm))
}

/// The numeric id encoded in the M1 ref thing [`make_ref_thing`] builds.
///
/// This is the read-back half of the ETS table handle: `erts_rust/crates/beam_bifs`
/// builds the handle with [`make_ref_thing`] and recovers its id here (see
/// `crate::ets`).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not an internal-ref thing, or when its id
/// word is not a non-negative small integer. The caller treats that as the `ets`
/// `badarg` for an invalid handle.
pub(crate) fn ref_thing_id(term: Eterm) -> Result<u64> {
    if !is_internal_ref(term) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_internal_ref`] confirmed `term` is a boxed term whose header
    // carries `REF_SUBTAG`. [`make_ref_thing`] is this crate's only producer of
    // such a term and always writes a 4-word `[header, node, serial, id]` block,
    // so word index `REF_PAYLOAD_WORDS` (the id) is in bounds and holds a small
    // integer — never unvalidated memory.
    let id_word = unsafe { *boxed_val(term).add(REF_PAYLOAD_WORDS) };
    if !is_small(id_word) {
        return Err(BeamError::BadArg);
    }
    let id = signed_value(id_word);
    u64::try_from(id).map_err(|_| BeamError::BadArg)
}

// ---------------------------------------------------------------------------
// Heap bitstrings (`ErlHeapBits`, `erl_bits.h`)
//
// The items below are a documented part of the `access.rs` surface reserved for
// dependents: `crate::bits` is their only caller, and `erlang:size/1`
// (`crate::term_ops`) is the only BIF wired to it in M1 — the `binary`/`unicode` BIFs
// that read the bytes back land later, so the unused-tail items carry
// `#[allow(dead_code)]` (the same convention as [`is_big`] above).
// ---------------------------------------------------------------------------

/// `ERL_HEAP_BITS_SIZE` — the number of words *before* the data area of an on-heap
/// bitstring (`[thing_word, size, data…]`).
const HEAP_BITS_PREFIX_WORDS: usize = ERL_HEAP_BITS_SIZE;

/// The total words a heap bitstring of `bit_len` bits occupies.
///
/// `erl_bits.h:heap_bits_size(bits)` is `sizeof(ErlHeapBits)/sizeof(Eterm) - 1 +
/// ceil(NBYTES(bits) / sizeof(Eterm))` = two prefix words plus `ceil(bits / 64)`
/// data words.
#[allow(dead_code)]
fn heap_bits_words(bit_len: usize) -> usize {
    HEAP_BITS_PREFIX_WORDS + bit_len.div_ceil(8 * size_of::<Eterm>())
}

/// Builds the on-heap bitstring `<<bytes:bit_len>>` on `heap`.
///
/// The block is the frozen `ErlHeapBits` of `erl_bits.h`:
/// `[make_header(1 + data_words, HEAP_BITS_SUBTAG), bit_len, data…]`, where
/// `data_words = ceil(bit_len / 64)`. The bits are the first `bit_len` bits of
/// `bytes`, packed **MSB-first** in address order (`erl_bits.h` addresses the data
/// area as a byte array: `Base = (byte*)&_hb->data[0]`, so a byte-aligned binary's
/// bytes are stored in order). The unused bytes of the final data word are zeroed,
/// so the layout is fully deterministic and endianness-agnostic.
///
/// Unlike C, no 64-byte on-heap limit is enforced: Milestone-1 keeps *every* binary
/// on the process heap (see `crate::bits`).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `bit_len > bytes.len() * 8`; otherwise propagates
/// [`BeamError::Alloc`](beam_common::BeamError::Alloc) when `heap` cannot fit the
/// block.
#[allow(dead_code)]
pub(crate) fn make_heap_bitstring(heap: &mut Heap, bytes: &[u8], bit_len: usize) -> Result<Eterm> {
    // `saturating_mul` can only saturate for a slice no allocator could ever hand
    // out (`len > usize::MAX / 8`), where every `bit_len` still fits — so the check
    // below stays exact without a wrapping multiply.
    if bit_len > bytes.len().saturating_mul(8) {
        return Err(BeamError::BadArg);
    }
    let data_words = bit_len.div_ceil(8 * size_of::<Eterm>());
    // SAFETY: `alloc_words` hands out `2 + data_words` exclusively-owned, writable
    // words.
    let p = unsafe { heap.alloc_words(heap_bits_words(bit_len))? };
    // SAFETY: `p` addresses the `2 + data_words` words just handed out; the header
    // word, the bit-size word and every data byte are written before the pointer
    // escapes this block.
    unsafe {
        *p = make_header(1 + data_words, HEAP_BITS_SUBTAG);
        *p.add(1) = Eterm::from_raw(bit_len);
        let data = core::slice::from_raw_parts_mut(
            p.add(HEAP_BITS_PREFIX_WORDS).cast::<u8>(),
            data_words * size_of::<Eterm>(),
        );
        let nbytes = bit_len.div_ceil(8);
        data[..nbytes].copy_from_slice(&bytes[..nbytes]);
        data[nbytes..].fill(0);
    }
    Ok(make_boxed(p as *const Eterm))
}

/// The bit size of a bitstring (heap or sub bits).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a bitstring.
pub(crate) fn bitstring_bit_size(term: Eterm) -> Result<usize> {
    if !is_bitstring(term) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_bitstring`] confirmed `term` is a live heap- or sub-bitstring
    // whose backing thing is valid for `ERL_SUB_BITS_SIZE`/`ERL_HEAP_BITS_SIZE`
    // words — never unvalidated memory.
    Ok(unsafe { beam_term::bitstring_size(term) })
}

/// Copies the bytes of a byte-aligned bitstring (a *binary*) out of the term.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a byte-aligned bitstring. An off-heap
/// `SUB_BITS` thing is also rejected: Milestone-1 builds none (every binary this
/// crate creates is on-heap), and its bytes live behind a `BinRef`/match-context
/// indirection that no M1 producer creates.
#[allow(dead_code)]
pub(crate) fn binary_bytes(term: Eterm) -> Result<Vec<u8>> {
    if !is_binary(term) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_binary`] confirmed `term` is a boxed, byte-aligned bitstring, so
    // its header word is a live term word.
    let header = unsafe { *boxed_val(term) };
    if header.raw() & BITSTRING_TAG_MASK != HEAP_BITS_SUBTAG {
        return Err(BeamError::BadArg);
    }
    // SAFETY: a heap bitstring is `[header, size, data…]`, so word 2 begins the live
    // data area, which holds `ceil(byte_len / 8)` words — i.e. at least `byte_len`
    // initialised bytes (`erl_bits.h`: the data area is byte-addressed).
    let data = unsafe { boxed_val(term).add(HEAP_BITS_PREFIX_WORDS).cast::<u8>() };
    let nbytes = unsafe { beam_term::bitstring_size(term) } / 8;
    // SAFETY: the `nbytes` bytes just described are initialised and in bounds; the
    // copy goes out byte by byte, so it is endianness-agnostic.
    Ok(unsafe { core::slice::from_raw_parts(data, nbytes) }.to_vec())
}

/// Copies the bit data of **any** bitstring (heap or sub, byte-aligned or not) out
/// of the term.
///
/// Returns `ceil(bit_len / 8)` bytes, packed **MSB-first**: bit `i` of the
/// bitstring (`0 <= i < bit_len`, counted from the most-significant bit of the
/// first byte) is bit `7 - (i % 8)` of byte `i / 8`, exactly the packing
/// [`make_heap_bitstring`] writes and `erl_bits.h` addresses. The bit length is
/// [`bitstring_bit_size`]. This is the read half for the `<<X:L>>` render form
/// (`crate::print`), which splits the bytes into the whole leading bytes plus the
/// trailing `L = bit_len % 8` bits.
///
/// Unlike [`binary_bytes`], this accepts a **non-byte-aligned** bitstring (and an
/// off-heap `SUB_BITS` thing): it reads bit by bit through the frozen
/// `beam_term::bit_at` accessor, so it never follows a `BinRef`/match-context
/// indirection and never touches unvalidated memory.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a bitstring.
pub(crate) fn bitstring_bits(term: Eterm) -> Result<Vec<u8>> {
    let bit_len = bitstring_bit_size(term)?;
    let mut out = vec![0u8; bit_len.div_ceil(8)];
    for i in 0..bit_len {
        // SAFETY: [`bitstring_bit_size`] confirmed `term` is a live bitstring and
        // `i < bit_len` keeps bit `i` within its size, so `bit_at` reads an
        // initialised bit of the (heap- or sub-)bitstring's storage.
        if unsafe { beam_term::bit_at(term, i) } {
            out[i / 8] |= 1u8 << (7 - (i % 8));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Flat maps (`beam_term::thing` owns the layout)
//
// Reserved for the `maps` module BIFs, which land later; every item is exercised by
// the tests below until then (hence the `#[allow(dead_code)]`).
// ---------------------------------------------------------------------------

/// `is_flatmap(x)` — a flat map (a small map), as opposed to a hashmap.
#[allow(dead_code)]
#[inline]
pub(crate) fn is_flatmap(x: Eterm) -> bool {
    // SAFETY: `x` is a live machine word; `is_flatmap` checks `is_boxed` before
    // dereferencing the header.
    unsafe { beam_term::is_flatmap(x) }
}

/// `is_hashmap(x)`.
// A documented part of the `access.rs` surface reserved for dependents: M1 builds no
// hashmaps (every M1 map is a flat map), so no BIF dispatches on it yet.
#[allow(dead_code)]
#[inline]
pub(crate) fn is_hashmap(x: Eterm) -> bool {
    // SAFETY: as [`is_flatmap`].
    unsafe { beam_term::is_hashmap(x) }
}

/// Builds a flat map from `(key, value)` pairs on `heap`.
///
/// The layout is owned by `beam_term::thing` (`write_flatmap`); the pair order is
/// preserved and duplicate keys are not detected (that needs term comparison, owned
/// by `beam_term::compare`).
///
/// # Errors
///
/// Propagates [`BeamError::Alloc`](beam_common::BeamError::Alloc) when `heap` cannot
/// fit the block.
#[allow(dead_code)]
pub(crate) fn make_flatmap(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let words = beam_term::thing::flatmap_size_words(pairs.len());
    // SAFETY: `alloc_words` hands out `words` exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(words)? };
    // SAFETY: `p` is the base of those `words` words; `write_flatmap` writes all of
    // them and returns a boxed pointer into the same block, which stays live (and
    // unmoved) for as long as `heap` does, which the caller keeps alive.
    Ok(unsafe { beam_term::thing::write_flatmap(p, pairs) })
}

/// Copies a flat map's keys and values out of the term (each in wire order).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a flat map.
#[allow(dead_code)]
pub(crate) fn flatmap_pairs(term: Eterm) -> Result<(Vec<Eterm>, Vec<Eterm>)> {
    if !is_flatmap(term) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_flatmap`] confirmed `term` is a live flat map in the
    // `write_flatmap` layout, so both borrowed slices are live and in bounds; they
    // are copied below, so nothing borrows past this call (the caller's heap stays
    // alive regardless).
    let (keys, values) = unsafe { beam_term::thing::flatmap_parts(term) };
    Ok((keys.to_vec(), values.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{is_boxed, make_small, NIL};

    fn heap() -> Heap {
        Heap::new(32).expect("test heap")
    }

    #[test]
    fn float_round_trips_through_its_bits() {
        let mut heap = heap();
        for value in [0.0f64, 1.0, -2.5, 1.5, f64::MAX, f64::MIN_POSITIVE] {
            let term = make_float(&mut heap, value).expect("the float fits");
            assert!(is_float(term), "not a float: {value}");
            assert_eq!(float_bits(term), value.to_bits());
            assert_eq!(float_value(term), value);
            assert!(is_boxed(term));
        }
        assert!(!is_float(make_small(7)));
    }

    #[test]
    fn tuple_size_and_elements() {
        let mut heap = heap();
        let tuple = heap
            .tuple(&[make_small(10), make_small(20), make_small(30)])
            .expect("the tuple fits");
        assert!(is_tuple(tuple));
        assert_eq!(tuple_size(tuple), 3);
        assert_eq!(tuple_element(tuple, 0), make_small(10));
        assert_eq!(tuple_element(tuple, 2), make_small(30));
        assert!(!is_tuple(make_small(1)));
    }

    #[test]
    fn list_car_and_cdr() {
        let mut heap = heap();
        let list = heap.cons(make_small(1), NIL).expect("the cons fits");
        assert!(is_list(list));
        assert_eq!(car(list), make_small(1));
        assert_eq!(cdr(list), NIL);
    }

    #[test]
    fn bitstring_bits_re_packs_any_bitstring_msb_first() {
        let mut heap = heap();
        // `<<1:1>>` — one bit, in the MSB of the single data byte.
        let one = make_heap_bitstring(&mut heap, &[0b1000_0000], 1).expect("fits");
        assert_eq!(bitstring_bits(one).unwrap(), vec![0b1000_0000]);
        // `<<5:3,3:2>>` = `10111` ⇒ the canonical MSB-first re-packing.
        let five = make_heap_bitstring(&mut heap, &[0b1011_1000], 5).expect("fits");
        assert_eq!(bitstring_bits(five).unwrap(), vec![0b1011_1000]);
        // `<<1:9>>` — a whole leading byte then the trailing bit.
        let nine = make_heap_bitstring(&mut heap, &[0x00, 0b1000_0000], 9).expect("fits");
        assert_eq!(bitstring_bits(nine).unwrap(), vec![0x00, 0b1000_0000]);
        // A byte-aligned binary re-packs to exactly its own bytes.
        let bin = make_heap_bitstring(&mut heap, &[0xAB, 0xCD], 16).expect("fits");
        assert_eq!(bitstring_bits(bin).unwrap(), vec![0xAB, 0xCD]);
        // An empty binary yields no bytes.
        let empty = make_heap_bitstring(&mut heap, &[], 0).expect("fits");
        assert_eq!(bitstring_bits(empty).unwrap(), Vec::<u8>::new());
        // A non-bitstring is rejected.
        for term in [make_small(7), NIL] {
            assert!(matches!(bitstring_bits(term), Err(BeamError::BadArg)));
        }
    }

    #[test]
    fn ref_thing_is_a_boxed_internal_ref() {
        let mut heap = heap();
        let thing = make_ref_thing(&mut heap, 42).expect("the ref fits");
        assert!(is_boxed(thing));
        assert!(is_internal_ref(thing));
    }

    #[test]
    fn ref_thing_rejects_an_id_beyond_the_small_range() {
        let mut heap = heap();
        assert!(matches!(
            make_ref_thing(&mut heap, u64::MAX),
            Err(BeamError::SystemLimit)
        ));
        assert_eq!(heap.used(), 0, "a rejected id allocates nothing");
    }

    #[test]
    fn composed_predicates() {
        assert!(is_pid(ProcessId::new(1, 0).to_eterm()));
        assert!(is_port(beam_term::make_internal_port(
            beam_term::make_port_data(3)
        )));
        assert!(!is_pid(make_small(1)));
        assert!(!is_port(NIL));
        assert!(!is_reference(NIL));
        assert!(is_number(make_small(1)));
        assert!(is_integer(make_small(1)));
        assert!(is_atom(beam_term::make_atom(beam_term::Atom::from_raw(0))));
    }

    #[test]
    fn heap_bitstring_has_the_frozen_header_and_layout() {
        let mut heap = Heap::new(64).expect("test heap");
        let bin = make_heap_bitstring(&mut heap, &[0xAB, 0xCD], 16).expect("the binary fits");
        assert!(is_boxed(bin));
        assert!(is_bitstring(bin));
        assert!(is_binary(bin));
        assert_eq!(bitstring_bit_size(bin).unwrap(), 16);
        // SAFETY: `bin` is the live heap bitstring built above; word 0 is its header,
        // word 1 the bit size and words 2.. its data (all written by
        // `make_heap_bitstring`).
        unsafe {
            assert_eq!(*boxed_val(bin), make_header(2, HEAP_BITS_SUBTAG));
            assert_eq!((*boxed_val(bin).add(1)).raw(), 16);
            // The bytes are stored in address order (`erl_bits.h`: the data area is
            // byte-addressed), so this is endianness-agnostic.
            assert_eq!(*(boxed_val(bin).add(2).cast::<u8>()), 0xAB);
            assert_eq!(*(boxed_val(bin).add(2).cast::<u8>().add(1)), 0xCD);
            // The unused bytes of the final data word are zeroed.
            assert_eq!(*(boxed_val(bin).add(2).cast::<u8>().add(2)), 0x00);
            assert_eq!(*(boxed_val(bin).add(2).cast::<u8>().add(7)), 0x00);
        }
        assert_eq!(binary_bytes(bin).unwrap(), vec![0xAB, 0xCD]);
    }

    #[test]
    fn heap_bitstring_sizes_follow_the_frozen_word_count() {
        let mut heap = Heap::new(64).expect("test heap");
        // `erl_bits.h:heap_bits_size`: 2 + ceil(bits / 64) words, header arity
        // `1 + ceil(bits / 64)`.
        for (nbytes, words, arity) in [(0usize, 2usize, 1usize), (2, 3, 2), (8, 3, 2), (64, 10, 9)] {
            let before = heap.used();
            let bin =
                make_heap_bitstring(&mut heap, &vec![0u8; nbytes], nbytes * 8).expect("fits");
            assert_eq!(heap.used() - before, words, "{nbytes}-byte binary word count");
            // SAFETY: `bin` is a live heap bitstring; word 0 is its header.
            let header = unsafe { *boxed_val(bin) };
            assert_eq!(header, make_header(arity, HEAP_BITS_SUBTAG));
        }
    }

    #[test]
    fn empty_flatmap_round_trips() {
        let mut heap = Heap::new(64).expect("test heap");
        let map = make_flatmap(&mut heap, &[]).expect("the empty map fits");
        assert!(is_flatmap(map));
        assert!(!is_hashmap(map));
        let (keys, values) = flatmap_pairs(map).unwrap();
        assert!(keys.is_empty());
        assert!(values.is_empty());
        // The empty block is the three-word `[head, 0, tuple0]` form.
        assert_eq!(heap.used(), 3);
    }

    #[test]
    fn flatmaps_round_trip_in_wire_order() {
        let mut heap = Heap::new(256).expect("test heap");
        for n in [1usize, 2, 32] {
            let pairs: Vec<(Eterm, Eterm)> = (0..n)
                .map(|i| (make_small(i as i64), make_small(-(i as i64))))
                .collect();
            let map = make_flatmap(&mut heap, &pairs).expect("the map fits");
            assert!(is_flatmap(map));
            let (keys, values) = flatmap_pairs(map).unwrap();
            assert_eq!(keys.len(), n);
            assert_eq!(values.len(), n);
            for (i, (key, value)) in pairs.iter().enumerate() {
                assert_eq!(keys[i], *key, "key {i}");
                assert_eq!(values[i], *value, "value {i}");
            }
        }
    }

    #[test]
    fn flatmap_accessors_reject_non_maps() {
        let mut heap = Heap::new(64).expect("test heap");
        let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
        for term in [make_small(1), NIL, tuple] {
            assert!(!is_flatmap(term));
            assert!(!is_hashmap(term));
            assert!(matches!(flatmap_pairs(term), Err(BeamError::BadArg)));
        }
    }
}